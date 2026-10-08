"""Recovery must prove ownership before changing a crashed lab's resources."""
import importlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        path = Path(__file__).with_name("dragonball_m4_recovery.py")
        self.assertTrue(path.exists(), "persistent lab ownership and recovery are missing")
        self.m = importlib.import_module("dragonball_m4_recovery")
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        for name in ("runs", "cgroups", "proc", "dev"):
            (self.base / name).mkdir()
        self.root = self.base / "runs/db-m4.test"
        self.root.mkdir()
        self.group = self.base / "cgroups/db-m4.test"
        self.group.mkdir()
        (self.group / "cgroup.procs").write_text("")
        (self.base / "proc/sys/kernel/random").mkdir(parents=True)
        (self.base / "proc/sys/kernel/random/boot_id").write_text("boot-one")
        for key, value in {"RUNS": self.base / "runs", "CGROUPS": self.base / "cgroups",
                           "PROC": self.base / "proc", "DEV": self.base / "dev",
                           "OWNER_UID": os.getuid()}.items():
            context = patch.object(self.m, key, value)
            context.start()
            self.addCleanup(context.stop)
        self.manifest = {"schema": 1, "root": str(self.root), "boot_id": "boot-one",
                         "root_identity": self.m.file_identity(self.root),
                         "cgroup_identity": self.m.file_identity(self.group),
                         "controller": {"pid": 123, "start_time": "100"},
                         "namespaces": [], "state": "running", "run_user": "aenv"}
        self.m.save_manifest(self.root, self.manifest)
        (self.root / "keep.log").write_text("failure evidence")

    def controller(self, start_time="100"):
        proc = self.base / "proc/123"
        proc.mkdir()
        # /proc/stat field 22; comm may contain spaces and parentheses.
        (proc / "stat").write_text("123 (controller (test)) " + " ".join(["S"] + ["0"] * 18 + [start_time]))

    def test_audit_does_not_mutate_or_remove_orphan_resources(self):
        before = {p.name: p.read_bytes() for p in self.root.iterdir()}
        report = self.m.recover(self.root)
        self.assertEqual(report["status"], "orphaned")
        self.assertTrue(self.group.exists())
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.root.iterdir()})

    def test_live_controller_is_refused_even_if_lock_is_available(self):
        self.controller()
        with patch.object(self.m, "remove_cgroup_tree") as remove:
            report = self.m.recover(self.root, apply=True)
        self.assertEqual(report["status"], "active")
        remove.assert_not_called()

    def test_reused_pid_is_not_signalled_and_recovery_is_repeatable(self):
        self.controller("200")
        with patch.object(self.m, "remove_cgroup_tree", side_effect=shutil.rmtree) as remove:
            first = self.m.recover(self.root, apply=True)
            second = self.m.recover(self.root, apply=True)
        self.assertEqual((first["status"], second["status"]), ("clean", "clean"))
        remove.assert_called_once_with(self.group)
        self.assertTrue((self.base / "proc/123/stat").exists())
        self.assertEqual((self.root / "keep.log").read_text(), "failure evidence")
        self.assertEqual(len(list(self.root.glob("recovery-*.json"))), 2)

    def test_boot_or_cgroup_identity_mismatch_never_kills(self):
        for field in ("boot_id", "cgroup_identity", "root_identity"):
            original = self.manifest[field]
            self.manifest[field] = "wrong" if field == "boot_id" else [0, 0]
            self.m.save_manifest(self.root, self.manifest)
            with patch.object(self.m, "remove_cgroup_tree") as remove:
                with self.assertRaisesRegex(ValueError, "identity"):
                    self.m.recover(self.root, apply=True)
                remove.assert_not_called()
            self.manifest[field] = original

    def test_manifest_symlink_is_refused_without_touching_target(self):
        manifest = self.root / "lab-manifest.json"
        target = self.root / "other.json"
        manifest.rename(target)
        manifest.symlink_to(target)
        with self.assertRaises(ValueError):
            self.m.recover(self.root, apply=True)
        self.assertTrue(target.exists())
        self.assertTrue(self.group.exists())

    def test_old_device_ids_are_only_hints_and_never_deleted(self):
        device = self.base / "dev/ublkc27"
        device.write_text("possibly reused")
        log = self.root / "a/home/logs/ublk-daemon.log"
        log.parent.mkdir(parents=True)
        log.write_text("device has been added to ublkdrv dev_id=27\n")
        with patch.object(self.m, "remove_cgroup_tree", side_effect=shutil.rmtree):
            report = self.m.recover(self.root, apply=True)
        self.assertEqual(report["status"], "manual_review")
        self.assertEqual(report["devices"], [str(device)])
        self.assertEqual(device.read_text(), "possibly reused")

    def test_cleanup_failure_is_recorded_and_can_be_retried(self):
        with patch.object(self.m, "remove_cgroup_tree", side_effect=TimeoutError("busy cgroup")):
            report = self.m.recover(self.root, apply=True)
        self.assertEqual(report["status"], "manual_review")
        self.assertIn("busy cgroup", report["errors"][0])
        receipt = json.loads(next(self.root.glob("recovery-*.json")).read_text())
        self.assertEqual(receipt, report)
        with patch.object(self.m, "remove_cgroup_tree", side_effect=shutil.rmtree):
            self.assertEqual(self.m.recover(self.root, apply=True)["status"], "clean")

    def test_external_reference_prevents_clean_claim(self):
        proc = self.base / "proc/456"
        proc.mkdir()
        (proc / "mountinfo").write_text("mount references " + str(self.root / "a/tmp"))
        with patch.object(self.m, "remove_cgroup_tree", side_effect=shutil.rmtree):
            report = self.m.recover(self.root, apply=True)
        self.assertEqual(report["status"], "manual_review")
        self.assertEqual(report["references"][0]["pid"], 456)

    def test_external_directory_fd_cwd_and_process_root_are_references(self):
        proc = self.base / "proc/456"
        (proc / "fd").mkdir(parents=True)
        (self.group / "cgroup.procs").unlink()
        self.group.rmdir()
        for name, target in (("fd/8", self.root), ("cwd", self.root / "a"), ("root", self.root)):
            with self.subTest(name=name):
                link = proc / name
                link.symlink_to(target)
                try:
                    self.assertEqual(self.m.audit(self.root)["status"], "manual_review")
                finally:
                    link.unlink()
        (proc / "cwd").symlink_to(str(self.root) + "-unrelated")
        self.assertEqual(self.m.audit(self.root)["status"], "clean")

    def test_external_namespace_bind_mount_prevents_clean_claim(self):
        self.manifest["namespaces"] = ["net:[4026532999]"]
        self.m.save_manifest(self.root, self.manifest)
        proc = self.base / "proc/456"
        proc.mkdir()
        (proc / "mountinfo").write_text("35 24 0:4 net:[4026532999] /run/netns/held rw - nsfs nsfs rw\n")
        with patch.object(self.m, "remove_cgroup_tree", side_effect=shutil.rmtree):
            report = self.m.recover(self.root, apply=True)
        self.assertEqual(report["status"], "manual_review")
        self.assertEqual(report["references"][0]["kinds"], ["mountinfo"])


if __name__ == "__main__":
    unittest.main()
