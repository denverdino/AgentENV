#!/usr/bin/env python3
"""Unprivileged contracts for the bounded real-VM performance runner."""
import importlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


def implementation():
    if not Path(__file__).with_name("dragonball_m4_performance.py").exists():
        raise AssertionError("bounded performance runner missing")
    return importlib.import_module("dragonball_m4_performance")


class PerformanceTests(unittest.TestCase):
    def test_direct_cell_rejects_kernel_mismatch_before_allocating_lab(self):
        from unittest.mock import patch
        m = implementation()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            kernel.write_bytes(b"db kernel")
            other = root / "fc-kernel"
            other.write_bytes(b"different kernel")
            assets = root / "assets.json"
            assets.write_text(json.dumps({"kernel_path": str(kernel)}))
            inputs = {name: str(kernel) for name in
                      ("server", "server_fc", "daemon", "s3", "registry", "ublk_cli", "workload", "firecracker")}
            inputs.update(assets=str(assets), kernel_fc=str(other), deps=str(root))
            with patch.object(m.os, "geteuid", return_value=0), patch.object(m, "Lab") as lab:
                with self.assertRaisesRegex(ValueError, "same kernel"):
                    m.run_cell(root / "unused", inputs, "dragonball",
                               {"memory_mib": 1024, "dirty": "low", "concurrency": 1}, None, root)
                lab.assert_not_called()
                self.assertFalse((root / "unused").exists())

    def test_preflight_refuses_missing_receipts_before_allocating_lab(self):
        from unittest.mock import patch
        m = implementation()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            asset = root / "asset"
            asset.write_text("same bytes")
            manifest = root / "assets.json"
            manifest.write_text(json.dumps({"kernel_path": str(asset)}))
            inputs = {name: str(asset) for name in
                      ("server", "server_fc", "daemon", "s3", "registry", "ublk_cli", "workload", "firecracker", "kernel_fc")}
            inputs.update(assets=str(manifest), deps=str(root))
            with patch.object(m.os, "geteuid", return_value=0), patch.object(m, "Lab") as lab:
                with self.assertRaisesRegex(ValueError, "receipt"):
                    m.run_cell(root / "unused", inputs, "dragonball",
                               {"memory_mib": 1024, "dirty": "low", "concurrency": 1}, None, root)
                lab.assert_not_called()

    def test_capacity_reserves_fixture_memory_before_admission(self):
        m = implementation()
        cell = {"memory_mib": 1024, "dirty": "low", "concurrency": 10}
        # 32 GiB node + 16 GiB fixture + 10 GiB reserve requires 58 GiB.
        for available, expected in ((57, False), (58, True)):
            result = m.capacity(cell, 1, 32 * m.GIB, available * m.GIB,
                                200 * m.GIB, ["capture"], fixture_budget=16 * m.GIB)
            self.assertEqual(result["feasible"], expected)
            self.assertEqual(result["fixture_memory_bytes"], 17179869184)

    def test_capacity_counts_live_children_and_retained_snapshots(self):
        m = implementation()
        cell = {"memory_mib": 8192, "dirty": "high", "concurrency": 100}
        result = m.capacity(cell, 1, 160 * m.GIB, 184 * m.GIB, 85 * m.GIB,
                            ["cold", "capture", "fork_n"])
        self.assertFalse(result["feasible"])
        self.assertIn("node_memory", result["reasons"])
        self.assertIn("retained_disk", result["reasons"])
        result = m.capacity({**cell, "memory_mib": 1024, "concurrency": 1},
                            20, 12 * m.GIB, 184 * m.GIB, 85 * m.GIB, ["cold"])
        self.assertTrue(result["feasible"])

    def test_rejects_unsafe_or_invalid_workload(self):
        m = implementation()
        for bad in (0, -1, 64, 16384):
            with self.assertRaises(ValueError):
                m.workload(bad, "high")
        low, high = m.workload(1024, "low"), m.workload(1024, "high")
        self.assertEqual(low["resident_bytes"], 536870912)
        self.assertEqual(high["resident_bytes"], 536870912)
        self.assertGreater(high["dirty_bytes_per_sweep"], low["dirty_bytes_per_sweep"])
        self.assertEqual(high["dirty_bytes_per_sweep"], 536870912)

    def test_statistics_preserve_cells_failures_and_sample_thresholds(self):
        m = implementation()
        base = {"backend": "dragonball", "operation": "capture", "memory_mib": 1024,
                "dirty": "low", "concurrency": 1, "ok": True, "seconds": 1.0}
        rows = [{**base, "seconds": float(n)} for n in range(1, 21)]
        rows += [{**base, "ok": False, "seconds": 100}, {**base, "memory_mib": 2048}]
        report = m.summarize(rows)
        group = report["groups"]["dragonball/capture/1024/low/1"]
        self.assertEqual(group["failure_rate"], 1 / 21)
        self.assertEqual(group["p95_seconds"], 19)
        self.assertIsNone(group["p99_seconds"])
        self.assertIsNone(report["groups"]["dragonball/capture/2048/low/1"]["p95_seconds"])
        self.assertFalse(report["performance_qualified"])

    def test_statistics_reject_nonfinite_negative_and_incomplete_rows(self):
        m = implementation()
        base = {"backend": "dragonball", "operation": "cold", "memory_mib": 1024,
                "dirty": "low", "concurrency": 1, "ok": True, "seconds": 1}
        for value in (float("nan"), float("inf"), -1):
            with self.assertRaises(ValueError):
                m.summarize([{**base, "seconds": value}])
        with self.assertRaises(ValueError):
            m.summarize([{"ok": True, "seconds": 1}])

    def test_relative_diagnostic_requires_matched_cells_and_enough_successes(self):
        m = implementation()
        base = {"operation": "capture", "memory_mib": 1024, "dirty": "high",
                "concurrency": 1, "ok": True}
        rows = [{**base, "backend": backend, "seconds": duration}
                for backend, duration in (("firecracker", 1.0), ("dragonball", 1.2))
                for _ in range(20)]
        report = m.summarize(rows)
        self.assertIn("relative_diagnostics", report)
        comparison = report["relative_diagnostics"]["capture/1024/high/1"]
        self.assertEqual(comparison["status"], "exceeds_10_percent")
        self.assertAlmostEqual(comparison["dragonball_over_firecracker_p95"], 1.2)
        self.assertFalse(report["performance_qualified"])
        report = m.summarize(rows[:-1])
        self.assertEqual(report["relative_diagnostics"]["capture/1024/high/1"]["status"], "insufficient_successes")

    def test_guest_verification_rejects_stale_or_corrupt_memory(self):
        from unittest.mock import patch
        m = implementation()
        spec = m.workload(1024, "low")
        # Guest HTTP calls are external; exercise validation of their real payload shape.
        with patch.object(m, "guest_file"), patch.object(m, "guest_command", return_value={
                "stdout": json.dumps({"token": "stale", "valid": True,
                    "resident_bytes": spec["resident_bytes"], "checksum": 1,
                    "written_bytes": 100, "elapsed_seconds": 1}), "stderr": "", "exit_code": 0}):
            with self.assertRaisesRegex(RuntimeError, "verification failed"):
                m.verify_guest(None, {}, spec)

    def test_server_config_uses_explicit_common_kernel_version_for_both_backends(self):
        import os
        import configparser
        from types import SimpleNamespace
        from unittest.mock import patch
        import dragonball_m4_scenarios as scenario
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            deps = root / "input-deps"
            for name in ("overlaybd", "regctl"):
                (deps / name).mkdir(parents=True)
            asset = root / "asset"
            asset.write_text("fixture")
            for backend in ("firecracker", "dragonball"):
                node_root = root / backend
                node_root.mkdir()
                identity = {"backend": backend, "firecracker": backend == "firecracker", "dragonball": backend == "dragonball"}
                node = SimpleNamespace(root=node_root, name="a", address="fixture", lab=SimpleNamespace(
                    root=root, account=SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())),
                    run=lambda argv: SimpleNamespace(stdout="999" if argv[0] == "id" else json.dumps(identity)),
                    spawn=lambda *args: SimpleNamespace(poll=lambda: None))
                inputs = {name: str(asset) for name in ("kernel_fc", "server", "server_fc", "firecracker", "daemon")}
                inputs.update(deps=str(deps), kernel_version="vmlinux-6.1.175-common",
                              oss_timeout_seconds=120, request_timeout_seconds=600)
                assets = {name: str(asset) for name in ("kernel_path", "runner_path", "tools_path")}
                with patch.object(scenario, "wait_http"):
                    scenario.start_server(node, inputs, asset, assets, backend)
                config = configparser.ConfigParser()
                config.read_string("[root]\n" + (node_root / "config.toml").read_text())
                self.assertEqual(json.loads(config["kernel"]["version"]), "vmlinux-6.1.175-common")
                self.assertEqual(int(config["backend.oss"]["timeout_secs"]), 120)
                self.assertEqual(node.request_timeout_seconds, 600)

    def test_cleanup_stops_owned_processes_before_device_cleanup_and_on_errors(self):
        from types import SimpleNamespace
        from unittest.mock import patch
        m = implementation()
        self.assertTrue(hasattr(m, "cleanup_cell"), "owned device cleanup is missing")
        for fail, oom in ((False, 0), (True, 0), (False, 1)):
            with self.subTest(device_delete_failure=fail, fixture_oom=oom), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                group = root / "services"
                group.mkdir()
                for name, value in (("memory.max", "2147483648"), ("memory.peak", "2000000000"),
                                    ("memory.events", f"max 3\noom {oom}\noom_kill {oom}\n")):
                    (group / name).write_text(value)
                vm = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
                fixture = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
                def stop(process):
                    process.terminate()
                    process.wait(timeout=5)
                node = SimpleNamespace(close=lambda: stop(vm))
                services = SimpleNamespace(group=group, close=lambda: stop(fixture))
                def devices(node, inputs):
                    self.assertIsNotNone(vm.poll(), "device cleanup ran while VM was live")
                    (root / "receipt").write_text("attempted")
                    if fail:
                        raise RuntimeError("device delete failed")
                try:
                    with patch.object(m, "cleanup_owned_ublk", side_effect=devices):
                        if fail or oom:
                            with self.assertRaisesRegex(RuntimeError, "device delete failed" if fail else "fixture cgroup OOM"):
                                m.cleanup_cell(SimpleNamespace(root=root, nodes=[node]), services, {})
                        else:
                            m.cleanup_cell(SimpleNamespace(root=root, nodes=[node]), services, {})
                    self.assertEqual((root / "receipt").read_text(), "attempted")
                    self.assertIsNotNone(fixture.poll(), "fixture leaked after device cleanup error")
                    metrics = json.loads((root / "fixture-resources.json").read_text())
                    self.assertEqual(metrics["memory_events"]["oom_kill"], oom)
                    self.assertEqual(metrics["memory_peak_bytes"], 2000000000)
                finally:
                    for process in (vm, fixture):
                        if process.poll() is None:
                            stop(process)

    def test_parallel_batch_really_overlaps_and_records_individual_failures(self):
        import threading
        m = implementation()
        barrier = threading.Barrier(3)
        def action(index):
            barrier.wait(timeout=2)
            time.sleep(0.02)
            if index == 1:
                raise RuntimeError("injected")
            return index
        rows = m.parallel_actions(3, action)
        self.assertEqual(sum(row["ok"] for row in rows), 2)
        self.assertIn("injected", rows[1]["error"])
        self.assertTrue(all(row["seconds"] >= 0.02 for row in rows))


