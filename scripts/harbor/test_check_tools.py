"""The selected interpreter and real unittest exit status govern the check."""
from pathlib import Path
import os
import tempfile
import unittest
from unittest.mock import patch


class CheckToolsTests(unittest.TestCase):
    def test_old_python_is_rejected_before_importing_driver_dependencies(self):
        from check_tools import require_python
        with self.assertRaises(ValueError):require_python((3, 9))
        require_python((3, 12))

    def test_failing_test_keeps_a_nonzero_status(self):
        from check_tools import run_checks
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            suite = Path(directory)
            (suite / 'test_failure.py').write_text('import unittest\nclass Failure(unittest.TestCase):\n'
                                                 '    def test_failure(self):self.fail("expected sentinel")\n')
            with tempfile.TemporaryFile() as log:
                self.assertNotEqual(run_checks(root, suite=suite, output=log), 0)
                log.seek(0);self.assertIn(b'expected sentinel', log.read())

    def test_directory_without_git_or_a_frozen_manifest_is_rejected(self):
        from check_tools import run_checks
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):run_checks(Path(directory))

    def test_nested_python_inherits_bytecode_protection(self):
        from check_tools import run_checks
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ):
            os.environ.pop('PYTHONDONTWRITEBYTECODE', None)
            suite = Path(directory)
            (suite / 'fixture.py').write_text('value = 1\n')
            (suite / 'test_nested.py').write_text('import subprocess,sys,unittest\n'
                'class Nested(unittest.TestCase):\n'
                '    def test_child(self):\n'
                f'        subprocess.run([sys.executable,"-c","import fixture"],cwd={str(suite)!r},check=True)\n')
            with tempfile.TemporaryFile() as log:
                self.assertEqual(run_checks(root, suite=suite, output=log), 0)
            self.assertFalse((suite / '__pycache__').exists())

    def test_successful_suite_cannot_modify_a_frozen_candidate(self):
        from check_tools import run_checks
        from freeze_driver import freeze
        repository = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            baseline = root / 'baseline';baseline.mkdir()
            candidate = root / 'candidate'
            freeze(baseline, repository, candidate)
            suite = root / 'suite';suite.mkdir()
            (suite / 'test_mutation.py').write_text('from pathlib import Path\nimport unittest\n'
                'class Mutation(unittest.TestCase):\n'
                '    def test_write(self):\n'
                f'        Path({str(candidate / "source/unexpected.txt")!r}).write_text("mutation")\n')
            with tempfile.TemporaryFile() as log:
                self.assertNotEqual(run_checks(candidate / 'source', suite=suite, output=log), 0)
                log.seek(0);self.assertIn(b'file set changed', log.read())
