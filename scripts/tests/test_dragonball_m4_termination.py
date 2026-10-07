#!/usr/bin/env python3
"""Explicit Linux CLI cancellation regression with real fixture/node processes."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

from dragonball_m4_lab import empty_cgroup


@unittest.skipUnless(__name__ == "__main__", "run directly with --inputs for real controller termination")
class TerminationTests(unittest.TestCase):
    def exercise(self, target):
        script = Path(__file__).with_name("dragonball_m4_scenarios.py")
        group = None
        with tempfile.NamedTemporaryFile(prefix="m4-termination-", suffix=".log", delete=False) as output:
            log = Path(output.name)
            child = subprocess.Popen([sys.executable, str(script), "--inputs", str(INPUTS)],
                                     stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                lines = log.read_text().splitlines()
                roots = [Path(line.removeprefix("M4 lab root: ")) for line in lines if line.startswith("M4 lab root: ")]
                if roots:
                    root = roots[0]
                    group = Path("/sys/fs/cgroup") / root.name
                    if all((root / name / "ready").exists() for name in ("a", "b")):
                        break
                self.assertIsNone(child.poll(), log.read_text())
                time.sleep(0.05)
            else:
                self.fail("nodes did not become ready: " + str(log))
            pids = {int(pid) for file in group.rglob("cgroup.procs") for pid in file.read_text().split()}
            self.assertTrue(pids)
            victim = child.pid
            if target == "inner":
                children = Path(f"/proc/{child.pid}/task/{child.pid}/children").read_text().split()
                self.assertEqual(len(children), 1)
                victim = int(children[0])
            os.kill(victim, signal.SIGTERM)
            self.assertNotEqual(child.wait(timeout=90), 0)
            deadline = time.monotonic() + 20
            while group.exists() and time.monotonic() < deadline:
                time.sleep(0.1)
            self.assertFalse(group.exists(), f"{target} SIGTERM leaked {group}; log {log}")
            self.assertFalse([pid for pid in pids if Path(f"/proc/{pid}").exists()])
            print(f"{target} SIGTERM cleanup passed: {root}; log {log}", flush=True)
        finally:
            # RED must not itself leave the deliberately orphaned test workloads.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=10)
            if group is not None and group.exists():
                empty_cgroup(group)
                for nested in sorted((p for p in group.rglob("*") if p.is_dir()), reverse=True):
                    if nested.exists():
                        nested.rmdir()
                if group.exists():
                    group.rmdir()

    def test_outer_controller_termination(self):
        self.exercise("outer")

    def test_inner_controller_termination(self):
        self.exercise("inner")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--inputs", type=Path, required=True)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("root orchestration required")
    INPUTS = args.inputs.resolve(strict=True)
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(TerminationTests))
    raise SystemExit(not result.wasSuccessful())