@unittest.skipUnless(sys.platform == "linux", "guest C helper executes on Linux; use --linux-workload there")
class WorkloadTests(unittest.TestCase):
    def test_dirty_sweep_cannot_repair_corrupted_restored_memory(self):
        import os
        import signal
        source = Path(__file__).with_name("dragonball_m4_workload.c")
        compiler = shutil.which("cc")
        if not compiler:
            self.skipTest("C compiler unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run([compiler, "-O2", "-Wall", "-Wextra", "-Werror", str(source),
                            "-o", str(root / "workload")], check=True)
            for label, dirty, offset in (("low_prefix", 16384, 4096), ("high", 1048576, 900000)):
                with self.subTest(label=label):
                    state = root / label
                    state.mkdir()
                    (state / "request").write_text("initial")
                    process = subprocess.Popen([str(root / "workload"), "1048576", str(dirty), "20", str(state)])
                    try:
                        deadline = time.monotonic() + 5
                        while not (state / "response").exists() and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertTrue((state / "response").exists(), "initial RAM verification never completed")
                        self.assertTrue(json.loads((state / "response").read_text())["valid"])
                        os.kill(process.pid, signal.SIGSTOP)
                        stopped, status = os.waitpid(process.pid, os.WUNTRACED)
                        self.assertEqual(stopped, process.pid)
                        self.assertTrue(os.WIFSTOPPED(status))
                        address = None
                        with open(f"/proc/{process.pid}/mem", "r+b", buffering=0) as memory:
                            for line in Path(f"/proc/{process.pid}/maps").read_text().splitlines():
                                fields = line.split()
                                start, end = (int(part, 16) for part in fields[0].split("-"))
                                if fields[1] != "rw-p" or len(fields) != 5 or not 1048576 <= end - start <= 8 * 1048576:
                                    continue
                                memory.seek(start)
                                block = memory.read(end - start)
                                position = block.find(bytes(1 + i % 251 for i in range(4096)))
                                if position >= 0:
                                    address = start + position
                                    break
                            self.assertIsNotNone(address, "could not locate owned anonymous workload mapping")
                            memory.seek(address + offset)
                            before = memory.read(1)
                            self.assertEqual(before, bytes([1 + offset % 251]))
                            memory.seek(address + offset)
                            memory.write(bytes([before[0] ^ 255]))
                        (state / "request").write_text("after_corruption")
                        os.kill(process.pid, signal.SIGCONT)
                        try:
                            code = process.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            self.fail("dirty sweep repaired corruption instead of stopping: " +
                                      (state / "response").read_text())
                        self.assertEqual(code, 5)
                        self.assertNotEqual(json.loads((state / "response").read_text())["token"], "after_corruption")
                    finally:
                        if process.poll() is None:
                            os.kill(process.pid, signal.SIGCONT)
                            process.terminate()
                            process.wait(timeout=5)

    def test_guest_helper_rewrites_anonymous_memory_and_answers_fresh_checks(self):
        source = Path(__file__).with_name("dragonball_m4_workload.c")
        self.assertTrue(source.exists(), "anonymous RAM workload helper missing")
        compiler = shutil.which("cc")
        if not compiler:
            self.skipTest("C compiler unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run([compiler, "-O2", "-Wall", "-Wextra", "-Werror", str(source),
                            "-o", str(root / "workload")], check=True)
            process = subprocess.Popen([str(root / "workload"), "1048576", "524288", "20", directory])
            try:
                for token in ("first", "second"):
                    (root / "request").write_text(token)
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        if (root / "response").exists():
                            response = json.loads((root / "response").read_text())
                            if response["token"] == token:
                                break
                        time.sleep(0.02)
                    else:
                        self.fail(f"workload did not answer {token}: exit={process.poll()}, "
                                  f"files={list(root.iterdir())}, response=" +
                                  ((root / "response").read_text() if (root / "response").exists() else "missing"))
                    self.assertTrue(response["valid"])
                    self.assertEqual(response["resident_bytes"], 1048576)
                    self.assertGreaterEqual(response["written_bytes"], 524288)
                    self.assertGreater(response["checksum"], 0)
            finally:
                process.terminate()
                process.wait(timeout=5)


