#!/usr/bin/env python3
"""Bounded real-VM M4 observations. Never an automatic qualification gate.

Root orchestrates private namespaces; servers, VMs and fixtures run as aenv.
Artifacts are retained. Capacity skips never lower concurrency or repetitions.
"""
import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import itertools
import json
import math
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import threading
import time
import uuid

from dragonball_m4_lab import Lab
from dragonball_m4_build import verify as verify_build
from dragonball_m4_cleanup import cleanup_owned_ublk
from dragonball_m4_metrics import resources
from dragonball_m4_rollback import digest
from dragonball_m4_scenarios import (
    Services, api, fixtures, guest_command, guest_file, isolated_controller,
    start_server, terminate_controller, validate_inputs,
)

GIB = 1024 ** 3
OPERATIONS = ("cold", "capture", "restore", "fork_1", "fork_n", "io", "pause_resume", "template")


def workload(memory_mib, dirty):
    if memory_mib not in (1024, 2048, 8192) or dirty not in ("low", "high"):
        raise ValueError("expected 1/2/8 GiB and low/high dirty workload")
    resident = memory_mib * 1024 ** 2 // 2
    writes = resident if dirty == "high" else resident // 50 // 4096 * 4096
    return {"resident_bytes": resident, "resident_fraction": 0.5,
            "dirty_bytes_per_sweep": writes, "dirty_fraction_of_guest_ram": writes / (resident * 2),
            "sweep_interval_ms": 200, "pattern": "1 + byte_offset % 251",
            "requested_write_bytes_per_second_upper_bound": writes * 5}


def capacity(cell, repeats, node_budget, available_memory, free_disk, operations, fixture_budget=2 * GIB):
    workload(cell["memory_mib"], cell["dirty"])
    if cell["concurrency"] not in (1, 10, 100) or not 1 <= repeats <= 100:
        raise ValueError("expected concurrency 1/10/100 and 1..100 repeats")
    if node_budget < 4 * GIB or node_budget > 512 * GIB or set(operations) - set(OPERATIONS):
        raise ValueError("invalid node budget or operations")
    if not 2 * GIB <= fixture_budget <= 128 * GIB:
        raise ValueError("fixture memory budget must be 2..128 GiB")
    count, memory = cell["concurrency"], cell["memory_mib"] * 1024 ** 2
    doubles = bool(set(operations) & {"restore", "fork_1", "fork_n", "template"})
    peak = count * (2 if doubles else 1) * (memory + 256 * 1024 ** 2) + 2 * GIB
    # Uncompressed full-memory upper bounds; backend artifacts and old labs survive.
    captures = sum(op in operations for op in ("capture", "fork_1", "fork_n", "pause_resume", "template"))
    if "restore" in operations and "capture" not in operations:
        captures += 1
    # Publication can temporarily retain both the writable source and upload data.
    disk = repeats * count * memory * captures + (2 * count * memory if captures else 0)
    disk += 2 * GIB + count * 128 * 1024 ** 2
    reasons = []
    if peak > node_budget:
        reasons.append("node_memory")
    if node_budget + fixture_budget + 10 * GIB > available_memory:
        reasons.append("host_memory_reserve")
    if disk + 8 * GIB > free_disk:
        reasons.append("retained_disk")
    return {"feasible": not reasons, "reasons": reasons, "required_node_bytes": peak,
            "fixture_memory_bytes": fixture_budget,
            "estimated_retained_bytes": disk, "host_memory_reserve_bytes": 10 * GIB,
            "disk_reserve_bytes": 8 * GIB}


