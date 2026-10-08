#!/usr/bin/env python3
"""Behavioral tests; --linux explicitly enables the privileged namespace suite."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

MODULE = Path(__file__).with_name("dragonball_m4_lab.py")


def implementation():
    if not MODULE.exists():
        raise AssertionError("M4 isolated lab implementation is missing")
    spec = importlib.util.spec_from_file_location("m4_lab", MODULE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ContractTests(unittest.TestCase):
    def test_missing_controllers_refused(self):
        # Removing the resource prerequisite check must not create an uncapped lab.
        module = implementation()
        with self.assertRaisesRegex(RuntimeError, "memory"):
            module.require_controllers("cpu pids")
        module.require_controllers("cpu memory pids")

    def test_nonprivate_network_refused(self):
        module = implementation()
        with tempfile.TemporaryDirectory(prefix="db-m4.") as directory:
            with self.assertRaisesRegex(RuntimeError, "private network"):
                module.Lab(Path(directory), "aenv")
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_preflight_refusal_holds_lock_without_allocating_and_releases_it(self):
        from types import SimpleNamespace
        from unittest.mock import patch
        import dragonball_m4_preflight as preflight
        module = implementation()
        real_check = preflight.check
        real_lock = module.acquire_host_lock
        real_exists, real_read, real_resolve = Path.exists, Path.read_text, Path.resolve
        real_readlink = os.readlink
        with tempfile.TemporaryDirectory(prefix="db-m4.", dir="/var/tmp") as directory, \
                tempfile.TemporaryDirectory() as locks:
            root = Path(directory)
            lock_path = Path(locks) / "host.lock"

            def check(profile):
                self.assertEqual(list(root.iterdir()), [])
                self.assertFalse((Path("/sys/fs/cgroup") / root.name).exists())
                with self.assertRaisesRegex(RuntimeError, "busy"):
                    real_lock(lock_path)
                return real_check(profile)

            with patch.dict(os.environ, {"AENV_M4_PARENT_NET": "net:[1]"}), \
                    patch.object(module.os, "geteuid", return_value=0), \
                    patch.object(module.os, "readlink", side_effect=lambda p: "net:[2]" if str(p) == "/proc/self/ns/net" else real_readlink(p)), \
                    patch.object(Path, "resolve", lambda p, **kw: p if p == root else real_resolve(p, **kw)), \
                    patch.object(module.pwd, "getpwnam", return_value=SimpleNamespace(pw_uid=1000, pw_gid=1000)), \
                    patch.object(Path, "exists", lambda p: True if str(p) == "/proc/self/ns/net" else real_exists(p)), \
                    patch.object(Path, "read_text", lambda p, **kw: "cpu memory pids" if str(p) == "/sys/fs/cgroup/cgroup.subtree_control" else real_read(p, **kw)), \
                    patch.object(module, "acquire_host_lock", side_effect=lambda: real_lock(lock_path)), \
                    patch.object(module, "require_clean_host"), \
                    patch.object(preflight, "check", side_effect=check) as checked:
                with self.assertRaisesRegex(RuntimeError, "preflight"):
                    module.Lab(root, "aenv", preflight_profile={"unknown": True})
            checked.assert_called_once()
            self.assertEqual(list(root.iterdir()), [])
            fd = real_lock(lock_path)
            os.close(fd)


class ReadinessTests(unittest.TestCase):
    # These fixtures exercise the actual pipe reader and subprocess lifecycle,
    # without requiring mount/network privileges on the developer's machine.
    message = {"pid": 1, "namespaces": {"net": "net:[101]", "mnt": "mnt:[102]", "pid": "pid:[103]"}}

    @contextmanager
    def producer(self, payload, mode="normal"):
        read_fd, write_fd = os.pipe()
        child = subprocess.Popen([sys.executable, "-c", """
import os, sys, time
fd, mode, payload = int(sys.argv[1]), sys.argv[2], bytes.fromhex(sys.argv[3])
if mode in ('exit', 'exit-held-writer'):
    sys.exit(23)
if mode == 'never':
    time.sleep(60)
