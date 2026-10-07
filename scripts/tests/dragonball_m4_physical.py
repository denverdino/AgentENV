#!/usr/bin/env python3
"""Two physical hosts; durable data crosses private TCP, never the SSH controller.

Root owns disposable namespace/relay orchestration. Existing Lab helpers run
servers, VMs and repository fixtures as aenv. Closing controller stdin tears
down the worker; a wall-clock watchdog bounds abandoned SSH sessions.
"""
import argparse
import contextlib
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import selectors
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback

from dragonball_m4_lab import Lab, wait_http
from dragonball_m4_cleanup import cleanup_owned_ublk, owned_device_ids
from dragonball_m4_scenarios import (
    API_KEY, Services, api, fixtures, guest_file, isolated_controller,
    start_server, terminate_controller, validate_restore,
)

PATHS = {"ram": "/dev/shm/m4-marker", "disk": "/var/tmp/m4-marker", "volume": "/volume/m4-marker"}
RESPONSE_TIMEOUT = 600
CLEANUP_TIMEOUT = 120
TERMINATE_TIMEOUT = 5


def private_ipv4(value):
    address = ipaddress.IPv4Address(value)
    if not any(address in ipaddress.IPv4Network(network)
               for network in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")):
        raise ValueError("relay requires an explicit RFC1918 private IPv4 address")
    return str(address)


def validate_physical(result):
    validate_restore(result)
    source, target = result["source"], result["target"]
    assert source["boot_id"] != target["boot_id"], "distinct physical boot IDs required"
    assert source["server_sha256"] == target["server_sha256"], "server builds differ"
    assert source["asset_sha256"] == target["asset_sha256"], "guest assets differ"
    assert source["runtime_uid"] != 0 and target["runtime_uid"] != 0, "root workload"
    assert result["source_stopped"], "source workload still running"
    assert result["empty_consumer_cache"], "consumer cache not empty"
    assert result["fault_status"] >= 400, "outage unexpectedly succeeded"
    assert result["catalog_retained"], "snapshot catalog entry lost"
    assert result["source_volume_id"] != result["restored_volume_id"], "exclusive volume not cloned"


def stream(host, port):
    """Binary stdin/stdout transport, including half-close for request EOF."""
    with socket.create_connection((host, port), timeout=15) as connection:
        connection.settimeout(None)
        errors = []
        def send():
            try:
                while data := os.read(0, 65536):
                    connection.sendall(data)
                connection.shutdown(socket.SHUT_WR)
            except OSError as error:
                errors.append(error)
        sender = threading.Thread(target=send, daemon=True)
        sender.start()
        while data := connection.recv(65536):
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        if errors:
            raise errors[0]


def relay(bind, port, upstream_pid, upstream_host, upstream_port):
    """Listener and its stream subprocesses share one controller-owned group."""
    private_ipv4(bind)
    private_ipv4(upstream_host)
    def forward(connection):
        with connection:
            child = subprocess.Popen([
                "nsenter", "-t", str(upstream_pid), "-n", "--", sys.executable,
                str(Path(__file__).resolve()), "--stream", upstream_host, str(upstream_port),
            ], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            try:
                def send():
                    try:
                        while data := connection.recv(65536):
                            child.stdin.write(data)
                            child.stdin.flush()
                    except (OSError, BrokenPipeError):
                        pass
                    finally:
                        child.stdin.close()
                threading.Thread(target=send, daemon=True).start()
                while data := os.read(child.stdout.fileno(), 65536):
                    connection.sendall(data)
                child.wait(timeout=5)
            finally:
                child.stdout.close()
                if child.poll() is None:
                    child.kill()
                    child.wait()
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((bind, port))
        listener.listen(128)
        while True:
            connection, _ = listener.accept()
            threading.Thread(target=forward, args=(connection,), daemon=True).start()


class Worker:
    def __init__(self, lab, inputs, host_pid, private_ip):
        self.lab, self.inputs = lab, inputs
        self.host_pid, self.private_ip = host_pid, private_ip
        self.services, self.node = None, None
        self.relays = []

    def add_relay(self, bind, port, upstream_pid, upstream_host, upstream_port, host=False):
        argv = [sys.executable, str(Path(__file__).resolve()), "--relay", bind, str(port),
                str(upstream_pid), upstream_host, str(upstream_port)]
        if host:
            argv = ["nsenter", "-t", str(self.host_pid), "-n", "--", *argv]
        with (self.lab.root / f"relay-{port}.log").open("w") as log:
            child = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        self.relays.append(child)
        time.sleep(0.2)
        if child.poll() is not None:
            raise RuntimeError(f"relay {port} failed")

    def identity(self):
        with Path(self.inputs["server"]).open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        assets = json.loads(Path(self.inputs["assets"]).read_text())
        hashes = {}
        for name in ("runner_path", "kernel_path", "tools_path"):
            with Path(assets[name]).open("rb") as source:
                hashes[name] = hashlib.file_digest(source, "sha256").hexdigest()
        return {"boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "hostname": socket.gethostname(), "private_ip": self.private_ip,
                "kernel": os.uname().release, "server_sha256": digest,
                "asset_sha256": hashes, "runtime_uid": self.lab.account.pw_uid,
                "root": str(self.lab.root)}

    def prepare(self, role, peer=None, certificate=None):
        private_ipv4(self.private_ip)
        if role == "source":
            self.services = Services(self.lab)
            cert, image, assets = fixtures(self.lab, self.inputs, self.services)
            self.add_relay(self.private_ip, 19000, os.getpid(), "10.241.0.1", 9000, host=True)
            self.add_relay(self.private_ip, 15443, os.getpid(), "10.241.0.1", 5443, host=True)
        else:
            private_ipv4(peer)
            self.add_relay("10.241.0.1", 9000, self.host_pid, peer, 19000)
            self.add_relay("10.241.0.1", 5443, self.host_pid, peer, 15443)
            cert = self.lab.root / "registry.pem"
            cert.write_text(certificate)
            image = "10.241.0.1:5443/m4/rootfs:native"
            assets = json.loads(Path(self.inputs["assets"]).read_text())
        wait_http("http://10.241.0.1:9000/minio/health/ready")
        self.node = self.lab.start_node("a")
        identity = start_server(self.node, self.inputs, cert, assets)
        return {**self.identity(), "component": identity, "certificate": cert.read_text(), "image": image}

    def capture(self, image):
        volume = api(self.node, "/volumes", "POST", {"name": "m4-physical", "sizeMB": 16,
                                                     "mode": "exclusive"}, 201)
        started = time.monotonic()
        source = api(self.node, "/sandboxes-cold", "POST", {
            "image": image, "cpuCount": 1, "memoryMB": 1024, "timeout": 1800,
            "autoPause": False, "secure": False,
            "volumeMounts": [{"name": volume["volumeID"], "path": "/volume"}]}, 201)
        create_seconds = time.monotonic() - started
        for kind, path in PATHS.items():
            guest_file(self.node, source, path, "m4-" + kind)
        started = time.monotonic()
        snapshot = api(self.node, f'/sandboxes/{source["sandboxID"]}/snapshots', "POST",
                       {"name": "m4-physical"}, 201)
        capture_seconds = time.monotonic() - started
        assert all(guest_file(self.node, source, path) == "m4-" + kind for kind, path in PATHS.items())
        self.node.close()
        stopped = not self.node.cgroup.exists() and all(p.poll() is not None for p in self.node.processes)
        result = {"source_id": source["sandboxID"], "snapshot_id": snapshot["snapshotID"],
                  "source_volume_id": volume["volumeID"], "source_stopped": stopped,
                  "source_process_exit_codes": [p.returncode for p in self.node.processes],
                  "create_seconds": create_seconds, "capture_seconds": capture_seconds}
        (self.lab.root / "capture.json").write_text(json.dumps(result, indent=2))
        return result

    def restore(self, snapshot_id):
        cache = self.node.root / "home/snapshot-local-cache"
        empty = not cache.exists() or not any(p.is_file() for p in cache.rglob("*"))
        assert empty, "consumer snapshot cache not empty"
        self.node.disconnect()
        try:
            fault = self.node.run([
                "curl", "--silent", "--show-error", "--max-time", "150", "-w", "\n%{http_code}",
                "-H", "X-API-Key: " + API_KEY, "-H", "Content-Type: application/json",
                "--data", json.dumps({"templateID": snapshot_id, "timeout": 600}),
                "http://127.0.0.1:8000/sandboxes"], timeout=160)
            status = int(fault.stdout.rsplit("\n", 1)[1])
            assert status >= 400, fault.stdout
            (self.lab.root / "outage-response.txt").write_text(fault.stdout)
        finally:
            self.node.reconnect()
        wait_http(f"http://{self.node.address}:8000/health", timeout=30)
        started = time.monotonic()
        child = api(self.node, "/sandboxes", "POST", {"templateID": snapshot_id, "timeout": 600,
                    "autoPause": False, "secure": False}, 201)
        restore_seconds = time.monotonic() - started
        info = api(self.node, f'/sandboxes/{child["sandboxID"]}')
        result = {"child_id": child["sandboxID"], "empty_consumer_cache": empty,
                  "fault_status": status, "restore_seconds": restore_seconds,
                  "markers": {kind: guest_file(self.node, child, path) for kind, path in PATHS.items()},
                  "restored_volume_id": next(v["name"] for v in info["volumeMounts"] if v["path"] == "/volume"),
                  "catalog_retained": any(s["snapshotID"] == snapshot_id for s in api(self.node, "/snapshots"))}
        api(self.node, f'/sandboxes/{child["sandboxID"]}', "DELETE", expected=204)
        (self.lab.root / "restore.json").write_text(json.dumps(result, indent=2))
        return result

    def close(self):
        errors = []
        if self.node:
            for action in (self.node.close, lambda: cleanup_owned_ublk(self.node, self.inputs)):
                try:
                    action()
                except Exception as error:
                    errors.append(str(error))
        for child in reversed(self.relays):
            try:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=10)
            except Exception as error:
                errors.append(str(error))
        if self.services:
            try:
                self.services.close()
            except Exception as error:
                errors.append(str(error))
        if errors:
            raise RuntimeError("worker cleanup: " + "; ".join(errors))


def worker(args):
    inputs = json.loads(args.inputs.read_text())
    address = args.private_ip if args.private_ip is not None else inputs.get("private_ip")
    if not address:
        raise ValueError("private_ip in --inputs or --private-ip is required")
    args.private_ip = private_ipv4(address)
    cleanup_binary = Path(inputs.get("ublk_cli", ""))
    if not cleanup_binary.is_absolute() or not cleanup_binary.is_file():
        raise ValueError("physical M4 requires absolute ublk_cli for exact owned-device cleanup")
    if not args.inside:
        env = {**os.environ, "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net")}
        return isolated_controller(["unshare", "--mount", "--net", "--propagation", "private",
            sys.executable, __file__, "--worker", "--inside", "--inputs", str(args.inputs),
            "--private-ip", args.private_ip, "--host-pid", str(os.getpid())], env)
    signal.signal(signal.SIGALRM, terminate_controller)
    signal.alarm(1800)
    root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
    with Lab(root, "aenv") as lab:
        instance = Worker(lab, inputs, args.host_pid, args.private_ip)
        try:
            print(json.dumps({"ready": instance.identity()}), flush=True)
            for line in sys.stdin:
                item = json.loads(line)
                if item["operation"] == "close":
                    break
                try:
                    with contextlib.redirect_stdout(sys.stderr):
                        result = getattr(instance, item["operation"])(**item.get("args", {}))
                    print(json.dumps({"result": result}), flush=True)
                except Exception:
                    error = traceback.format_exc()
                    (root / "failure.txt").write_text(error)
                    print(json.dumps({"error": error, "root": str(root)}), flush=True)
                    return 1
        finally:
            # EOF can race parent cancellation. Once cleanup begins, a first
            # signal must not interrupt Lab.close halfway through its teardown.
            for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGALRM):
                signal.signal(signum, signal.SIG_IGN)
            instance.close()
    print(json.dumps({"cleanup": "passed", "root": str(root)}), flush=True)
    return 0