def summarize(samples):
    groups = {}
    for sample in samples:
        required = ("backend", "operation", "memory_mib", "dirty", "concurrency", "ok", "seconds")
        if any(name not in sample for name in required):
            raise ValueError("incomplete measurement")
        seconds = sample["seconds"]
        if (type(seconds) not in (int, float) or not math.isfinite(seconds) or seconds < 0 or
                type(sample["ok"]) is not bool):
            raise ValueError("invalid measurement")
        key = "/".join(str(sample[name]) for name in required[:5])
        groups.setdefault(key, []).append(sample)
    result = {}
    for key, rows in groups.items():
        values = sorted(row["seconds"] for row in rows if row["ok"])
        result[key] = {"count": len(rows), "successes": len(values),
                       "failure_rate": (len(rows) - len(values)) / len(rows)}
        for label, fraction, minimum in (("p50", .5, 1), ("p95", .95, 20), ("p99", .99, 100)):
            result[key][label + "_seconds"] = values[math.ceil(fraction * len(values)) - 1] if len(values) >= minimum else None
    comparisons = {}
    for key in sorted({key.split("/", 1)[1] for key in groups}):
        fc, db = result.get("firecracker/" + key, {}), result.get("dragonball/" + key, {})
        fc95, db95 = fc.get("p95_seconds"), db.get("p95_seconds")
        comparison = {"status": "insufficient_successes"}
        if fc95 is not None and db95 is not None and fc95 > 0:
            ratio = db95 / fc95
            comparison = {"dragonball_over_firecracker_p95": ratio,
                          "status": "within_10_percent" if ratio <= 1.1 else "exceeds_10_percent"}
            if fc.get("failure_rate") or db.get("failure_rate"):
                comparison["status"] = "failed_samples_prevent_acceptance"
        comparisons[key] = comparison
    return {"groups": result, "relative_diagnostics": comparisons, "performance_qualified": False,
            "relative_engineering_gate_percent": 10,
            "limitations": ["capture_pause_and_peak_slo_not_frozen", "capture_api_is_not_guest_pause",
                            "object_store_read_upload_bytes_unmeasured", "shared_host_page_cache",
                            "percentiles_are_diagnostic_not_slo_acceptance"]}


def parallel_actions(count, action):
    barrier = threading.Barrier(count)
    def execute(index):
        barrier.wait(timeout=30)
        started = time.monotonic()
        try:
            value = action(index)
            return {"ok": True, "seconds": time.monotonic() - started, "value": value}
        except Exception as error:
            return {"ok": False, "seconds": time.monotonic() - started, "error": str(error)}
    with ThreadPoolExecutor(max_workers=count) as executor:
        return list(executor.map(execute, range(count)))


def append_json(path, value):
    with path.open("a") as output:
        output.write(json.dumps(value, allow_nan=False) + "\n")
        output.flush()
        os.fsync(output.fileno())


def verify_guest(node, sandbox, expected):
    token = uuid.uuid4().hex
    guest_file(node, sandbox, "/var/tmp/m4-load/request", token)
    result = guest_command(node, sandbox,
        "i=0; while [ $i -lt 90 ]; do "
        f"if /agentenv/bin/busybox grep -q '{token}' /var/tmp/m4-load/response 2>/dev/null; then "
        "cat /var/tmp/m4-load/response; exit 0; fi; i=$((i+1)); sleep 1; done; exit 1")
    proof = json.loads(result["stdout"])
    size = expected["resident_bytes"]
    cycles, remainder = divmod(size, 251)
    checksum = cycles * (251 * 252 // 2) + remainder * (remainder + 1) // 2
    if (proof.get("token") != token or proof.get("valid") is not True or
            proof.get("resident_bytes") != size or proof.get("checksum") != checksum or
            proof.get("dirty_bytes_per_sweep") != expected["dirty_bytes_per_sweep"] or
            proof.get("written_bytes", 0) < expected["dirty_bytes_per_sweep"] or
            not math.isfinite(proof.get("elapsed_seconds", float("nan"))) or
            proof["elapsed_seconds"] <= 0):
        raise RuntimeError("guest anonymous memory verification failed: " + str(proof))
    proof["observed_write_bytes_per_second_since_start"] = proof["written_bytes"] / proof["elapsed_seconds"]
    return proof


def start_workload(node, sandbox, encoded_binary, spec):
    guest_file(node, sandbox, "/var/tmp/m4-workload.b64", encoded_binary)
    guest_command(node, sandbox,
        "mkdir -p /var/tmp/m4-load; /agentenv/bin/busybox base64 -d /var/tmp/m4-workload.b64 "
        "> /var/tmp/m4-workload; chmod 755 /var/tmp/m4-workload; "
        f"/agentenv/bin/busybox nohup /var/tmp/m4-workload {spec['resident_bytes']} "
        f"{spec['dirty_bytes_per_sweep']} {spec['sweep_interval_ms']} /var/tmp/m4-load "
        ">/var/tmp/m4-workload.log 2>&1 </dev/null &")
    return verify_guest(node, sandbox, spec)


def build_template(node, image, memory_mib):
    created = api(node, "/v3/templates", "POST",
                  {"name": "m4-perf-" + uuid.uuid4().hex, "cpuCount": 1, "memoryMB": memory_mib}, 202)
    path = f"/templates/{created['templateID']}/builds/{created['buildID']}"
    api(node, "/v2" + path, "POST", {"fromImage": image, "force": True, "steps": []}, 202)
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        status = api(node, path + "/status")
        if status["status"] == "ready":
            return {**created, "status": status, "workload": "empty_steps_from_common_image"}
        if status["status"] == "error":
            raise RuntimeError("template build failed: " + str(status))
        time.sleep(0.5)
    raise TimeoutError("template build did not finish in 600 seconds")


def mem_available():
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) * 1024
    raise RuntimeError("MemAvailable missing")


