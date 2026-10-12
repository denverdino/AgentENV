#!/usr/bin/env python3
"""Real HTTP server cross-node exercise; no mocked VM or snapshot repository."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import ssl
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

from dragonball_m4_lab import Lab, command, empty_cgroup, wait_http
from dragonball_m4_cleanup import cleanup_owned_ublk

API_KEY = "m4-disposable-api-key-not-for-production"


def route_pool(operation, new_backend, existing_backend=None):
    backend = new_backend if operation == "create" else existing_backend
    if operation not in ("create", "resume") or backend not in ("firecracker", "dragonball"):
        raise ValueError("explicit existing backend required for resume")
    return backend


def process_output(body):
    result = {"stdout": "", "stderr": "", "exit_code": None}
    ended = False
    while body:
        if len(body) < 5:
            raise ValueError("truncated Connect envelope")
        flag, length = struct.unpack(">BI", body[:5])
        if flag not in (0, 2) or len(body) < length + 5 or ended:
            raise ValueError("invalid Connect envelope")
        message = json.loads(body[5:length + 5])
        body = body[length + 5:]
        if flag == 2:
            if message.get("error"):
                raise ValueError("Connect error: " + str(message["error"]))
            ended = True
            continue
        event = message.get("event", {})
        for key in ("stdout", "stderr"):
            if key in event.get("data", {}):
                result[key] += base64.b64decode(event["data"][key], validate=True).decode()
        if "end" in event:
            result["exit_code"] = event["end"].get("exitCode", 0)
    if not ended or result["exit_code"] is None:
        raise ValueError("process stream has no terminal exit")
    return result


def guest_command(node, sandbox, script, *, timeout=None):
    data = json.dumps({"process": {"cmd": "/bin/sh", "args": ["-ec", script]}, "stdin": False}).encode()
    headers = {"Content-Type": "application/connect+json", "Connect-Protocol-Version": "1",
               "x-agentenv-sandbox-id": sandbox["sandboxID"], "x-agentenv-target-port": "49983"}
    if sandbox.get("envdAccessToken"):
        headers["X-Access-Token"] = sandbox["envdAccessToken"]
    body, _ = request(f"http://{node.address}:8000/process.Process/Start", "POST",
                      struct.pack(">BI", 0, len(data)) + data, headers=headers,
                      timeout=getattr(node, "request_timeout_seconds", 180) if timeout is None else timeout)
    result = process_output(body)
    if result["exit_code"] != 0:
        raise RuntimeError("guest command failed: " + str(result))
    return result


def check_status(actual, expected, body):
    if actual != expected:
        raise RuntimeError(f"HTTP {actual}, expected {expected}: {body[:2000]!r}")


def validate_restore(result):
    assert result["source_id"] != result["child_id"], "restore must create a new ID"
    assert result["markers"] == {"ram": "m4-ram", "disk": "m4-disk", "volume": "m4-volume"}, result


def request(url, method="GET", payload=None, expected=200, headers=None, context=None, timeout=180):
    headers = {"X-API-Key": API_KEY, **(headers or {})}
    if isinstance(payload, dict):
        payload = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=payload, method=method, headers=headers)
    deadline = time.monotonic() + timeout
    try:
        response = urllib.request.urlopen(req, timeout=timeout, context=context)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        stream = response.fp if isinstance(response, urllib.error.HTTPError) else response
        # urllib's timeout is an idle socket timeout. Streaming keepalives must
        # not extend the response budget; read1 returns after one buffered read.
        sock = stream.fp.raw._sock
        chunks = []
        while not stream.isclosed():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("HTTP response deadline exceeded")
            sock.settimeout(remaining)
            chunk = stream.read1(64 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
        if time.monotonic() > deadline:
            raise TimeoutError("HTTP response deadline exceeded")
        body = b"".join(chunks)
        check_status(response.status, expected, body)
        return body, dict(response.headers)


def api(node, path, method="GET", payload=None, expected=200):
    body, _ = request(f"http://{node.address}:8000{path}", method, payload, expected,
                      timeout=getattr(node, "request_timeout_seconds", 180))
    return json.loads(body) if body else None


def guest_file(node, sandbox, path, content=None):
    headers = {"x-agentenv-sandbox-id": sandbox["sandboxID"], "x-agentenv-target-port": "49983"}
    if sandbox.get("envdAccessToken"):
        headers["X-Access-Token"] = sandbox["envdAccessToken"]
    url = f"http://{node.address}:8000/files?" + urllib.parse.urlencode({"path": path})
    payload = None
    if content is not None:
        boundary = "m4-test-file-boundary"
        headers["Content-Type"] = "multipart/form-data; boundary=" + boundary
        payload = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="marker"\r\n'
                   f'Content-Type: application/octet-stream\r\n\r\n{content}\r\n--{boundary}--\r\n').encode()
    data, _ = request(url, "POST" if content is not None else "GET", payload, headers=headers,
                      timeout=getattr(node, "request_timeout_seconds", 180))
    return data.decode()


class Services:
    """Own only fixture processes in a separate bounded cgroup."""
    def __init__(self, lab, memory_gib=2):
        if not 2 <= memory_gib <= 128:
            raise ValueError("fixture memory budget must be 2..128 GiB")
        self.lab = lab
        self.group = lab.cgroup / "services"
        self.group.mkdir()
        try:
            (self.group / "memory.max").write_text(str(memory_gib * 1024**3))
            (self.group / "pids.max").write_text("1024")
        except BaseException:
            self.group.rmdir()
            raise
        self.children = []

    def spawn(self, name, argv, env=None):
        def join():
            (self.group / "cgroup.procs").write_text(str(os.getpid()))
        with (self.lab.root / (name + ".log")).open("w") as log:
            child = subprocess.Popen(["runuser", "-u", self.lab.run_user, "--", *argv],
                                     env={**os.environ, **(env or {})}, stdout=log,
                                     stderr=subprocess.STDOUT, preexec_fn=join, cwd=self.lab.root)
        self.children.append(child)
        return child

    def close(self):
        empty_cgroup(self.group)
        for child in self.children:
            child.wait(timeout=10)
        self.group.rmdir()


def create_fixture_bucket(endpoint, timeout=30):
    # The node health endpoint can be ready before the S3 object layer exists.
    # Wait only for that explicit startup state, before any measured VM action.
    deadline = time.monotonic() + timeout
    last = "no S3 response"
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("S3 bucket initialization deadline: " + last)
        probe = command(["curl", "--silent", "--show-error", "--max-time", str(min(10, remaining)),
                         "--aws-sigv4", "aws:amz:us-east-1:s3", "--user", "m4-test:m4-disposable-password",
                         "-X", "PUT", "-w", "\n%{http_code}", endpoint + "/m4-test"], timeout=remaining)
        body, status = probe.stdout.rsplit("\n", 1)
        status = int(status)
        if status == 200:
            return
        code = None
        if status == 503:
            try:
                code = ET.fromstring(body).findtext("Code")
            except ET.ParseError:
                pass
        if status != 503 or code != "XMinioServerNotInitialized":
            check_status(status, 200, body.encode())
        last = f"HTTP {status}: {body}"
        print("M4: waiting for S3 initialization: " + last, flush=True)
        time.sleep(min(0.1, max(0, deadline - time.monotonic())))


def fixtures(lab, inputs, services, *, snapshot_config=None):
    repo = lab.root / "repo"
    if snapshot_config is None:
        endpoint = "http://10.241.0.1:9000"
        services.spawn("s3", [inputs["s3"], "server", "--address", "10.241.0.1:9000",
                              "--console-address", "127.0.0.1:0", str(repo / "s3")],
                       {"MINIO_ROOT_USER": "m4-test", "MINIO_ROOT_PASSWORD": "m4-disposable-password"})
        wait_http(endpoint + "/minio/health/ready")
        create_fixture_bucket(endpoint)
        for expected in (200, 412):
            probe = command(["curl", "--silent", "--show-error", "--max-time", "10", "--aws-sigv4",
                             "aws:amz:us-east-1:s3", "--user", "m4-test:m4-disposable-password",
                             "-X", "PUT", "-H", "If-None-Match: *", "--data", "probe",
                             "-w", "\n%{http_code}", endpoint + "/m4-test/conditional-probe"])
            body, status = probe.stdout.rsplit("\n", 1)
            check_status(int(status), expected, body.encode())
    cert, key = lab.root / "registry.pem", repo / "registry.key"
    command(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-subj", "/CN=m4-registry", "-addext", "subjectAltName=IP:10.241.0.1",
             "-addext", "basicConstraints=critical,CA:FALSE", "-keyout", str(key), "-out", str(cert)])
    os.chown(key, lab.account.pw_uid, lab.account.pw_gid)
    config = repo / "registry.json"
    config.write_text(json.dumps({"version": 0.1, "log": {"level": "error"},
        "storage": {"filesystem": {"rootdirectory": str(repo / "registry")}},
        "http": {"addr": "10.241.0.1:5443", "tls": {"certificate": str(cert), "key": str(key)}}}))
    services.spawn("registry", [inputs["registry"], "serve", str(config)])
    context = ssl.create_default_context(cafile=str(cert))
    url = "https://10.241.0.1:5443/v2/m4/rootfs"
    deadline = time.monotonic() + 15
    while True:
        try:
            request("https://10.241.0.1:5443/v2/", context=context)
            break
        except OSError:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.1)

    def upload(data, digest):
        _, headers = request(url + "/blobs/uploads/", "POST", b"", 202, context=context)
        location = urllib.parse.urljoin(url, headers["Location"])
        separator = "&" if "?" in location else "?"
        request(location + separator + urllib.parse.urlencode({"digest": digest}), "PUT", data, 201,
                {"Content-Type": "application/octet-stream"}, context)

    assets = json.loads(Path(inputs["assets"]).read_text())
    lower_config = json.loads(Path(assets["image_path"]).read_text())
    layers = []
    for lower in lower_config["lowers"]:
        data = Path(lower["file"]).read_bytes()
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        if digest != lower["digest"]:
            raise RuntimeError("prepared rootfs layer hash mismatch")
        upload(data, digest)
        layers.append({"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest,
                       "size": len(data), "annotations": {"containerd.io/snapshot/overlaybd/blob-digest": digest}})
    data = json.dumps({"architecture": "amd64", "os": "linux", "config": assets["image_configs"][0]["config"],
                       "rootfs": {"type": "layers", "diff_ids": [v["digest"] for v in layers]}}).encode()
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    upload(data, digest)
    manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest,
                           "size": len(data)}, "layers": layers}
    request(url + "/manifests/native", "PUT", json.dumps(manifest).encode(), 201,
            {"Content-Type": "application/vnd.oci.image.manifest.v1+json"}, context)
    return cert, "10.241.0.1:5443/m4/rootfs:native", assets


def network_cache_slots(inputs):
    value = inputs.get("network_cache_slots", 1)
    if type(value) is not int or value not in (0, 1):
        raise ValueError("network_cache_slots must be 0 or 1")
    return value


def start_server(node, inputs, cert, assets, backend="dragonball", *, snapshot_config=None, extra_env=None):
    node.request_timeout_seconds = inputs.get("request_timeout_seconds", 180)
    deps = node.root / "deps"
    deps.mkdir()
    for name in ("overlaybd", "regctl"):
        shutil.copytree(Path(inputs["deps"]) / name, deps / name)
    for path in [deps, *deps.rglob("*")]:
        os.chown(path, node.lab.account.pw_uid, node.lab.account.pw_gid)
    kernel_path = assets["kernel_path"]
    kernel_version = "vmlinux-6.1.175-dragonball-m2.1"
    if backend == "firecracker":
        kernel_path = str(deps / "vmlinux-fc.bin")
        shutil.copy2(inputs["kernel_fc"], kernel_path)
        kernel_version = "vmlinux-6.1.175"
    kernel_version = inputs.get("kernel_version", kernel_version)
    config = node.root / "config.toml"
    config.write_text(f'''virtualization_mode = "kvm"
[dragonball]
binary_path = {json.dumps(assets["runner_path"])}
[kernel]
version = {json.dumps(kernel_version)}
image_path = {json.dumps(kernel_path)}
[tools]
version = {json.dumps(inputs.get("tools_version", "0.1.0"))}
drive_path = {json.dumps(assets["tools_path"])}
[ublk]
daemon_binary_path = {json.dumps(inputs["daemon"])}
daemon_metrics_listen_addr = ""
[pool]
low_watermark = 0
high_watermark = {network_cache_slots(inputs)}
[pool.network]
enabled = false
maintenance_enabled = false
startup_prewarm = false
[pool.block]
enabled = false
maintenance_enabled = false
startup_prewarm = false
[pool.firecracker]
enabled = false
maintenance_enabled = false
startup_prewarm = false
[sandbox]
access_token_hash_seed = "m4-disposable-cross-node-test-seed"
[snapshot]
repository_backend = "oss"
p2p_enabled = false
[snapshot.image_publish]
enabled = false
[snapshot.memory_startup_pack]
enabled = false
consume_enabled = false
[snapshot.publish_compression]
enabled = false
[backend.oss]
endpoint = "http://10.241.0.1:9000"
bucket = "m4-test"
prefix = "db"
region = "us-east-1"
access_key_id = "m4-test"
access_key_secret = "m4-disposable-password"
cache_max_size_gb = 2
timeout_secs = {inputs.get("oss_timeout_seconds", 30)}
''')
    if snapshot_config is not None:
        config.write_text(config.read_text().split('\n[backend.oss]\n', 1)[0] + '\n[backend.oss]\n' +
                          ''.join(f'{key} = {json.dumps(value)}\n' for key, value in snapshot_config.items()))
    if backend == "firecracker":
        binary = node.root / "deps/firecracker"
        shutil.copy2(inputs["firecracker"], binary)
        with config.open("a") as output:
            output.write("\n[firecracker]\nbinary_path = " + json.dumps(str(binary)) +
                         '\nlog_level = "Info"\nserial_dir = "$AENV_HOME/logs/serial"\n')
    regctl = node.root / "regctl.json"
    regctl.write_text(json.dumps({"version": 1, "hosts": {"10.241.0.1:5443": {
        "tls": "enabled", "regCert": cert.read_text()}}}))
    server = inputs["server"] if backend == "dragonball" else inputs["server_fc"]
    from dragonball_m4_inputs import check as check_inputs
    report = check_inputs({'toml': [str(config)], 'json': [str(regctl)], 'executables': [server]},
                          run_user=node.lab.run_user)
    if not report['ok']:
        raise ValueError('generated runtime input preflight refused: ' + '; '.join(report['errors']))
    identity = json.loads(node.run([server, "--component-info-json"]).stdout)
    assert identity["backend"] == backend and identity[backend], identity
    assert not identity["firecracker" if backend == "dragonball" else "dragonball"], identity
    identity["runtime_uid"] = int(node.run(["id", "-u"]).stdout)
    assert identity["runtime_uid"] != 0
    timing_env = (["RUST_LOG=info,agentenv::sandbox::startup_timing=debug,agentenv::sandbox::dragonball::sandbox=debug"]
                  if inputs.get('phase_timing_versions') else [])
    child = node.spawn(["env", *timing_env, *[key + '=' + value for key, value in (extra_env or {}).items()],
                        "AENV_API_KEY=" + API_KEY, "REGCTL_CONFIG=" + str(regctl),
                        "SSL_CERT_FILE=" + str(cert), "API_ADDR=0.0.0.0:8000",
                        server, "--config", str(config)], node.lab.root / (node.name + ".server.log"))
    wait_http(f"http://{node.address}:8000/health", timeout=90)
    assert child.poll() is None
    return identity


def run_restore(lab, inputs):
    services = Services(lab)
    try:
        cert, image, assets = fixtures(lab, inputs, services)
        a, b = lab.start_node("a"), lab.start_node("b")
        identities = [start_server(n, inputs, cert, assets) for n in (a, b)]
        print("M4: real servers ready", flush=True)
        volume = api(a, "/volumes", "POST", {"name": "m4-source", "sizeMB": 16, "mode": "exclusive"}, 201)
        started = time.monotonic()
        source = api(a, "/sandboxes-cold", "POST", {"image": image, "cpuCount": 1, "memoryMB": 1024,
                     "timeout": 1800, "autoPause": False, "secure": False,
                     "volumeMounts": [{"name": volume["volumeID"], "path": "/volume"}]}, 201)
        create_seconds = time.monotonic() - started
        paths = {"ram": "/dev/shm/m4-marker", "disk": "/var/tmp/m4-marker", "volume": "/volume/m4-marker"}
        for kind, path in paths.items():
            guest_file(a, source, path, "m4-" + kind)
        print("M4: guest markers written", flush=True)
        started = time.monotonic()
        snapshot = api(a, f'/sandboxes/{source["sandboxID"]}/snapshots', "POST", {"name": "m4-db"}, 201)
        capture_seconds = time.monotonic() - started
        assert all(guest_file(a, source, path) == "m4-" + kind for kind, path in paths.items())
        a.close()  # hard node-process failure only after durable publication
        print("M4: source node stopped after commit", flush=True)
        cache = b.root / "home/snapshot-local-cache"
        assert not cache.exists() or not any(p.is_file() for p in cache.rglob("*")), "consumer cache not empty"
        b.disconnect()
        try:
            fault = b.run(["curl", "--silent", "--show-error", "--max-time", "150", "-w", "\n%{http_code}",
                           "-H", "X-API-Key: " + API_KEY, "-H", "Content-Type: application/json",
                           "--data", json.dumps({"templateID": snapshot["snapshotID"], "timeout": 600}),
                           "http://127.0.0.1:8000/sandboxes"], timeout=160)
            fault_status = int(fault.stdout.rsplit("\n", 1)[1])
            assert fault_status >= 400, fault.stdout
        finally:
            b.reconnect()
        network = {"outer_links": command(["ip", "-j", "link"]).stdout,
                   "outer_addresses": command(["ip", "-j", "address"]).stdout,
                   "outer_neighbors": command(["ip", "-j", "neigh"]).stdout,
                   "node_links": b.run(["ip", "-j", "link"]).stdout,
                   "node_addresses": b.run(["ip", "-j", "address"]).stdout,
                   "node_neighbors": b.run(["ip", "-j", "neigh"]).stdout}
        (lab.root / "reconnected-network.json").write_text(json.dumps(network, indent=2))
        # Link UP is not IP readiness: failed ARP neighbors may still need to
        # recover after the injected outage. Bound that recovery separately.
        recovery_started = time.monotonic()
        wait_http(f"http://{b.address}:8000/health", timeout=30)
        network_recovery_seconds = time.monotonic() - recovery_started
        print("M4: disconnected restore rejected; retrying", flush=True)
        started = time.monotonic()
        restored = api(b, "/sandboxes", "POST", {"templateID": snapshot["snapshotID"],
                       "timeout": 600, "autoPause": False, "secure": False}, 201)
        restore_seconds = time.monotonic() - started
        result = {"source_id": source["sandboxID"], "child_id": restored["sandboxID"],
                  "oss_timeout_seconds": inputs.get("oss_timeout_seconds", 30),
                  "request_timeout_seconds": inputs.get("request_timeout_seconds", 180),
                  "snapshot_id": snapshot["snapshotID"], "backend_identities": identities,
                  "node_namespaces": {n.name: json.loads((n.root / "ready").read_text()) for n in (a, b)},
                  "markers": {kind: guest_file(b, restored, path) for kind, path in paths.items()},
                  "fault_status": fault_status, "empty_consumer_cache": True,
                  "network_recovery_seconds": network_recovery_seconds,
                  "timings_seconds": {"cold_create_api": create_seconds, "capture_api": capture_seconds,
                                      "restore_api": restore_seconds},
                  "scope": "single-host-simulated-nodes", "performance_qualified": False}
        validate_restore(result)
        info = api(b, f'/sandboxes/{restored["sandboxID"]}')
        restored_volume = next(v["name"] for v in info["volumeMounts"] if v["path"] == "/volume")
        assert restored_volume != volume["volumeID"], "exclusive volume was not cloned"
        result["restored_volume_id"] = restored_volume
        assert any(s["snapshotID"] == snapshot["snapshotID"] for s in api(b, "/snapshots"))
        api(b, f'/sandboxes/{restored["sandboxID"]}', "DELETE", expected=204)
        return result
    finally:
        services.close()


def validate_inputs(inputs, scenario, uid):
    network_cache_slots(inputs)
    if uid != 0:
        raise ValueError("M4 requires root orchestration; workloads run as aenv")
    for name in ("oss_timeout_seconds", "request_timeout_seconds"):
        if name in inputs and (type(inputs[name]) is not int or not 1 <= inputs[name] <= 3600):
            raise ValueError(name + " must be an integer in 1..3600")
    names = ["server", "daemon", "assets", "s3", "registry", "deps", "ublk_cli"]
    if scenario == "rollback":
        names += ["server_fc", "firecracker", "kernel_fc"]
    for name in names:
        path = Path(inputs.get(name, ""))
        valid = path.is_dir() if name == "deps" else path.is_file()
        if not path.is_absolute() or not valid:
            raise ValueError("missing absolute input: " + name)


def terminate_controller(signum, _frame):
    # A second cancellation must not interrupt the finally blocks reclaiming nodes.
    for value in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(value, signal.SIG_IGN)
    raise SystemExit(128 + signum)


def isolated_controller(argv, env):
    from dragonball_m4_host import lock_fds
    child = None
    blocked = {signal.SIGTERM, signal.SIGINT, signal.SIGHUP}
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, blocked)
    try:
        try:
            # Track the child before a pending cancellation can run our handler.
            child = subprocess.Popen(argv, env=env, start_new_session=True, pass_fds=lock_fds(),
                                     preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, previous))
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)
        return child.wait()
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            child.wait(timeout=90)


def execute_scenario(lab, inputs, scenario):
    try:
        with lab:
            if scenario == "restore":
                result = run_restore(lab, inputs)
            else:
                from dragonball_m4_rollback import run_rollback
                result = run_rollback(lab, inputs)
            (lab.root / "result.json").write_text(json.dumps(result, indent=2))
    finally:
        errors = []
        for node in lab.nodes:
            try:
                cleanup_owned_ublk(node, inputs, receipt_path=node.root / "ublk-cleanup.json")
            except Exception as error:
                errors.append(f"{node.name}: {error}")
        if errors:
            raise RuntimeError("scenario ublk cleanup: " + "; ".join(errors))
    return result


def main():
    for value in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(value, terminate_controller)
    parser = argparse.ArgumentParser()
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--inside", action="store_true")
    parser.add_argument("--scenario", choices=("restore", "rollback"), default="restore")
    args = parser.parse_args()
    if sys.version_info < (3, 11):
        parser.error("M4 requires Python >= 3.11")
    inputs = json.loads(args.inputs.read_text())
    validate_inputs(inputs, args.scenario, os.geteuid())
    if not args.inside:
        env = {**os.environ, "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net")}
        return isolated_controller(["unshare", "--mount", "--net", "--propagation", "private", sys.executable,
                                    __file__, "--inside", "--inputs", str(args.inputs.resolve()),
                                    "--scenario", args.scenario], env)
    root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
    print("M4 lab root: " + str(root), flush=True)
    result = execute_scenario(Lab(root, "aenv"), inputs, args.scenario)
    print(json.dumps({"root": str(root), "cleanup": "passed", "result": result}), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
