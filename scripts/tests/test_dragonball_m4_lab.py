#!/usr/bin/env python3
"""Behavioral tests; --linux explicitly enables the privileged namespace suite."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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


@unittest.skipUnless("--inside" in sys.argv, "explicit --linux namespace qualification required")
class LinuxTests(unittest.TestCase):
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
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(LinuxTests if "--inside" in sys.argv else ContractTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
