#!/usr/bin/env python3
"""Exact owned-device cleanup continues after an individual deletion failure."""
import importlib
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


class CleanupTests(unittest.TestCase):
    def test_explicit_node_receipts_do_not_overwrite_each_other(self):
        cleanup = importlib.import_module("dragonball_m4_cleanup")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("a", "b"):
                node = SimpleNamespace(root=root / name, cgroup=root / "cgroup" / name,
                                       lab=SimpleNamespace(root=root, run_user="aenv"))
                node.root.mkdir()
                cleanup.cleanup_owned_ublk(node, {"ublk_cli": "/test/uvm-ublk"},
                                           receipt_path=node.root / "ublk-cleanup.json")
            for name in ("a", "b"):
                self.assertEqual(json.loads((root / name / "ublk-cleanup.json").read_text()), [])
            self.assertFalse((root / "ublk-cleanup.json").exists())

    def test_failed_delete_does_not_skip_remaining_owned_devices_or_receipt(self):
        module_path = Path(__file__).with_name("dragonball_m4_cleanup.py")
        self.assertTrue(module_path.exists(), "shared owned-device cleanup missing")
        cleanup = importlib.import_module("dragonball_m4_cleanup")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            node = SimpleNamespace(root=root / "a", cgroup=root / "cgroup",
                                   lab=SimpleNamespace(root=root, run_user="aenv"))
            log = node.root / "home/logs/ublk-daemon.log"
            log.parent.mkdir(parents=True)
            log.write_text("device has been added to ublkdrv dev_id=27\n"
                           "device has been added to ublkdrv dev_id=28\n")
            real_exists = Path.exists
            alive = {27, 28}
            attempts = []
            def exists(path):
                if str(path).startswith("/dev/ublkc"):
                    return int(path.name[5:]) in alive
                return real_exists(path)
            def delete(argv):
                value = int(argv[-1])
                attempts.append(value)
                if value == 27:
                    raise subprocess.CalledProcessError(1, argv, "", "injected")
                alive.remove(value)
                return subprocess.CompletedProcess(argv, 0, "deleted", "")
            with patch.object(Path, "exists", exists), patch.object(cleanup, "command", side_effect=delete):
                with self.assertRaises(RuntimeError):
                    cleanup.cleanup_owned_ublk(node, {"ublk_cli": "/test/uvm-ublk"})
            self.assertEqual(attempts, [27, 28])
            receipt = json.loads((root / "ublk-cleanup.json").read_text())
            self.assertEqual([item["dev_id"] for item in receipt], [27, 28])
            self.assertEqual([item["exit_code"] for item in receipt], [1, 0])


if __name__ == "__main__":
    unittest.main()
