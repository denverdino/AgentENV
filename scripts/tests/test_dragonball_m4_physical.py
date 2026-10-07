#!/usr/bin/env python3
"""Physical qualification checks and real TCP relay contract."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import dragonball_m4_physical as physical


class PhysicalTests(unittest.TestCase):
    def test_worker_address_config_and_cli_override_are_validated_before_allocation(self):
        cases = [
            ({"private_ip": "10.20.0.10"}, [], "absolute ublk_cli"),
            ({"private_ip": "203.0.113.1"}, ["--private-ip", "10.20.0.11"], "absolute ublk_cli"),
            ({"private_ip": "10.20.0.10"}, ["--private-ip", "203.0.113.1"], "RFC1918"),
            ({"private_ip": "203.0.113.1"}, [], "RFC1918"),
            ({}, [], "private_ip in --inputs or --private-ip is required"),
        ]
        with tempfile.TemporaryDirectory() as directory:
            inputs = Path(directory) / "inputs.json"
            for config, flags, expected in cases:
                with self.subTest(config=config, flags=flags):
                    inputs.write_text(json.dumps(config))
                    result = subprocess.run([
                        sys.executable, str(Path(physical.__file__)), "--worker",
                        "--inputs", str(inputs), *flags,
                    ], capture_output=True, text=True, timeout=5)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(expected, result.stderr)

    def test_relative_cleanup_binary_is_rejected_before_lab_allocation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "cli").write_text("placeholder")
            (root / "inputs.json").write_text(json.dumps({"ublk_cli": "cli"}))
            result = subprocess.run([sys.executable, str(Path(physical.__file__)), "--worker",
                "--inputs", str(root / "inputs.json"), "--private-ip", "10.20.0.10"],
                cwd=root, capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("absolute ublk_cli", result.stderr)

    def test_unresponsive_close_is_reaped_and_never_claims_remote_cleanup(self):
        child = subprocess.Popen([sys.executable, "-c",
            "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('{\"ready\": {}}',flush=True); time.sleep(60)"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            with tempfile.TemporaryDirectory() as directory, \
                 patch.object(physical.subprocess, "Popen", return_value=child), \
                 patch.object(physical, "CLEANUP_TIMEOUT", 0.05), \
                 patch.object(physical, "TERMINATE_TIMEOUT", 0.05):
                spec = {"ssh": "unused", "script": "unused", "inputs": "unused", "private_ip": "10.20.0.10"}
                remote = physical.Remote(spec, Path(directory) / "ssh.log")
                with self.assertRaises(TimeoutError):
                    remote.close()
                self.assertFalse(remote.cleanup_confirmed)
                self.assertEqual(child.returncode, -9)
                self.assertTrue(child.stdin.closed)
                self.assertTrue(child.stdout.closed)
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()
            child.stderr.close()

    def test_device_or_receipt_failure_still_stops_real_relay_and_fixture_processes(self):
        for failure in ("device deletion", "receipt write"):
            with self.subTest(failure=failure):
                relay = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
                fixture = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
                def close_fixture():
                    fixture.terminate()
                    fixture.wait(timeout=2)
                instance = physical.Worker(None, {}, 1, "10.20.0.10")
                instance.node = SimpleNamespace(close=lambda: None)
                instance.relays = [relay]
                instance.services = SimpleNamespace(close=close_fixture)
                try:
                    with patch.object(physical, "cleanup_owned_ublk", side_effect=RuntimeError(failure)):
                        with self.assertRaises(RuntimeError):
                            instance.close()
                    self.assertIsNotNone(relay.poll())
                    self.assertIsNotNone(fixture.poll())
                finally:
                    for child in (relay, fixture):
                        if child.poll() is None:
                            child.kill()
                        child.wait()

    def test_unresponsive_failed_start_is_killed_without_masking_readiness_error(self):
        child = subprocess.Popen([sys.executable, "-c",
            "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('invalid-json',flush=True); time.sleep(60)"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            with tempfile.TemporaryDirectory() as directory, \
                 patch.object(physical.subprocess, "Popen", return_value=child), \
                 patch.object(physical, "CLEANUP_TIMEOUT", 0.05, create=True), \
                 patch.object(physical, "TERMINATE_TIMEOUT", 0.05, create=True):
                spec = {"ssh": "unused", "script": "unused", "inputs": "unused", "private_ip": "10.20.0.10"}
                errors = []
                def connect():
                    try:
                        physical.Remote(spec, Path(directory) / "ssh.log")
                    except Exception as error:
                        errors.append(error)
                thread = threading.Thread(target=connect)
                thread.start()
                thread.join(timeout=1)
                stalled = thread.is_alive()
                if stalled:
                    child.kill()
                    thread.join(timeout=5)
                self.assertFalse(stalled, "unresponsive startup transport was not reaped")
                self.assertIsInstance(errors[0], json.JSONDecodeError)
                self.assertEqual(child.returncode, -9)
                self.assertTrue(child.stdin.closed)
                self.assertTrue(child.stdout.closed)
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()
            child.stderr.close()

    def test_node_cleanup_failure_still_stops_relays_and_fixtures(self):
        relay = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        fixture = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        def failing_node_close():
            raise RuntimeError("injected node teardown failure")
        def close_fixture():
            fixture.terminate()
            fixture.wait(timeout=2)
        instance = physical.Worker(None, {}, 1, "10.20.0.10")
        instance.node = SimpleNamespace(close=failing_node_close)
        instance.relays = [relay]
        instance.services = SimpleNamespace(close=close_fixture)
        try:
            with self.assertRaises(RuntimeError):
                instance.close()
            self.assertIsNotNone(relay.poll(), "relay survived node cleanup failure")
            self.assertIsNotNone(fixture.poll(), "fixture survived node cleanup failure")
        finally:
            for child in (relay, fixture):
                if child.poll() is None:
                    child.kill()
                child.wait()

    def test_cleanup_only_selects_devices_recorded_as_owned_allocations(self):
        self.assertTrue(hasattr(physical, "owned_device_ids"), "owned ublk allocation parser missing")
        log = ("try add_dev requested_dev_id=4294967295\n"
               "device has been added to ublkdrv \x1b[3mdev_id\x1b[0m=27 zero_copy=false\n"
               "restack dev_id=90\n"
               "device has been added to ublkdrv dev_id=28 zero_copy=false\n")
        self.assertEqual(physical.owned_device_ids(log), {27, 28})

    def evidence(self):
        return {
            "source": {"boot_id": "boot-a", "server_sha256": "same", "runtime_uid": 999,
                       "asset_sha256": {"runner_path": "runner", "kernel_path": "kernel", "tools_path": "tools"}},
            "target": {"boot_id": "boot-b", "server_sha256": "same", "runtime_uid": 999,
                       "asset_sha256": {"runner_path": "runner", "kernel_path": "kernel", "tools_path": "tools"}},
            "source_stopped": True, "empty_consumer_cache": True, "fault_status": 500,
            "source_id": "source", "child_id": "child", "source_volume_id": "vol-a",
            "restored_volume_id": "vol-b", "catalog_retained": True,
            "markers": {"ram": "m4-ram", "disk": "m4-disk", "volume": "m4-volume"},
        }

    def test_only_independent_hosts_and_identical_binaries_qualify(self):
        result = self.evidence()
        physical.validate_physical(result)
        for key, value in (("boot_id", "boot-a"), ("server_sha256", "different"), ("runtime_uid", 0)):
            with self.subTest(key=key):
                bad = json.loads(json.dumps(result))
                bad["target"][key] = value
                with self.assertRaises(AssertionError):
                    physical.validate_physical(bad)

    def test_changed_guest_assets_cannot_qualify(self):
        result = self.evidence()
        result["target"]["asset_sha256"]["kernel_path"] = "other-kernel"
        with self.assertRaises(AssertionError):
            physical.validate_physical(result)

    def test_relay_scope_refuses_public_wildcard_and_loopback(self):
        for address in ("0.0.0.0", "127.0.0.1", "1.1.1.1", "203.0.113.1", "::1"):
            with self.subTest(address=address), self.assertRaises(ValueError):
                physical.private_ipv4(address)
        self.assertEqual(physical.private_ipv4("10.20.0.10"), "10.20.0.10")

    def test_failed_ready_closes_real_transport_process(self):
        child = subprocess.Popen([sys.executable, "-c",
            "import sys; print('invalid-json', flush=True); sys.stdin.read()"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            with tempfile.TemporaryDirectory() as directory:
                spec = {"ssh": "unused", "script": "unused", "inputs": "unused", "private_ip": "10.20.0.10"}
                with patch.object(physical.subprocess, "Popen", return_value=child):
                    with self.assertRaises(ValueError):
                        physical.Remote(spec, Path(directory) / "ssh.log")
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.fail("failed worker readiness leaked its transport process")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            for stream in (child.stdin, child.stdout, child.stderr):
                stream.close()

    def test_partial_ready_is_bounded_and_cleans_transport(self):
        child = subprocess.Popen([sys.executable, "-c",
            "import sys; sys.stdout.write('{'); sys.stdout.flush(); sys.stdin.read()"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        errors = []
        with tempfile.TemporaryDirectory() as directory:
            def connect():
                try:
                    spec = {"ssh": "unused", "script": "unused", "inputs": "unused", "private_ip": "10.20.0.10"}
                    with patch.object(physical.subprocess, "Popen", return_value=child), \
                         patch.object(physical, "RESPONSE_TIMEOUT", 0.1, create=True):
                        physical.Remote(spec, Path(directory) / "ssh.log")
                except Exception as error:
                    errors.append(error)
            thread = threading.Thread(target=connect)
            thread.start()
            thread.join(timeout=1)
            stalled = thread.is_alive()
            if stalled:
                child.stdin.close()
                thread.join(timeout=5)
            try:
                self.assertFalse(stalled, "partial response bypassed response deadline")
                self.assertIsInstance(errors[0], TimeoutError)
                self.assertEqual(child.wait(timeout=2), 0)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                for stream in (child.stdin, child.stdout, child.stderr):
                    stream.close()

    def test_source_shutdown_empty_cache_fault_and_catalog_are_required(self):
        for key, value in (("source_stopped", False), ("empty_consumer_cache", False),
                           ("fault_status", 201), ("catalog_retained", False),
                           ("restored_volume_id", "vol-a")):
            with self.subTest(key=key):
                result = self.evidence()
                result[key] = value
                with self.assertRaises(AssertionError):
                    physical.validate_physical(result)

    def test_stream_transfers_binary_data_and_propagates_eof(self):
        payload = bytes(range(256)) * 4096
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            received = []
            def peer():
                with listener.accept()[0] as connection:
                    chunks = []
                    while data := connection.recv(65536):
                        chunks.append(data)
                    received.append(b"".join(chunks))
                    connection.sendall(b"reply\x00" + received[0])
            thread = threading.Thread(target=peer)
            thread.start()
            result = subprocess.run([sys.executable, str(Path(physical.__file__)), "--stream",
                                     "127.0.0.1", str(listener.getsockname()[1])],
                                    input=payload, capture_output=True, timeout=10)
            thread.join(timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(received, [payload])
            self.assertEqual(result.stdout, b"reply\x00" + payload)


@unittest.skipUnless(os.environ.get("AENV_PHYSICAL_TEST_INPUTS"), "explicit Linux worker inputs required")
class WorkerCleanupTests(unittest.TestCase):
    def exercise(self, action):
        child = subprocess.Popen([
            sys.executable, str(Path(physical.__file__)), "--worker", "--inputs",
            os.environ["AENV_PHYSICAL_TEST_INPUTS"],
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready = json.loads(child.stdout.readline())["ready"]
            root = Path(ready["root"])
            group = Path("/sys/fs/cgroup") / root.name
            self.assertTrue(group.exists())
            if action == "cancel":
                child.terminate()
                stdout, stderr = child.communicate(timeout=30)
                self.assertEqual(child.returncode, 143, stderr)
            else:
                stdout, stderr = child.communicate("{truncated" if action == "truncated" else "", timeout=30)
                self.assertEqual(child.returncode == 0, action == "eof", stderr)
            self.assertFalse(group.exists(), (stdout, stderr))
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()

    def test_eof_removes_owned_cgroup(self):
        self.exercise("eof")

    def test_truncated_controller_request_removes_owned_cgroup(self):
        self.exercise("truncated")

    def test_controller_cancellation_removes_owned_cgroup(self):
        self.exercise("cancel")


if __name__ == "__main__":
    unittest.main()