if mode in ('fragmented', 'trickle'):
    for byte in payload:
        os.write(fd, bytes([byte]))
        time.sleep(0.002 if mode == 'fragmented' else 0.05)
else:
    os.write(fd, payload)
os.close(fd)
time.sleep(60)
""", str(write_fd), mode, payload.hex()], pass_fds=(write_fd,))
        if mode != "exit-held-writer":
            os.close(write_fd)
        try:
            yield child, read_fd
        finally:
            if mode == "exit-held-writer":
                os.close(write_fd)
            os.close(read_fd)
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
        self.assertIsNotNone(child.returncode, "fixture process leaked")

    def receive(self, payload=None, mode="normal", timeout=1):
        module = implementation()
        self.assertTrue(hasattr(module, "_wait_node_ready"), "bounded pipe readiness protocol missing")
        if payload is None:
            payload = (json.dumps(self.message) + "\n").encode()
        with self.producer(payload, mode) as (child, fd):
            return module._wait_node_ready(child, fd, time.monotonic() + timeout)

    def test_normal_and_fragmented_readiness(self):
        for mode in ("normal", "fragmented"):
            with self.subTest(mode=mode):
                self.assertEqual(self.receive(mode=mode), self.message)

    def test_never_ready_and_trickle_share_total_deadline(self):
        for mode in ("never", "trickle"):
            with self.subTest(mode=mode):
                started = time.monotonic()
                with self.assertRaisesRegex(TimeoutError, "readiness"):
                    self.receive(mode=mode, timeout=0.3)
                self.assertLess(time.monotonic() - started, 1.5)

    def test_early_exit_fails_before_deadline(self):
        for mode in ("exit", "exit-held-writer"):
            with self.subTest(mode=mode):
                started = time.monotonic()
                with self.assertRaisesRegex(RuntimeError, "exited|closed"):
                    self.receive(mode=mode, timeout=5)
                self.assertLess(time.monotonic() - started, 1.5)

    def test_malformed_oversized_and_partial_messages_are_refused(self):
        payloads = [b"not json\n", b"x" * 4097, b'{"pid":', b"[]\n",
                    b"{}\n", b"\xff\n", b"[" * 1100 + b"]" * 1100 + b"\n",
                    (json.dumps(self.message).replace('"pid": 1', '"pid": 1, "pid": 1') + "\n").encode(),
                    (json.dumps(self.message) + "\n{}\n").encode()]
        for payload in payloads:
            with self.subTest(payload=payload[:40]):
                with self.assertRaisesRegex(RuntimeError, "readiness"):
                    self.receive(payload)

    def test_decoder_recursion_failure_has_readiness_context(self):
        from unittest.mock import patch
        module = implementation()
        # Python decoder stack limits differ by version; exercise the failure
        # explicitly while retaining the real subprocess/pipe transport.
        with self.producer(b"[[[]]]\n") as (child, fd):
            with patch.object(module.json, "loads", side_effect=RecursionError("decoder stack exhausted")):
                with self.assertRaisesRegex(RuntimeError, "node readiness: malformed JSON"):
                    module._wait_node_ready(child, fd, time.monotonic() + 1)

    def test_pid_and_namespace_schema_is_strict(self):
        invalid = [dict(self.message, pid=value) for value in (True, 0, 2, "1", None)]
        invalid += [dict(self.message, extra=True), {"pid": 1, "namespaces": []}]
        for value in ("net:[0]", "net:[abc]", "mnt:[101]", None):
            invalid.append({"pid": 1, "namespaces": dict(self.message["namespaces"], net=value)})
        invalid.append({"pid": 1, "namespaces": {"net": "net:[101]"}})
        for message in invalid:
            with self.subTest(message=message):
                with self.assertRaisesRegex(RuntimeError, "readiness"):
                    self.receive((json.dumps(message) + "\n").encode())

    def test_serial_and_concurrent_readiness_failures(self):
        def trial(mode):
            if mode in ("never", "trickle"):
                with self.assertRaises(TimeoutError):
                    self.receive(mode=mode, timeout=0.3)
            elif mode == "exit":
                with self.assertRaises(RuntimeError):
                    self.receive(mode=mode)
            else:
                self.assertEqual(self.receive(mode=mode), self.message)
        modes = ["normal", "fragmented", "never", "trickle", "exit"]
        for mode in modes:
            trial(mode)
        with ThreadPoolExecutor(max_workers=4) as executor:
            list(executor.map(trial, modes * 3))


@unittest.skipUnless("--inside" in sys.argv, "explicit --linux namespace qualification required")
class LinuxTests(unittest.TestCase):
    def test_receipt_start_failure_cleans_cgroup_and_releases_host(self):
        from unittest.mock import patch
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix='db-m4.', dir='/var/tmp'))
        group = Path('/sys/fs/cgroup') / root.name
        with patch.object(module.RunRecord, '__enter__', side_effect=OSError('injected receipt write')):
            with self.assertRaisesRegex(OSError, 'injected receipt write'):
                module.Lab(root, 'aenv')
        self.assertFalse(group.exists(), 'receipt failure leaked owned cgroup')
        fd = module.acquire_host_lock()
        os.close(fd)

    def test_receipt_finish_failure_releases_host_after_resource_cleanup(self):
        from unittest.mock import patch
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix='db-m4.', dir='/var/tmp'))
        lab = module.Lab(root, 'aenv')
        with patch.object(module.RunRecord, '__exit__', side_effect=OSError('injected final receipt')):
            with self.assertRaisesRegex(OSError, 'injected final receipt'):
                lab.close()
        self.assertFalse(lab.cgroup.exists(), 'receipt failure leaked owned cgroup')
        fd = module.acquire_host_lock()
        os.close(fd)

    def test_preflight_refuses_under_host_lock_before_lab_allocation(self):
        from unittest.mock import patch
        import dragonball_m4_preflight as preflight
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        real_check = preflight.check

        def check(profile):
            self.assertEqual(list(root.iterdir()), [])
            self.assertFalse((Path("/sys/fs/cgroup") / root.name).exists())
            with self.assertRaisesRegex(RuntimeError, "busy"):
                module.acquire_host_lock()
            return real_check(profile)

        with patch.object(preflight, "check", side_effect=check) as checked:
            with self.assertRaisesRegex(RuntimeError, "preflight"):
                module.Lab(root, "aenv", preflight_profile={"unknown": True})
        checked.assert_called_once()
        self.assertEqual(list(root.iterdir()), [])
        self.assertFalse((Path("/sys/fs/cgroup") / root.name).exists())
        fd = module.acquire_host_lock()
        os.close(fd)

    def test_startup_protocol_failures_remain_cleanable(self):
        from unittest.mock import patch
        module = implementation()
        real_popen = subprocess.Popen
        for mode in ("never", "exit", "malformed"):
            with self.subTest(mode=mode):
                root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
                children = []

                def producer(argv, **kwargs):
                    fd = kwargs["pass_fds"][0]
                    child = real_popen([sys.executable, "-c", r"""