def vm_cleanup_audit():
    """Opt-in: fail a two-request batch after one real cold VM is running."""
    import argparse
    import os
    import signal
    import threading
    from types import SimpleNamespace
    from unittest.mock import patch
    from dragonball_m4_cleanup import owned_device_ids
    from dragonball_m4_scenarios import isolated_controller, terminate_controller, validate_inputs
    parser = argparse.ArgumentParser()
    parser.add_argument("--linux-vm-cleanup", action="store_true")
    parser.add_argument("--inside-vm-cleanup", action="store_true")
    parser.add_argument("--inputs", type=Path, required=True)
    args = parser.parse_args()
    inputs = json.loads(args.inputs.read_text())
    validate_inputs(inputs, "rollback", os.geteuid())
    for value in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(value, terminate_controller)
    if not args.inside_vm_cleanup:
        env = {**os.environ, "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net")}
        return isolated_controller(["unshare", "--mount", "--net", "--propagation", "private",
            sys.executable, str(Path(__file__).resolve()), *sys.argv[1:], "--inside-vm-cleanup"], env)
    m = implementation()
    real_api = m.api
    first_done = threading.Event()
    lock = threading.Lock()
    calls = 0
    def partial_create(*args, **kwargs):
        nonlocal calls
        if args[1] == "/sandboxes-cold":
            with lock:
                calls += 1
                index = calls
            if index == 1:
                try:
                    return real_api(*args, **kwargs)
                finally:
                    first_done.set()
            if not first_done.wait(timeout=180):
                raise TimeoutError("first real cold create never completed")
            raise RuntimeError("injected second cold request failure after first allocation")
        return real_api(*args, **kwargs)
    output = Path(tempfile.mkdtemp(prefix="db-m4-performance-audit.", dir="/var/tmp"))
    root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
    print("M4 partial cold cleanup audit root: " + str(root), flush=True)
    with patch.object(m, "api", side_effect=partial_create):
        result = m.run_cell(root, inputs, "dragonball", {"memory_mib": 1024, "dirty": "low", "concurrency": 2},
            SimpleNamespace(repeats=1, operations=["cold"], node_memory_gib=12, node_cpus=4, fixture_memory_gib=16), output)
    rows = result["samples"]
    assert len(rows) == 2 and sum(row["ok"] for row in rows) == 1, result
    assert "batch contains failed samples" in result.get("error", ""), result
    assert result["identity"]["runtime_uid"] != 0, result
    owned = owned_device_ids((root / "a/home/logs/ublk-daemon.log").read_text())
    assert owned, "audit did not allocate a real ublk device"
    assert not (Path("/sys/fs/cgroup") / root.name).exists(), "owned cgroup leaked"
    assert all(not Path(f"/dev/ublkc{dev_id}").exists() for dev_id in owned), "owned ublk device leaked"
    receipt = json.loads((root / "ublk-cleanup.json").read_text())
    assert receipt and all(item["exit_code"] == 0 for item in receipt), receipt
    report = {"root": str(root), "output": str(output), "owned_device_ids": sorted(owned),
              "cleanup_receipt": receipt, "partial_cold_failure_cleanup": "passed",
              "performance_qualified": False}
    (output / "audit.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report), flush=True)
    return 0


if __name__ == "__main__":
    if "--linux-vm-cleanup" in sys.argv:
        raise SystemExit(vm_cleanup_audit())
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(
        WorkloadTests if "--linux-workload" in sys.argv else PerformanceTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
