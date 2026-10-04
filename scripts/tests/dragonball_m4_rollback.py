"""Explicit test-only pool routing and serial FC/DB HTTP smoke measurements."""
import hashlib
import json
from pathlib import Path

from dragonball_m4_metrics import measure, summarize
from dragonball_m4_scenarios import (
    Services, api, fixtures, guest_command, guest_file, request, route_pool, start_server,
)


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run_rollback(lab, inputs):
    for name in ("server_fc", "firecracker", "kernel_fc"):
        if not Path(inputs[name]).is_absolute() or not Path(inputs[name]).is_file():
            raise ValueError("missing absolute input: " + name)
    services = Services(lab)
    samples = []
    try:
        cert, image, assets = fixtures(lab, inputs, services)
        nodes = {"dragonball": lab.start_node("a"), "firecracker": lab.start_node("b")}
        identities = {backend: start_server(node, inputs, cert, assets, backend) for backend, node in nodes.items()}
        assert identities["dragonball"]["git_commit"] == identities["firecracker"]["git_commit"]
        records, snapshots = {}, {}
        for backend in ("dragonball", "firecracker"):
            node = nodes[backend]
            def create():
                guest = api(node, "/sandboxes-cold", "POST", {"image": image, "cpuCount": 1,
                            "memoryMB": 1024, "timeout": 1800, "autoPause": False, "secure": False,
                            "envVars": {"M4_READY": "ready"}}, 201)
                guest_command(node, guest, 'test "$M4_READY" = ready')
                return guest
            source = measure(node, backend, "cold_create_envd_ready", samples, create)
            guest_file(node, source, "/dev/shm/m4-pool", backend)
            guest_file(node, source, "/var/tmp/m4-pool", backend)
            snapshot = measure(node, backend, "capture_api", samples,
                lambda: api(node, f'/sandboxes/{source["sandboxID"]}/snapshots', "POST", {"name": "m4-" + backend}, 201))
            snapshots[backend] = snapshot
            records[backend] = source
            for _ in range(3):
                def restore():
                    guest = api(node, "/sandboxes", "POST", {"templateID": snapshot["snapshotID"],
                                "timeout": 600, "autoPause": False, "secure": False}, 201)
                    assert guest_file(node, guest, "/dev/shm/m4-pool") == backend
                    return guest
                child = measure(node, backend, "restore_warm_node_envd_ready", samples, restore)
                api(node, f'/sandboxes/{child["sandboxID"]}', "DELETE", expected=204)
            for count in (1, 3):
                def fork():
                    children = api(node, f'/sandboxes/{source["sandboxID"]}/fork', "POST", {"count": count, "timeout": 600}, 201)
                    assert len(children) == count and all(c.get("sandbox") and not c.get("error") for c in children), children
                    for entry in children:
                        assert guest_file(node, entry["sandbox"], "/dev/shm/m4-pool") == backend
                    return children
                children = measure(node, backend, f"fork_{count}_envd_ready", samples, fork)
                for entry in children:
                    api(node, f'/sandboxes/{entry["sandbox"]["sandboxID"]}', "DELETE", expected=204)
            output = measure(node, backend, "guest_16mib_write_fsync_rpc", samples,
                lambda: guest_command(node, source, "/agentenv/bin/busybox time -f 'M4_IO_SECONDS=%e' /agentenv/bin/busybox dd if=/dev/zero of=/var/tmp/m4-io bs=1048576 count=16 conv=fsync"))
            (lab.root / (backend + "-guest-io.json")).write_text(json.dumps(output, indent=2))
            measure(node, backend, "pause_api", samples,
                lambda: api(node, f'/sandboxes/{source["sandboxID"]}/pause', "POST", expected=204))
            print("M4: serial lifecycle complete and paused: " + backend, flush=True)

        # Simulated rollout/rollback changes only the new-create entrypoint.
        new_pool = "firecracker"
        selected = route_pool("create", new_pool)
        rolled_back = api(nodes[selected], "/sandboxes", "POST", {
            "templateID": snapshots[selected]["snapshotID"], "timeout": 600, "secure": False}, 201)
        assert guest_file(nodes[selected], rolled_back, "/dev/shm/m4-pool") == "firecracker"
        api(nodes[selected], f'/sandboxes/{rolled_back["sandboxID"]}', "DELETE", expected=204)
        resumed_ids = {}
        for backend, original in records.items():
            selected = route_pool("resume", new_pool, backend)
            node = nodes[selected]
            resumed = measure(node, backend, "resume_same_id_envd_ready", samples,
                lambda: api(node, f'/sandboxes/{original["sandboxID"]}/resume', "POST", {"timeout": 600}, 201))
            assert resumed["sandboxID"] == original["sandboxID"]
            assert guest_file(node, resumed, "/dev/shm/m4-pool") == backend
            assert guest_file(node, resumed, "/var/tmp/m4-pool") == backend
            resumed_ids[backend] = resumed["sandboxID"]
        # Same repository but incompatible memory backend: explicit rejection.
        for backend, foreign in (("firecracker", "dragonball"), ("dragonball", "firecracker")):
            node = nodes[backend]
            before = {s["sandboxID"] for s in api(node, "/sandboxes")}
            request(f"http://{node.address}:8000/sandboxes", "POST", {
                "templateID": snapshots[foreign]["snapshotID"], "timeout": 600}, expected=400,
                timeout=node.request_timeout_seconds)
            assert {s["sandboxID"] for s in api(node, "/sandboxes")} == before
            assert any(s["snapshotID"] == snapshots[foreign]["snapshotID"] for s in api(node, "/snapshots"))
        for backend, sandbox_id in resumed_ids.items():
            api(nodes[backend], f"/sandboxes/{sandbox_id}", "DELETE", expected=204)
        conditions = {"kernel_sha256": {"dragonball": digest(assets["kernel_path"]), "firecracker": digest(inputs["kernel_fc"])},
                      "tools_sha256": digest(assets["tools_path"]), "image": image,
                      "server_sha256": {"dragonball": digest(inputs["server"]), "firecracker": digest(inputs["server_fc"])},
                      "runner_sha256": {"dragonball": digest(assets["runner_path"]), "firecracker": digest(inputs["firecracker"])},
                      "memory_mib": 1024, "vcpu": 1, "node_memory_limit_gib": 12, "cpu_quota": 4,
                      "pool": "disabled", "compression": "disabled", "startup_pack": "disabled",
                      "order": ["dragonball", "firecracker"], "host_page_cache": "shared_uncontrolled",
                      "skipped": ["10/100 concurrent: outside this bounded smoke",
                                  "2/8 GiB and high-dirty matrix: not run", "template build timing: not run"]}
        return {"rollback_new_create": "firecracker", "same_id_resume": resumed_ids,
                "oss_timeout_seconds": inputs.get("oss_timeout_seconds", 30),
                "request_timeout_seconds": inputs.get("request_timeout_seconds", 180),
                "foreign_backend_refused": True, "backend_identities": identities,
                "node_namespaces": {n.name: json.loads((n.root / "ready").read_text()) for n in nodes.values()},
                "measurements": summarize(samples, conditions), "scope": "single-host-simulated-pools"}
    finally:
        services.close()