import os, sys, time
fd, mode = int(sys.argv[1]), sys.argv[2]
if mode == 'exit':
    sys.exit(23)
if mode == 'malformed':
    os.write(fd, b'not JSON\n')
time.sleep(60)
""", str(fd), mode], **kwargs)
                    children.append(child)
                    return child

                expected = TimeoutError if mode == "never" else RuntimeError
                with module.Lab(root, "aenv") as lab:
                    # A stale evidence file must never make a new process ready.
                    (root / "a" / "ready").write_text('{"stale":true}')
                    with patch.object(module.subprocess, "Popen", side_effect=producer), \
                            patch.object(module, "_NODE_READY_TIMEOUT", 0.3):
                        started = time.monotonic()
                        with self.assertRaises(expected):
                            lab.start_node("a")
                        self.assertLess(time.monotonic() - started, 1.5)
                    self.assertEqual(len(lab.nodes), 1, "failed node lost cleanup ownership")
                self.assertFalse(lab.cgroup.exists())
                self.assertTrue(all(child.poll() is not None for child in children))
                self.assertEqual((root / "a" / "ready").read_text(), '{"stale":true}')
                self.assertTrue((root / "a.init.log").exists())

    def test_namespace_identity_mismatch_is_refused_and_cleaned(self):
        from unittest.mock import patch
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        real_wait = module._wait_node_ready

        def wrong_identity(*args):
            message = real_wait(*args)
            message["namespaces"]["net"] = os.readlink("/proc/self/ns/net")
            return message

        with module.Lab(root, "aenv") as lab:
            with patch.object(module, "_wait_node_ready", side_effect=wrong_identity):
                with self.assertRaisesRegex(RuntimeError, "namespace identity mismatch"):
                    lab.start_node("a")
        self.assertFalse(lab.cgroup.exists())
        self.assertIsNotNone(lab.nodes[0].init.poll())

    def test_private_tempdir_allows_same_mount_rename_and_masks_peer_paths(self):
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        with module.Lab(root, "aenv") as lab:
            a, b = lab.start_node("a"), lab.start_node("b")
            try:
                result = a.run(["python3", "-c", """
