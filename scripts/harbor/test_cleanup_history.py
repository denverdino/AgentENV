import importlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock


class CleanupHistoryTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('cleanup_history.py').exists(),
                        'Benchmark startup has no owned historical cleanup')
        return importlib.import_module('cleanup_history')

    def lab(self, root, name):
        p = root / ('db-m4.' + name); p.mkdir()
        (p / 'home').mkdir(); (p / 'home/data').write_bytes(b'old payload')
        s = p.stat()
        (p / 'resources-test.json').write_text(json.dumps({'schema': 1, 'family': 'm4-lab',
            'evidence_root': {'path': str(p), 'identity': [s.st_dev, s.st_ino], 'uid': os.getuid()},
            'state': 'completed', 'retention': 'retain; no automatic deletion authorization'}))
        os.utime(p, (100, 100))
        return p

    def test_explicit_old_owned_payload_is_deleted_but_protected_dependencies_survive(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); old = self.lab(root, 'old'); dep = self.lab(root, 'dep')
            plan = m.plan_cleanup([old, dep], cutoff=200, protected={dep / 'home/data'}, inspect_references=lambda p: [])
            result = m.apply_cleanup(plan, inspect_references=lambda p: [])
            self.assertFalse(old.exists()); self.assertTrue(dep.exists())
            self.assertEqual(result['removed'], [str(old)])
            self.assertIn('free_after', result)

    def test_current_pinned_active_unknown_and_symlink_roots_are_not_deleted(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); current = self.lab(root, 'current'); pin = self.lab(root, 'pin')
            (pin / 'PIN').touch(); os.utime(pin, (100, 100))
            active = self.lab(root, 'active'); unknown = root / 'db-m4.unknown'; unknown.mkdir()
            link = root / 'db-m4.link'; link.symlink_to(active, target_is_directory=True)
            os.utime(current, (300, 300))
            plan = m.plan_cleanup([current, pin, active, unknown, link], cutoff=200, protected=set(),
                                  inspect_references=lambda p: ['process'] if p == active else [])
            self.assertEqual(plan['targets'], [])
            self.assertEqual(len(plan['skipped']), 5)
            m.apply_cleanup(plan, inspect_references=lambda p: [])
            self.assertTrue(all(p.exists() for p in (current, pin, active, unknown, link)))

    def test_replaced_directory_or_new_reference_invalidates_plan_before_deletion(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); first = self.lab(root, 'first'); second = self.lab(root, 'second')
            plan = m.plan_cleanup([first, second], cutoff=200, protected=set(), inspect_references=lambda p: [])
            second.rename(root / 'saved'); second.mkdir(); os.utime(second, (100, 100))
            with self.assertRaises(ValueError): m.apply_cleanup(plan, inspect_references=lambda p: [])
            self.assertTrue(first.exists())
            with self.assertRaises(ValueError): m.apply_cleanup(plan, inspect_references=lambda p: ['new process'])
            self.assertTrue(first.exists())

    def test_existing_retention_markers_and_manifest_pin_are_preserved(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            for index, marker in enumerate(('pinned', 'PIN', '.pin')):
                p = self.lab(root, str(index)); (p / marker).symlink_to('/nonexistent-pin-target')
                os.utime(p, (100, 100))
                self.assertEqual(m.plan_cleanup([p], cutoff=200, protected=set(), inspect_references=lambda p: [])['targets'], [])
            p = self.lab(root, 'manifest')
            (p / 'lab-manifest.json').write_text(json.dumps({'pinned': True}))
            os.utime(p, (100, 100))
            self.assertEqual(m.plan_cleanup([p], cutoff=200, protected=set(), inspect_references=lambda p: [])['targets'], [])

    def test_partial_deletion_persists_progress_and_actual_space(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); old = self.lab(root, 'old'); receipts = []
            plan = m.plan_cleanup([old], cutoff=200, protected=set(), inspect_references=lambda p: [])
            with mock.patch('cleanup_history.shutil.rmtree', side_effect=OSError('disk I/O error')):
                try:
                    with self.assertRaises(OSError):
                        m.apply_cleanup(plan, inspect_references=lambda p: [], record=receipts.append)
                except TypeError: self.fail('Destructive cleanup has no partial progress journal')
            self.assertEqual(receipts[-1]['in_progress'], str(old))
            self.assertEqual(receipts[-1]['error_type'], 'OSError'); self.assertIn('free_after', receipts[-1])


if __name__ == '__main__':
    unittest.main()