def cleanup_cell(lab, services, inputs):
    errors = []
    fixture_resources = {}
    try:
        fixture_resources = {
            "memory_max_bytes": int((services.group / "memory.max").read_text()),
            "memory_peak_bytes": int((services.group / "memory.peak").read_text()),
            "memory_events": {key: int(value) for key, value in
                              (line.split() for line in (services.group / "memory.events").read_text().splitlines())}}
        (lab.root / "fixture-resources.json").write_text(json.dumps(fixture_resources, indent=2))
        if any(fixture_resources["memory_events"].get(key, 0) for key in ("oom", "oom_kill", "oom_group_kill")):
            errors.append("fixture cgroup OOM: " + str(fixture_resources["memory_events"]))
    except Exception as error:
        errors.append("fixture resource record: " + str(error))
    for node in reversed(lab.nodes):
        try:
            node.close()
        except Exception as error:
            errors.append("node close: " + str(error))
        try:
            cleanup_owned_ublk(node, inputs)
        except Exception as error:
            errors.append("owned ublk cleanup: " + str(error))
    try:
        services.close()
    except Exception as error:
        errors.append("services close: " + str(error))
    if errors:
        raise RuntimeError("; ".join(errors))
    return fixture_resources



def preflight(inputs):
    """Shared admission for CLI and direct experiment callers; allocates no lab."""
    validate_inputs(inputs, "rollback", os.geteuid())
    for name in ("workload", "ublk_cli"):
        path = Path(inputs.get(name, ""))
        if not path.is_absolute() or not path.is_file():
            raise ValueError("missing absolute input: " + name)
    assets = json.loads(Path(inputs["assets"]).read_text())
    if digest(assets["kernel_path"]) != digest(inputs["kernel_fc"]):
        raise ValueError("comparative matrix requires the same kernel bytes for both backends")
    builds = {}
    for backend, key in (("dragonball", "server"), ("firecracker", "server_fc")):
        builds[backend] = verify_build(Path(inputs.get(key + "_receipt", "")), inputs[key], backend)
    return {"assets": assets, "builds": builds}


def comparable_builds(results):
    if any("error" in row for row in results):
        return False
    rows = [row for row in results if "identity" in row]
    if {row["backend"] for row in rows} != {"firecracker", "dragonball"}:
        return False
    signatures = set()
    for row in rows:
        receipt = row.get("build_receipt")
        if not receipt:
            return False
        build = receipt["build"]
        signatures.add(json.dumps([receipt["source"], build["profile"], build["toolchains"],
                                   build["environment_sha256"], build["configurations"]], sort_keys=True))
    return len(signatures) == 1


