#!/usr/bin/env python3
"""Opt-in SIGKILL qualification; real namespaces/cgroups, non-root workloads."""
import json
import os
from pathlib import Path
import pwd
import signal
import subprocess
import sys
import tempfile
import time
import unittest


@unittest.skipUnless("--linux" in sys.argv, "explicit --linux crash recovery qualification required")
class RecoveryLinuxTests(unittest.TestCase):
    def test_sigkill_audit_recovery_and_external_namespace_reference(self):
        from dragonball_m4_lab import empty_cgroup
        root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
        group = Path("/sys/fs/cgroup") / root.name
        scripts = Path(__file__).resolve().parent
        worker = """
import sys, time
from pathlib import Path
from dragonball_m4_lab import Lab
from dragonball_m4_scenarios import Services
root = Path(sys.argv[1])
with Lab(root, 'aenv') as lab:
    for name in ('a', 'b'):
        lab.start_node(name).spawn(['sh', '-c', 'sleep 600 & wait'], root / (name+'.worker.log'))
    services = Services(lab)
    services.spawn('service', ['sleep', '600'])
    (root / 'retain.txt').write_text('failure evidence')
    (root / 'controller-ready').touch()
    time.sleep(600)
"""
        env = {**os.environ, "PYTHONPATH": str(scripts),
               "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net")}
        with tempfile.NamedTemporaryFile(prefix="m4-recovery-controller-", suffix=".log", delete=False) as log:
            controller_log = Path(log.name)
            controller = subprocess.Popen(["unshare", "--mount", "--net", "--propagation", "private",
                                           sys.executable, "-c", worker, str(root)],
                                          env=env, stdout=log, stderr=subprocess.STDOUT)
        namespace = None
        pin = None
        pinned = False
        root_fd = None
        def cli(*extra):
            return subprocess.run([sys.executable, str(scripts / "dragonball_m4_recovery.py"),
                                   "--root", str(root), *extra], text=True, capture_output=True, timeout=30)
        try:
            deadline = time.monotonic() + 30
            while not (root / "controller-ready").exists():
                self.assertIsNone(controller.poll(), controller_log.read_text())
                self.assertLess(time.monotonic(), deadline, "controller readiness timed out")
                time.sleep(0.05)
            pids = {int(pid) for file in group.rglob("cgroup.procs") for pid in file.read_text().split()}
            uid = pwd.getpwnam("aenv").pw_uid
            self.assertNotEqual(uid, 0)
            self.assertTrue(any(f"Uid:\t{uid}\t" in Path(f"/proc/{pid}/status").read_text() for pid in pids))
            active = cli("--apply")
            self.assertNotEqual(active.returncode, 0)
            self.assertIn("test host is busy", active.stderr)
            namespace = os.open(f"/proc/{controller.pid}/ns/net", os.O_RDONLY)
            with tempfile.NamedTemporaryFile(prefix="m4-recovery-ns-", dir="/var/tmp", delete=False) as file:
                pin = Path(file.name)
            subprocess.run(["mount", "--bind", f"/proc/{controller.pid}/ns/net", str(pin)], check=True)
            pinned = True
            controller.send_signal(signal.SIGKILL)
            controller.wait(timeout=10)
            audit = cli()
            self.assertEqual(json.loads(audit.stdout)["status"], "orphaned", audit.stderr)
            self.assertTrue(group.exists(), "audit mutated the abandoned cgroup")
            blocked = subprocess.run([sys.executable, str(scripts / "dragonball_m4_host.py"), "true"],
                                     text=True, capture_output=True, timeout=10)
            self.assertNotEqual(blocked.returncode, 0)
            self.assertIn("audit before starting", blocked.stderr)
            recovered = cli("--apply")
            report = json.loads(recovered.stdout)
            self.assertEqual(report["status"], "manual_review", recovered.stderr)
            self.assertFalse(group.exists())
            self.assertTrue(any(row["pid"] == os.getpid() for row in report["references"]))
            os.close(namespace)
            namespace = None
            mounted = cli("--apply")
            self.assertEqual(json.loads(mounted.stdout)["status"], "manual_review", mounted.stderr)
            subprocess.run(["umount", str(pin)], check=True)
            pinned = False
            root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            held = cli("--apply")
            self.assertEqual(json.loads(held.stdout)["status"], "manual_review", held.stderr)
            os.close(root_fd)
            root_fd = None
            for _ in range(2):
                result = cli("--apply")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(json.loads(result.stdout)["status"], "clean")
            self.assertEqual((root / "retain.txt").read_text(), "failure evidence")
            self.assertEqual(len(list(root.glob("recovery-*.json"))), 5)
            print(json.dumps({"root": str(root), "controller_log": str(controller_log), "SIGKILL_recovery": "passed",
                              "external_namespace_refusal": "passed", "namespace_bind_refusal": "passed",
                              "root_directory_fd_refusal": "passed", "workload_pids": sorted(pids)}))
        finally:
            if root_fd is not None:
                os.close(root_fd)
            if namespace is not None:
                os.close(namespace)
            if pinned:
                subprocess.run(["umount", str(pin)], check=True)
            if pin is not None:
                pin.unlink(missing_ok=True)
            if controller.poll() is None:
                controller.kill()
            controller.wait(timeout=10)
            if group.exists():
                empty_cgroup(group)
                for directory in sorted((p for p in group.rglob("*") if p.is_dir()),
                                        key=lambda p: len(p.parts), reverse=True):
                    directory.rmdir()
                group.rmdir()


if __name__ == "__main__":
    if "--linux" not in sys.argv or os.geteuid() != 0:
        raise SystemExit("use --linux as root; workloads remain aenv")
    if "--inside" not in sys.argv:
        raise SystemExit(subprocess.call(["unshare", "--mount", "--propagation", "private",
                                         sys.executable, __file__, "--linux", "--inside"]))
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(RecoveryLinuxTests))
    raise SystemExit(not result.wasSuccessful())
