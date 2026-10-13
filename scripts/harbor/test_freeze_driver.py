"""Candidate packaging cannot inherit secrets or overwrite frozen evidence."""
from pathlib import Path
import tempfile
import unittest

from freeze_driver import freeze


class FreezeDriverTests(unittest.TestCase):
    def test_deleted_harbor_module_is_rejected_before_candidate_creation(self):
        repository = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);baseline = root / 'baseline'
            old = baseline / 'scripts/harbor/test_runtime_dirs.py'
            old.parent.mkdir(parents=True);old.write_text('raise RuntimeError("stale test")\n')
            with self.assertRaisesRegex(ValueError, 'test_runtime_dirs.py'):
                freeze(baseline, repository, root / 'candidate')
            self.assertFalse((root / 'candidate').exists())
            self.assertEqual(old.read_text(), 'raise RuntimeError("stale test")\n')

    def test_private_env_in_baseline_is_rejected_before_output_is_created(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);baseline = root / 'baseline';baseline.mkdir()
            (baseline / '.env').write_text('secret')
            with self.assertRaises(ValueError):
                freeze(baseline, root, root / 'candidate')
            self.assertFalse((root / 'candidate').exists())

    def test_control_scripts_are_frozen_and_verified_without_private_files(self):
        import hashlib
        from freeze_driver import verify_candidate
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);baseline = root / 'baseline';baseline.mkdir()
            control = root / 'controller.py';control.write_text('print("control")\n')
            candidate = root / 'candidate'
            manifest = freeze(baseline, Path(__file__).resolve().parents[2], candidate, controls=[control])
            name = 'harbor-control/controller.py'
            self.assertEqual(manifest[name], hashlib.sha256(control.read_bytes()).hexdigest())
            verify_candidate(candidate)
            (candidate / 'source' / name).write_text('changed')
            with self.assertRaises(ValueError):verify_candidate(candidate)
            private = root / '.env';private.write_text('private')
            with self.assertRaises(ValueError):
                freeze(baseline, root, root / 'private-candidate', controls=[private])
            self.assertFalse((root / 'private-candidate').exists())
            disguised = root / '.env.sh';disguised.write_text('PRIVATE=fixture\n')
            with self.assertRaises(ValueError):freeze(baseline, Path(__file__).resolve().parents[2], root / 'disguised-candidate', controls=[disguised])
            self.assertFalse((root / 'disguised-candidate').exists())

    def test_existing_candidate_cannot_be_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);baseline = root / 'baseline';baseline.mkdir()
            candidate = root / 'candidate';candidate.mkdir()
            (candidate / 'original').write_text('keep')
            with self.assertRaises(FileExistsError):
                freeze(baseline, root, candidate)
            self.assertEqual((candidate / 'original').read_text(), 'keep')

    def test_stale_baseline_is_replaced_with_importable_runtime_and_capability_runner(self):
        import subprocess
        import shutil
        import os
        import sys
        from freeze_driver import verify_candidate
        repository = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); baseline = root / 'baseline'
            shutil.copytree(repository / 'scripts/tests', baseline / 'scripts/tests')
            old_lab = baseline / 'scripts/tests/dragonball_m4_lab.py'
            old_lab.write_text('raise RuntimeError("stale runtime")\n')
            old_campaign_test = baseline / 'scripts/tests/test_dragonball_m4_campaign.py'
            old_campaign_test.write_text('raise RuntimeError("stale campaign tests")\n')
            candidate = root / 'candidate'
            freeze(baseline, repository, candidate)
            verify_candidate(candidate)
            tests = candidate / 'source/scripts/tests'
            result = subprocess.run([sys.executable, '-B', '-c',
                'import dragonball_m4_lab, dragonball_m4_scenarios'],
                cwd=tests, capture_output=True, text=True, timeout=10,
                env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
            self.assertEqual(result.returncode, 0, result.stderr)
            runner = candidate / 'source/scripts/run-with-capabilities.sh'
            self.assertTrue(runner.is_file(), 'Frozen runtime cannot delegate capabilities')
            self.assertTrue(os.access(runner, os.X_OK), 'Capability runner must remain executable')
            self.assertEqual(runner.read_bytes(), (repository / 'scripts/run-with-capabilities.sh').read_bytes())
            self.assertEqual((tests / 'dragonball_m4_lab.py').read_bytes(),
                             (repository / 'scripts/tests/dragonball_m4_lab.py').read_bytes())
            self.assertEqual((tests / 'test_dragonball_m4_campaign.py').read_bytes(),
                             (repository / 'scripts/tests/test_dragonball_m4_campaign.py').read_bytes())


if __name__ == '__main__':
    unittest.main()