import json, os, sys, tempfile
from pathlib import Path
root = Path(sys.argv[1])
source = root / 'rename-source'
source.write_text('sealed-upper')
target = Path(tempfile.mkdtemp(prefix='rename-check-')) / 'sealed'
os.rename(source, target)
Path('/tmp/m4-private-rename-marker').write_text('node-a')
print(json.dumps({'target': str(target), 'contents': target.read_text()}))
""", str(a.root)])
            except subprocess.CalledProcessError as error:
                self.fail("same-mount rename failed: " + error.stderr)
            output = json.loads(result.stdout)
            self.assertEqual(output["contents"], "sealed-upper")
            self.assertTrue(Path(output["target"]).is_relative_to(a.root / "tmp"))
            b.run(["python3", "-c", """
import sys
from pathlib import Path
assert not Path('/tmp/m4-private-rename-marker').exists()
try:
    Path(sys.argv[1]).read_text()
except PermissionError:
    pass
else:
    raise AssertionError('peer canonical temporary directory was accessible')
""", output["target"]])
        self.assertFalse(lab.cgroup.exists())

    def test_link_cleanup_tolerates_namespace_disappearance(self):
        from unittest.mock import patch
        module = implementation()
        self.assertTrue(hasattr(module, "delete_link"), "race-aware link cleanup missing")
        module.command(["ip", "link", "add", "m4-race", "type", "dummy"])
        real_command = module.command
        def disappearing_link(argv, **kwargs):
            if argv == ["ip", "link", "delete", "m4-race"]:
                real_command(argv)
            return real_command(argv, **kwargs)
        with patch.object(module, "command", side_effect=disappearing_link):
            module.delete_link("m4-race")
        self.assertNotEqual(subprocess.run(["ip", "link", "show", "m4-race"], capture_output=True).returncode, 0)

    def test_isolation_network_and_cleanup(self):
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        cgroups = []
        with module.Lab(root, "aenv") as lab:
            a, b = lab.start_node("a"), lab.start_node("b")
            cgroups = [a.cgroup, b.cgroup, lab.cgroup]
            (a.root / "private-marker").write_text("source-only")
            probes = []
            for node in (a, b):
                probe = node.run(["python3", "-c", """
import json, os
print(json.dumps({'uid': os.getuid(), 'ns': {
    n: os.readlink('/proc/self/ns/' + n) for n in ('net','mnt','pid')},
    'init': open('/proc/1/cmdline','rb').read().decode()}))
