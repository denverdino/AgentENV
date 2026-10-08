"""Real preparation child processes must hold admission through fixture generation."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import shlex
import unittest
from unittest.mock import patch

import dragonball_m4_host as host
import dragonball_regression as regression
import dragonball_regression_prepare as preparation

DEFAULT_LOCK_PATH = host.LOCK_PATH


class PreparationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir='/var/tmp')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.state = self.root / 'state'
        self.state.mkdir()
        self.config = self.root / 'config.toml'
        self.config.write_text('')
        self.path = self.root / 'host.lock'
        env = {'AENV_TEST_STATE_DIR': str(self.state), 'AENV_CONFIG_PATH': str(self.config),
               'AENV_HOME_PATH': str(self.state / 'home'), 'AENV_RUNTIME_PATH': str(self.state / 'run'),
               'AENV_DEPS_PATH': str(self.root), 'PYTHONPATH': str(Path(__file__).parent)}
        for context in (patch.dict(os.environ, env), patch.object(host, 'LOCK_PATH', self.path),
                        patch('dragonball_m4_preflight.platform.system', return_value='Linux')):
            context.start()
            self.addCleanup(context.stop)

    def test_busy_host_refuses_before_any_preparation_output(self):
        fd = host.acquire_host_lock(self.path)
        try:
            with self.assertRaisesRegex(RuntimeError, 'busy'):
                preparation.prepare([sys.executable, '-c', 'raise Exception("must not run")'], 1)
            self.assertEqual(list(self.state.iterdir()), [])
        finally:
            os.close(fd)

    def test_socket_admission_refuses_before_creating_evidence(self):
        with patch.dict(os.environ, {'AENV_RUNTIME_PATH': str(self.state / ('long-' * 30))}):
            with self.assertRaisesRegex(ValueError, 'socket'):
                preparation.prepare([sys.executable, '-c', 'raise Exception("must not run")'], 1)
        self.assertEqual(list(self.state.iterdir()), [])

    def test_fresh_state_is_created_only_after_admission(self):
        self.state.rmdir()
        command = [sys.executable, '-c',
                   'import os; from pathlib import Path; from dragonball_regression import validate_fixtures; '
                   'paths=validate_fixtures(os.environ,prepared=False); '
                   '[(p.parent.mkdir(parents=True,exist_ok=True),p.write_text("{}")) for p in paths]']
        preparation.prepare(command, 1)
        self.assertTrue(self.state.is_dir())
        self.assertEqual(len(regression.validate_fixtures(os.environ)), 4)

    def test_fresh_state_rejects_unowned_writable_parent_and_symlink(self):
        self.state.rmdir()
        self.root.chmod(0o777)
        with self.assertRaisesRegex(ValueError, 'parent'):
            preparation.prepare([sys.executable, '-c', 'raise Exception("must not run")'], 1)
        self.assertFalse(self.state.exists())
        self.root.chmod(0o700)
        other = self.root / 'elsewhere'
        other.mkdir()
        self.state.symlink_to(other, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'owned|isolated'):
            preparation.prepare([sys.executable, '-c', 'raise Exception("must not run")'], 1)
        self.assertEqual(list(other.iterdir()), [])

    def test_preparation_child_keeps_lease_and_receipt_tracks_all_four_globals(self):
        paths = regression.validate_fixtures(os.environ, prepared=False)
        script = '''
import json,sys
from pathlib import Path
from dragonball_m4_host import acquire_host_lock,lock_fds
assert lock_fds(Path(sys.argv[1]))
try:
    acquire_host_lock(Path(sys.argv[1]))
except RuntimeError:
    pass
else:
    raise AssertionError('build did not hold host lease')
for name in json.loads(sys.argv[2]):
    path=Path(name); path.parent.mkdir(parents=True,exist_ok=True); path.write_text('{}')
'''
        preparation.prepare([sys.executable, '-c', script, str(self.path), json.dumps([str(p) for p in paths])], 1)
        receipt = json.loads(next(self.state.glob('prepare-*/resources-*.json')).read_text())
        self.assertEqual(receipt['state'], 'completed')
        self.assertEqual(set(receipt['inputs']['generated_configs']), {str(p) for p in paths})
        self.assertIn('build.log', receipt['evidence'])

    def test_preparation_failure_retains_log_and_records_failure(self):
        with self.assertRaisesRegex(RuntimeError, 'preparation failed'):
            preparation.prepare([sys.executable, '-c', 'print("failure evidence"); raise SystemExit(7)'], 1)
        receipt = json.loads(next(self.state.glob('prepare-*/resources-*.json')).read_text())
        self.assertEqual(receipt['state'], 'failed')
        self.assertEqual(receipt['exit_code'], 7)
        self.assertIn('failure evidence', next(self.state.glob('prepare-*/build.log')).read_text())

    def test_recursive_make_child_retains_actual_lock_descriptor(self):
        makefile = self.root / 'fixture.mk'
        checker = Path(preparation.__file__).resolve()
        command = shlex.join([sys.executable, str(checker), '--assert-locked'])
        makefile.write_text('outer:\n\t$(MAKE) -f ' + shlex.quote(str(makefile)) +
                            ' inner\ninner:\n\t' + command + '\n')
        with patch.object(host, 'LOCK_PATH', DEFAULT_LOCK_PATH), host.host_lock() as fd:
            result = subprocess.run(['make', '-f', str(makefile), 'outer'],
                                    pass_fds=(fd,), text=True, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_make_dry_run_never_runs_preparation_or_creates_evidence(self):
        result = subprocess.run(['make', '-n', 'prepare-dragonball-regression', 'CARGO=false',
                                 'REGRESSION_PYTHON=' + sys.executable],
                                cwd=Path(__file__).resolve().parents[2], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.state.iterdir()), [])
        self.assertIn('dragonball_regression_prepare.py', result.stdout)

    def test_private_make_target_cannot_build_without_lease(self):
        result = subprocess.run(['make', '_prepare-dragonball-regression', 'CARGO=false',
                                 'REGRESSION_PYTHON=' + sys.executable],
                                cwd=Path(__file__).resolve().parents[2], text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('requires an inherited host lock', result.stderr)


if __name__ == '__main__':
    unittest.main()
