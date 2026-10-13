"""Ordinary unit tests use the same isolated state as capability tests."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest


class UnitEnvironmentTests(unittest.TestCase):
    def test_make_unit_commands_override_live_state_with_selected_test_state(self):
        repository = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cargo = root / 'cargo'
            cargo.write_text('#!' + sys.executable + '\nimport json,os,sys\n'
                             'if "-vV" in sys.argv:print("host: x86_64-unknown-linux-gnu")\n'
                             'else:print(json.dumps({k:os.environ.get(k) for k in '
                             '("AENV_HOME_PATH","AENV_RUNTIME_PATH","AENV_DEPS_PATH")}))\n')
            cargo.chmod(0o755)
            env = {**os.environ, 'AENV_HOME_PATH': str(root / 'live')}
            commands = subprocess.check_output(['make', '-n', 'test-unit', 'CARGO=' + str(cargo),
                'AENV_TEST_STATE_DIR=' + str(root / 'test'),
                'AENV_TEST_DEPS_PATH=' + str(root / 'deps')], cwd=repository, env=env, text=True)
            selected = [line for line in commands.splitlines() if str(cargo) in line and (
                '-p agentenv -p envd -p linux-cap --lib' in line or '-p aenv --bin aenv' in line)]
            self.assertEqual(len(selected), 2)
            for command in selected:
                tokens = shlex.split(command)
                child_env = dict(env)
                while tokens and '=' in tokens[0]:
                    key, value = tokens.pop(0).split('=', 1)
                    child_env[key] = value
                result = json.loads(subprocess.check_output(tokens, env=child_env, text=True))
                self.assertEqual(result, {'AENV_HOME_PATH': str(root / 'test/home'),
                    'AENV_RUNTIME_PATH': str(root / 'test/run'), 'AENV_DEPS_PATH': str(root / 'deps')})