"""])
                probes.append(json.loads(probe.stdout))
                self.assertNotEqual(probes[-1]["uid"], 0)
                self.assertIn("--node-init", probes[-1]["init"])
                self.assertEqual(json.loads((node.root / "ready").read_text()), probes[-1]["ns"])
                self.assertFalse((node.root / ".ready.tmp").exists())
                self.assertEqual((node.cgroup / "memory.max").read_text().strip(), "12884901888")
                self.assertEqual((node.cgroup / "cpu.max").read_text().strip(), "400000 100000")
                self.assertEqual((node.cgroup / "memory.swap.max").read_text().strip(), "0")
            for namespace in ("net", "mnt", "pid"):
                self.assertNotEqual(probes[0]["ns"][namespace], probes[1]["ns"][namespace])
                self.assertNotEqual(probes[0]["ns"][namespace], os.readlink("/proc/self/ns/" + namespace))
            self.assertEqual(b.run(["test", "!", "-e", str(a.root / "private-marker")]).returncode, 0)
            # Snapshot helpers may use /tmp instead of configured home/run.
            temporary_marker = "/tmp/" + root.name
            a.run(["python3", "-c", f"from pathlib import Path; Path({temporary_marker!r}).write_text('node-a')"])
            try:
                b.run(["test", "!", "-e", temporary_marker])
            finally:
                a.run(["python3", "-c", f"from pathlib import Path; Path({temporary_marker!r}).unlink()"])
            servers = []
            for node in (a, b):
                servers.append(node.spawn(["python3", "-m", "http.server", "8099", "--bind", "0.0.0.0", "--directory", str(node.root)], root / (node.name + ".http.log")))
                module.wait_http(f"http://{node.address}:8099/", timeout=10)
            self.assertIn("source-only", b.run(["python3", "-c", f"import urllib.request; print(urllib.request.urlopen('http://{a.address}:8099/private-marker', timeout=2).read().decode())"]).stdout)
            b.disconnect()
            with self.assertRaises(subprocess.CalledProcessError):
                b.run(["python3", "-c", f"import urllib.request; urllib.request.urlopen('http://{a.address}:8099/', timeout=1)"], timeout=5)
            b.reconnect()
            self.assertEqual(b.run(["python3", "-c", f"import urllib.request; urllib.request.urlopen('http://{a.address}:8099/', timeout=2)"]).returncode, 0)
            a.close()
            b.disconnect()
            with self.assertRaises(subprocess.CalledProcessError):
                b.run(["python3", "-c", "import socket; socket.create_connection(('10.241.0.1', 9000), timeout=4)"])
            b.reconnect()
            try:
                module.wait_http(f"http://{b.address}:8099/", timeout=10)
            except TimeoutError:
                print(module.command(["bridge", "-j", "link"]).stdout)
                print(module.command(["ip", "-j", "neigh"]).stdout)
                raise
            # A timeout must kill grandchildren, not just nsenter's direct process.
            with self.assertRaises(subprocess.TimeoutExpired):
                b.run(["sh", "-c", "sleep 600 & wait"], timeout=1)
        for group in cgroups:
            self.assertFalse(group.exists(), str(group))
        for server in servers:
            self.assertIsNotNone(server.poll())
        self.assertTrue((root / "a.http.log").exists(), "retain diagnostics")
        print(json.dumps({"lab_root": str(root), "isolation": probes, "cleanup": "passed"}))

    def test_exception_cleanup(self):
        module = implementation()
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        group = None
        with self.assertRaisesRegex(RuntimeError, "injected"):
            with module.Lab(root, "aenv") as lab:
                group = lab.cgroup
                lab.start_node("a").spawn(["sleep", "600"], root / "sleep.log")
                raise RuntimeError("injected")
        self.assertFalse(group.exists())


if __name__ == "__main__":
    if "--linux" in sys.argv:
        if os.geteuid() != 0:
            raise SystemExit("--linux requires root orchestration (workloads remain aenv)")
        env = dict(os.environ, AENV_M4_PARENT_NET=os.readlink("/proc/self/ns/net"))
        raise SystemExit(subprocess.call(["unshare", "--mount", "--net", "--propagation", "private", sys.executable, __file__, "--inside"], env=env))
    suite = (unittest.defaultTestLoader.loadTestsFromTestCase(LinuxTests) if "--inside" in sys.argv
             else unittest.TestSuite(unittest.defaultTestLoader.loadTestsFromTestCase(case)
                                     for case in (ContractTests, ReadinessTests)))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
