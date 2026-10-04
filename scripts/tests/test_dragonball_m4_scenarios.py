#!/usr/bin/env python3
import importlib.util
import contextlib
import http.server
import io
import json
import struct
import tempfile
import threading
from pathlib import Path
import unittest
from types import SimpleNamespace
from unittest.mock import patch


def module():
    path = Path(__file__).with_name("dragonball_m4_scenarios.py")
    if not path.exists():
        raise AssertionError("real-server M4 scenario missing")
    spec = importlib.util.spec_from_file_location("scenarios", path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


@contextlib.contextmanager
def bucket_service(responses):
    requests = []
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_PUT(self):
            requests.append((self.path, self.headers.get("Authorization", "")))
            status, body = responses[min(len(requests) - 1, len(responses) - 1)]
            self.send_response(status)
            self.end_headers()
            self.wfile.write(body.encode())

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.01))
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        thread.join(timeout=2)
        server.server_close()


class ScenarioTests(unittest.TestCase):
    def test_scenario_reclaims_each_node_after_close_even_when_an_operation_or_cleanup_fails(self):
        for failure_stage in (None, "operation", "lab close"):
            for cleanup_fails in (False, True):
                with self.subTest(failure_stage=failure_stage, cleanup_fails=cleanup_fails), \
                     tempfile.TemporaryDirectory() as directory:
                    implementation = module()
                    class FakeLab:
                        def __init__(self):
                            self.root = Path(directory)
                            self.nodes = [SimpleNamespace(name=name, root=self.root / name) for name in ("a", "b")]
                            for node in self.nodes:
                                node.root.mkdir()
                            self.closed = False
                        def __enter__(self):
                            return self
                        def __exit__(self, *args):
                            self.closed = True
                            if failure_stage == "lab close":
                                raise RuntimeError("injected lab close failure")
                    lab, attempts = FakeLab(), []
                    def cleanup(node, inputs, *, receipt_path):
                        self.assertTrue(lab.closed)
                        self.assertEqual(receipt_path, node.root / "ublk-cleanup.json")
                        attempts.append(node.name)
                        if cleanup_fails and node.name == "a":
                            raise RuntimeError("injected cleanup failure")
                    failure = RuntimeError("injected operation failure") if failure_stage == "operation" else None
                    with patch.object(implementation, "run_restore", return_value={"ok": True}, side_effect=failure), \
                         patch.object(implementation, "cleanup_owned_ublk", side_effect=cleanup, create=True):
                        if failure_stage or cleanup_fails:
                            with self.assertRaisesRegex(RuntimeError, "cleanup" if cleanup_fails else failure_stage):
                                implementation.execute_scenario(lab, {}, "restore")
                        else:
                            self.assertEqual(implementation.execute_scenario(lab, {}, "restore"), {"ok": True})
                    self.assertEqual(attempts, ["a", "b"])

    def test_bucket_waits_for_s3_initialization_using_real_signed_requests(self):
        pending = "<Error><Code>XMinioServerNotInitialized</Code></Error>"
        with bucket_service([(503, pending), (200, "")]) as (endpoint, requests):
            module().create_fixture_bucket(endpoint, timeout=2)
        self.assertEqual(len(requests), 2)
        self.assertTrue(all(path == "/m4-test" and auth.startswith("AWS4-HMAC-SHA256 ")
                            for path, auth in requests))

    def test_bucket_does_not_retry_other_s3_failures(self):
        for status, body in ((403, "<Error><Code>AccessDenied</Code></Error>"),
                             (500, "<Error><Code>InternalError</Code></Error>"),
                             (503, "<Error><Code>SlowDown</Code></Error>"),
                             (503, "not XML")):
            with self.subTest(status=status, body=body), bucket_service([(status, body)]) as (endpoint, requests):
                with self.assertRaisesRegex(RuntimeError, str(status)):
                    module().create_fixture_bucket(endpoint, timeout=2)
                self.assertEqual(len(requests), 1)

    def test_bucket_initialization_has_a_total_deadline(self):
        implementation = module()
        pending = "<Error><Code>XMinioServerNotInitialized</Code></Error>"
        # Deadline arithmetic is deterministic; recovery/classification above
        # retain real HTTP/curl without requiring process scheduling in 250ms.
        with patch.object(implementation.time, "monotonic", side_effect=[0, 0, 0.2, 0.25]), \
             patch.object(implementation.time, "sleep") as sleep, \
             patch.object(implementation, "command", return_value=SimpleNamespace(stdout=pending + "\n503")) as command:
            with self.assertRaisesRegex(TimeoutError, "XMinioServerNotInitialized"):
                implementation.create_fixture_bucket("http://127.0.0.1:9000", timeout=0.25)
        command.assert_called_once()
        self.assertEqual(command.call_args.kwargs["timeout"], 0.25)
        self.assertAlmostEqual(sleep.call_args.args[0], 0.05)

    def test_api_uses_explicit_node_deadline_without_changing_default(self):
        implementation = module()
        for configured, expected in ((None, 180), (600, 600)):
            with self.subTest(configured=configured):
                node = SimpleNamespace(address="127.0.0.1")
                if configured is not None:
                    node.request_timeout_seconds = configured
                response = io.BytesIO(b'{}')
                response.status, response.headers = 200, {}
                with patch.object(implementation.urllib.request, "urlopen", return_value=response) as opened:
                    self.assertEqual(implementation.api(node, "/snapshots"), {})
                self.assertEqual(opened.call_args.kwargs["timeout"], expected)

    def test_preflight_validates_explicit_timeout_budgets(self):
        implementation = module()
        with tempfile.TemporaryDirectory() as directory:
            file = Path(directory) / "asset"
            file.write_text("fixture")
            inputs = {name: str(file) for name in ("server", "daemon", "assets", "s3", "registry", "ublk_cli")}
            inputs["deps"] = directory
            for name in ("oss_timeout_seconds", "request_timeout_seconds"):
                for invalid in (0, -1, 3601, True, 1.5, "120"):
                    with self.subTest(name=name, invalid=invalid), self.assertRaisesRegex(ValueError, name):
                        implementation.validate_inputs({**inputs, name: invalid}, "restore", uid=0)
            implementation.validate_inputs({**inputs, "oss_timeout_seconds": 120,
                                             "request_timeout_seconds": 600}, "restore", uid=0)

    def test_guest_requests_use_the_same_node_deadline(self):
        implementation = module()
        node = SimpleNamespace(address="127.0.0.1", request_timeout_seconds=600)
        sandbox = {"sandboxID": "test-sandbox"}
        end = json.dumps({"event": {"end": {"exitCode": 0}}}).encode()
        command_body = struct.pack(">BI", 0, len(end)) + end + struct.pack(">BI", 2, 2) + b"{}"
        for body, call, expected in (
            (b"file", lambda: implementation.guest_file(node, sandbox, "/file"), "file"),
            (command_body, lambda: implementation.guest_command(node, sandbox, "true"),
             {"stdout": "", "stderr": "", "exit_code": 0}),
        ):
            with self.subTest(body=body):
                response = io.BytesIO(body)
                response.status, response.headers = 200, {}
                with patch.object(implementation.urllib.request, "urlopen", return_value=response) as opened:
                    self.assertEqual(call(), expected)
                self.assertEqual(opened.call_args.kwargs["timeout"], 600)

    def test_fixture_budget_is_explicit_and_invalid_budget_allocates_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            lab = SimpleNamespace(cgroup=Path(directory))
            implementation = module()
            for budget in (0, 129):
                with self.assertRaises(ValueError):
                    implementation.Services(lab, memory_gib=budget)
                self.assertEqual(list(lab.cgroup.iterdir()), [])
            services = implementation.Services(lab, memory_gib=16)
            self.assertEqual((services.group / "memory.max").read_text(), "17179869184")

    def test_partial_fixture_initialization_removes_owned_cgroup(self):
        with tempfile.TemporaryDirectory() as directory:
            lab = SimpleNamespace(cgroup=Path(directory))
            with patch.object(Path, "write_text", side_effect=OSError("injected limit failure")):
                with self.assertRaisesRegex(OSError, "injected"):
                    module().Services(lab)
            self.assertEqual(list(lab.cgroup.iterdir()), [])

    def test_preflight_rejects_missing_paths_and_nonroot_before_allocation(self):
        implementation = module()
        self.assertTrue(hasattr(implementation, "validate_inputs"), "pre-allocation validator missing")
        with tempfile.TemporaryDirectory() as directory:
            file = Path(directory) / "asset"
            file.write_text("fixture")
            inputs = {name: str(file) for name in ("server", "daemon", "assets", "s3", "registry", "ublk_cli")}
            inputs["deps"] = directory
            with self.assertRaisesRegex(ValueError, "server_fc"):
                implementation.validate_inputs(inputs, "rollback", uid=0)
            with self.assertRaisesRegex(ValueError, "root orchestration"):
                implementation.validate_inputs(inputs, "restore", uid=999)
            with self.assertRaisesRegex(ValueError, "ublk_cli"):
                implementation.validate_inputs({key: value for key, value in inputs.items() if key != "ublk_cli"}, "restore", uid=0)
            inputs["server"] = "relative"
            with self.assertRaisesRegex(ValueError, "server"):
                implementation.validate_inputs(inputs, "restore", uid=0)

    def test_rollback_only_changes_new_create_pool(self):
        implementation = module()
        self.assertTrue(hasattr(implementation, "route_pool"), "pool selector missing")
        route = implementation.route_pool
        self.assertEqual(route("create", "firecracker"), "firecracker")
        self.assertEqual(route("resume", "firecracker", "dragonball"), "dragonball")
        self.assertEqual(route("resume", "dragonball", "firecracker"), "firecracker")
        with self.assertRaises(ValueError):
            route("resume", "firecracker")

    def test_process_frames_require_exit_and_reject_stream_errors(self):
        implementation = module()
        self.assertTrue(hasattr(implementation, "process_output"), "Connect decoder missing")
        def frame(value, flag=0):
            data = json.dumps(value).encode()
            return struct.pack(">BI", flag, len(data)) + data
        good = frame({"event": {"data": {"stdout": "b2s="}}}) + frame({"event": {"end": {"exitCode": 0}}}) + frame({}, 2)
        self.assertEqual(implementation.process_output(good), {"stdout": "ok", "stderr": "", "exit_code": 0})
        for broken in (good[:-1], frame({}, 2), frame({"error": {"code": "unavailable"}}, 2)):
            with self.assertRaises(ValueError):
                implementation.process_output(broken)

    def test_oracle_requires_ram_disk_volume_and_new_id(self):
        validate = module().validate_restore
        good = {"source_id": "source", "child_id": "child", "markers": {
            "ram": "m4-ram", "disk": "m4-disk", "volume": "m4-volume"}}
        validate(good)
        for key in ("ram", "disk", "volume"):
            bad = {**good, "markers": {**good["markers"], key: "wrong"}}
            with self.assertRaises(AssertionError):
                validate(bad)
        with self.assertRaises(AssertionError):
            validate({**good, "child_id": "source"})

    def test_http_status_not_silently_accepted(self):
        check = module().check_status
        check(201, 201, b"{}")
        with self.assertRaisesRegex(RuntimeError, "500"):
            check(500, 201, b"failure")


if __name__ == "__main__":
    unittest.main(verbosity=2)
