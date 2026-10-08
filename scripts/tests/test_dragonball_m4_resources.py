"""Shared evidence inventories never grant deletion rights over unrelated state."""
import importlib
import json
import os
from pathlib import Path
import tempfile
import unittest


class ResourceTests(unittest.TestCase):
    def module(self):
        path = Path(__file__).with_name('dragonball_test_resources.py')
        self.assertTrue(path.exists(), 'shared resource receipt missing')
        return importlib.import_module('dragonball_test_resources')

    def test_failed_and_successful_runs_retain_evidence_and_exact_owned_paths(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            state = root / 'state'
            state.mkdir()
            evidence = root / 'evidence'
            evidence.mkdir()
            for failed in (False, True):
                with self.subTest(failed=failed):
                    run = m.RunRecord(evidence, 'regression', owned=[state])
                    try:
                        with run:
                            (evidence / 'stage.log').write_text('diagnostic')
                            if failed:
                                raise RuntimeError('fixture failed')
                    except RuntimeError:
                        pass
                    record = json.loads(run.path.read_text())
                    self.assertEqual(record['state'], 'failed' if failed else 'completed')
                    self.assertEqual(record['resources'][0]['path'], str(state))
                    self.assertEqual(record['resources'][0]['identity'], [state.stat().st_dev, state.stat().st_ino])
                    self.assertIn('stage.log', record['evidence'])
                    self.assertEqual(record['retention'], 'retain; no automatic deletion authorization')
                    self.assertTrue(state.exists())
                    self.assertEqual((evidence / 'stage.log').read_text(), 'diagnostic')

    def test_rejects_symlink_or_writable_by_other_accounts_before_receipt(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            owned = root / 'state'
            owned.mkdir()
            evidence = root / 'evidence'
            evidence.mkdir()
            link = root / 'link'
            link.symlink_to(owned, target_is_directory=True)
            for invalid in (link, owned):
                if invalid == owned:
                    owned.chmod(0o777)
                with self.assertRaisesRegex(ValueError, 'owned'):
                    m.RunRecord(evidence, 'regression', owned=[invalid])
                self.assertEqual(list(evidence.iterdir()), [])