def run_cell(root, inputs, backend, cell, args, output):
    checked = preflight(inputs)
    receipt = checked["builds"][backend]
    samples = []
    spec = workload(cell["memory_mib"], cell["dirty"])
    encoded = base64.b64encode(Path(inputs["workload"]).read_bytes()).decode()
    result = {"root": str(root), "backend": backend, **cell, "workload": spec,
              "oss_timeout_seconds": inputs.get("oss_timeout_seconds", 30),
              "request_timeout_seconds": inputs.get("request_timeout_seconds", 180),
              "repeats_requested": args.repeats, "samples": samples, "cleanup": "pending",
              "build_receipt": receipt,
              "fixture_resources_path": str(root / "fixture-resources.json")}
    try:
        with Lab(root, "aenv") as lab:
            services = Services(lab, memory_gib=args.fixture_memory_gib)
            try:
                cert, image, assets = fixtures(lab, inputs, services)
                node = lab.start_node("a")
                # Only this disposable node is changed, before starting its server.
                (node.cgroup / "memory.max").write_text(str(args.node_memory_gib * GIB))
                (node.cgroup / "cpu.max").write_text(f"{args.node_cpus * 100000} 100000")
                (node.cgroup / "pids.max").write_text(str(max(2048, cell["concurrency"] * 128)))
                result["identity"] = start_server(node, inputs, cert, assets, backend)
                if any(result["identity"].get(key) != value for key, value in receipt["identity"].items()):
                    raise ValueError("running server identity differs from build receipt")
                result["config_sha256"] = digest(node.root / "config.toml")
                result["namespaces"] = json.loads((node.root / "ready").read_text())
                count = cell["concurrency"]

                def batch(operation, action, request_count=count, **details):
                    # Re-check actual disk/memory before every operation. No global cache flush.
                    if shutil.disk_usage(root).free < 8 * GIB + count * cell["memory_mib"] * 1024 ** 2:
                        raise RuntimeError("disk reserve reached; remaining operations not attempted")
                    if mem_available() < 10 * GIB:
                        raise RuntimeError("host memory reserve reached; remaining operations not attempted")
                    before = resources(node)
                    rows = parallel_actions(request_count, action)
                    after = resources(node)
                    for index, row in enumerate(rows):
                        sample = {**cell, "backend": backend, "operation": operation, "repeat": repeat,
                                  "request_index": index, "request_concurrency": request_count,
                                  "cache_phase": "first_guest" if repeat == 0 else "subsequent_guest",
                                  "ok": row["ok"], "seconds": row["seconds"],
                                  "resources_before_batch": before, "resources_after_batch": after,
                                  "resource_scope": "whole_node_batch_cumulative_memory_peak", **details}
                        if "error" in row:
                            sample["error"] = row["error"]
                        samples.append(sample)
                        append_json(root / "samples.jsonl", sample)
                        append_json(output / "samples.jsonl", sample)
                    if not all(row["ok"] for row in rows):
                        raise RuntimeError(f"{operation}: batch contains failed samples")
                    return [row["value"] for row in rows]

                def remove(guests):
                    for guest in guests:
                        api(node, f"/sandboxes/{guest['sandboxID']}", "DELETE", expected=204)

                def verify(guests, phase):
                    proofs = batch(phase + "_memory_verify", lambda i: verify_guest(node, guests[i], spec), len(guests))
                    append_json(root / "workload-checks.jsonl", {"repeat": repeat, "phase": phase, "proofs": proofs})

                for repeat in range(args.repeats):
                    def create(_):
                        guest = api(node, "/sandboxes-cold", "POST", {"image": image, "cpuCount": 1,
                            "memoryMB": cell["memory_mib"], "timeout": 7200, "autoPause": False, "secure": False}, 201)
                        guest_command(node, guest, "true")
                        return guest
                    guests = batch("cold_api_to_envd_ready", create, workload_phase="before_guest_workload")
                    try:
                        if set(args.operations) == {"cold"}:
                            continue
                        ready = batch("workload_initialize", lambda i: start_workload(node, guests[i], encoded, spec))
                        append_json(root / "workload-checks.jsonl", {"repeat": repeat, "phase": "start", "proofs": ready})
                        # Allow multiple dirty sweeps before measuring capture-related operations.
                        time.sleep(1)
                        if set(args.operations) & {"capture", "restore"}:
                            snapshots = batch("capture_api", lambda i: api(node,
                                f"/sandboxes/{guests[i]['sandboxID']}/snapshots", "POST",
                                {"name": "m4-perf-" + uuid.uuid4().hex}, 201))
                            verify(guests, "capture_source")
                            if "restore" in args.operations:
                                def restore(i):
                                    guest = api(node, "/sandboxes", "POST", {"templateID": snapshots[i]["snapshotID"],
                                        "timeout": 7200, "autoPause": False, "secure": False}, 201)
                                    guest_command(node, guest, "true")
                                    return guest
                                children = batch("restore_api_to_envd_ready", restore)
                                verify(children, "restore")
                                remove(children)
                        for operation in ("fork_1", "fork_n"):
                            if operation not in args.operations:
                                continue
                            child_count = 1 if operation == "fork_1" else count
                            requests = count if operation == "fork_1" else 1
                            def fork(i):
                                entries = api(node, f"/sandboxes/{guests[i]['sandboxID']}/fork", "POST",
                                              {"count": child_count, "timeout": 7200}, 201)
                                if len(entries) != child_count or any(not e.get("sandbox") or e.get("error") for e in entries):
                                    raise RuntimeError("partial fork failure: " + str(entries))
                                children = [e["sandbox"] for e in entries]
                                for child in children:
                                    guest_command(node, child, "true")
                                return children
                            batches = batch(operation + "_api_to_envd_ready", fork, requests, children_per_request=child_count)
                            children = [child for children in batches for child in children]
                            verify(children, operation)
                            remove(children)
                        if "io" in args.operations:
                            outputs = batch("guest_16mib_random_write_fsync_rpc", lambda i: guest_command(node, guests[i],
                                "/agentenv/bin/busybox dd if=/dev/urandom of=/var/tmp/m4-io bs=1048576 count=16 conv=fsync"))
                            append_json(root / "io-results.jsonl", {"repeat": repeat, "results": outputs})
                        if "pause_resume" in args.operations:
                            batch("pause_api", lambda i: api(node, f"/sandboxes/{guests[i]['sandboxID']}/pause", "POST", expected=204))
                            def resume(i):
                                resumed = api(node, f"/sandboxes/{guests[i]['sandboxID']}/resume", "POST", {"timeout": 7200}, 201)
                                if resumed["sandboxID"] != guests[i]["sandboxID"]:
                                    raise RuntimeError("resume changed sandbox ID")
                                guest_command(node, resumed, "true")
                                return resumed
                            guests = batch("resume_api_to_envd_ready", resume)
                            verify(guests, "resume")
                        if "template" in args.operations:
                            templates = batch("template_empty_build_ready", lambda i: build_template(node, image, cell["memory_mib"]))
                            append_json(root / "template-results.jsonl", {"repeat": repeat, "results": templates})
                    finally:
                        # Lab teardown also reclaims VMs created by partially failed API batches.
                        remove(guests)
            finally:
                result["fixture_resources"] = cleanup_cell(lab, services, inputs)
        result["cleanup"] = "passed"
    except Exception as error:
        result["error"] = str(error)
        result["cleanup"] = "check_retained_logs_and_cgroups"
    result["measurements"] = summarize(samples)
    (root / "result.json").write_text(json.dumps(result, indent=2, allow_nan=False))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True, help="M4 inputs plus static Linux x86_64 workload binary")
    parser.add_argument("--memory-mib", nargs="+", type=int, choices=(1024, 2048, 8192), default=[1024, 2048, 8192])
    parser.add_argument("--dirty", nargs="+", choices=("low", "high"), default=["low", "high"])
    parser.add_argument("--concurrency", nargs="+", type=int, choices=(1, 10, 100), default=[1, 10, 100])
    parser.add_argument("--operations", nargs="+", choices=OPERATIONS, default=list(OPERATIONS))
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--node-memory-gib", type=int, default=12)
    parser.add_argument("--fixture-memory-gib", type=int, default=2,
                        help="bounded S3/registry fixture memory including page cache (2..128 GiB)")
    parser.add_argument("--node-cpus", type=int, default=4)
    parser.add_argument("--plan-only", action="store_true", help="capacity plan only, no lab or VM")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.version_info < (3, 11):
        parser.error("M4 runner requires Python >= 3.11")
    if not 2 <= args.fixture_memory_gib <= 128:
        parser.error("fixture memory budget must be 2..128 GiB")
    inputs = json.loads(args.inputs.read_text())
    checked = preflight(inputs)
    assets = checked["assets"]
    if not 1 <= args.node_cpus <= (os.cpu_count() or 1):
        parser.error("node CPU budget must fit host logical CPUs")
    cells = [{"memory_mib": m, "dirty": d, "concurrency": c}
             for m, d, c in itertools.product(args.memory_mib, args.dirty, args.concurrency)]
    for value in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(value, terminate_controller)
    if not args.inside and not args.plan_only:
        env = {**os.environ, "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net")}
        return isolated_controller(["unshare", "--mount", "--net", "--propagation", "private",
                                    sys.executable, str(Path(__file__).resolve()), *sys.argv[1:], "--inside"], env)
    output = Path(tempfile.mkdtemp(prefix="db-m4-performance.", dir="/var/tmp"))
    conditions = {"host": os.uname()._asdict() if hasattr(os.uname(), "_asdict") else list(os.uname()),
        "host_cpuinfo": Path("/proc/cpuinfo").read_text(), "operations": args.operations,
        "repeats": args.repeats, "node_memory_gib": args.node_memory_gib, "node_cpu_quota": args.node_cpus,
        "fixture_memory_gib": args.fixture_memory_gib, "build_receipts": checked["builds"],
        "vcpu_per_sandbox": 1, "cache_policy": "fresh_node_caches_shared_host_cache_no_flush",
        "backend_order": "alternating_by_cell_firecracker_first_even_dragonball_first_odd",
        "pool": "disabled", "storage": "overlaybd_ublk_oss_local_minio",
        "compression": "ordinary_publish_disabled_template_uses_identical_server_defaults",
        "kernel_sha256": digest(assets["kernel_path"]), "tools_sha256": digest(assets["tools_path"]),
        "rootfs_config_sha256": digest(assets["image_path"]),
        "rootfs_layers": json.loads(Path(assets["image_path"]).read_text())["lowers"],
        "binaries_sha256": {name: digest(inputs[name]) for name in
            ("server", "server_fc", "daemon", "firecracker", "workload", "s3", "registry")},
        "dragonball_runner_sha256": digest(assets["runner_path"])}
    (output / "conditions.json").write_text(json.dumps(conditions, indent=2))
    print("M4 performance output: " + str(output), flush=True)
    results = []
    for cell_index, cell in enumerate(cells):
        backends = ("firecracker", "dragonball") if cell_index % 2 == 0 else ("dragonball", "firecracker")
        # Reserve both retained backend outputs up front; no lab artifacts are deleted.
        free_disk = shutil.disk_usage(output).free
        admission = capacity(cell, args.repeats, args.node_memory_gib * GIB,
                             mem_available(), (free_disk - 8 * GIB) // 2 + 8 * GIB, args.operations,
                             args.fixture_memory_gib * GIB)
        if not admission["feasible"] or args.plan_only:
            row = {**cell, "status": "planned" if admission["feasible"] else "capacity_skipped", "capacity": admission}
            results.append(row)
            append_json(output / "cells.jsonl", row)
            continue
        for backend in backends:
            admission = capacity(cell, args.repeats, args.node_memory_gib * GIB,
                                 mem_available(), shutil.disk_usage(output).free, args.operations,
                                 args.fixture_memory_gib * GIB)
            if not admission["feasible"]:
                row = {**cell, "backend": backend, "status": "capacity_skipped", "capacity": admission}
            else:
                root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
                print(f"M4 {backend} {cell}: {root}", flush=True)
                row = run_cell(root, inputs, backend, cell, args, output)
            results.append(row)
            append_json(output / "cells.jsonl", row)
    samples = [sample for row in results for sample in row.get("samples", [])]
    comparable = comparable_builds(results)
    report = {"conditions": conditions, "conditions_comparable": comparable,
              "cells": results, **summarize(samples)}
    if not comparable:
        for comparison in report["relative_diagnostics"].values():
            comparison["status"] = "missing_or_mismatched_build_provenance"
    (output / "result.json").write_text(json.dumps(report, indent=2, allow_nan=False))
    print(json.dumps({"output": str(output), "performance_qualified": False,
                      "failed_cells": sum("error" in row for row in results)}), flush=True)
    return int(any("error" in row for row in results))


if __name__ == "__main__":
    raise SystemExit(main())