class Remote:
    def __init__(self, specification, log):
        self.spec = specification
        self.buffer = b""
        self.cleanup_confirmed = False
        self.output = log.open("w")
        self.child = subprocess.Popen(["ssh", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=3", specification["ssh"], shlex.join([
                "python3", specification["script"], "--worker", "--inputs", specification["inputs"],
                "--private-ip", specification["private_ip"]])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.output, text=True, bufsize=1)
        try:
            self.ready = self.read()["ready"]
        except BaseException:
            # Construction failed before the controller could take ownership.
            # Closing stdin lets the remote unwind; retain the original error.
            try:
                self.reap_transport()
            except Exception as error:
                print(f"transport cleanup after readiness failure: {error}", file=sys.stderr)
            raise

    def read(self, timeout=None):
        deadline = time.monotonic() + (RESPONSE_TIMEOUT if timeout is None else timeout)
        with selectors.DefaultSelector() as selector:
            selector.register(self.child.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(timeout=remaining):
                    raise TimeoutError("remote worker response timed out")
                data = os.read(self.child.stdout.fileno(), 65536)
                if not data:
                    raise RuntimeError("remote worker exited; inspect SSH log")
                self.buffer += data
                if len(self.buffer) > 1024 * 1024:
                    raise ValueError("oversized remote worker response")
        line, self.buffer = self.buffer.split(b"\n", 1)
        value = json.loads(line)
        if "error" in value:
            raise RuntimeError(value)
        return value

    def call(self, operation, **args):
        self.child.stdin.write(json.dumps({"operation": operation, "args": args}) + "\n")
        self.child.stdin.flush()
        return self.read()["result"]

    def close(self):
        try:
            self.child.stdin.close()
            value = self.read(timeout=CLEANUP_TIMEOUT)
            if value.get("cleanup") != "passed":
                raise RuntimeError(f"remote cleanup not confirmed: {value}")
            self.cleanup_confirmed = True
        finally:
            self.reap_transport()
        if self.child.returncode != 0:
            raise RuntimeError(f"remote worker exit {self.child.returncode}")

    def reap_transport(self):
        try:
            self.child.stdin.close()
            try:
                self.child.wait(timeout=CLEANUP_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.child.terminate()
                try:
                    self.child.wait(timeout=TERMINATE_TIMEOUT)
                except subprocess.TimeoutExpired:
                    self.child.kill()
                    self.child.wait(timeout=TERMINATE_TIMEOUT)
        finally:
            self.child.stdin.close()
            self.child.stdout.close()
            self.output.close()


def controller(args):
    specifications = json.loads(args.hosts.read_text())
    assert len(specifications) == 2
    assert specifications[0]["ssh"] != specifications[1]["ssh"]
    for specification in specifications:
        private_ipv4(specification["private_ip"])
    args.output.mkdir(parents=True, exist_ok=False)
    for index in (0, 1):
        source = target = None
        result = {"direction": index, "scope": "two-physical-hosts-private-network",
                  "performance_qualified": False, "failure_scope": "source-workload-node-stopped; repository-fixtures-remain"}
        try:
            source = Remote(specifications[index], args.output / f"{index}-source.log")
            target = Remote(specifications[1 - index], args.output / f"{index}-target.log")
            assert source.ready["boot_id"] != target.ready["boot_id"]
            assert source.ready["server_sha256"] == target.ready["server_sha256"]
            assert source.ready["asset_sha256"] == target.ready["asset_sha256"]
            print(json.dumps({"direction": index, "source": source.ready, "target": target.ready}), flush=True)
            prepared = source.call("prepare", role="source")
            result["source"] = {k: v for k, v in prepared.items() if k != "certificate"}
            prepared_target = target.call("prepare", role="target", peer=source.spec["private_ip"],
                                          certificate=prepared["certificate"])
            result["target"] = {k: v for k, v in prepared_target.items() if k != "certificate"}
            result.update(source.call("capture", image=prepared["image"]))
            print(json.dumps({"direction": index, "source_stopped": result["source_stopped"],
                              "snapshot_id": result["snapshot_id"]}), flush=True)
            result.update(target.call("restore", snapshot_id=result["snapshot_id"]))
            validate_physical(result)
            result["functional_qualified"] = True
        finally:
            errors = []
            for remote in (target, source):
                if remote:
                    try:
                        remote.close()
                    except Exception as error:
                        errors.append(str(error))
            result["cleanup_errors"] = errors
            (args.output / f"{index}-result.json").write_text(json.dumps(result, indent=2))
            if errors:
                raise RuntimeError(errors)
        print(json.dumps(result), flush=True)
    return 0


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--stream":
        stream(sys.argv[2], int(sys.argv[3]))
        return 0
    if len(sys.argv) == 7 and sys.argv[1] == "--relay":
        relay(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], int(sys.argv[6]))
        return 0
    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, terminate_controller)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--inside", action="store_true")
    parser.add_argument("--inputs", type=Path)
    parser.add_argument("--private-ip", help="worker RFC1918 address; overrides private_ip in --inputs")
    parser.add_argument("--host-pid", type=int)
    parser.add_argument("--hosts", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    return worker(args) if args.worker else controller(args)


if __name__ == "__main__":
    raise SystemExit(main())
