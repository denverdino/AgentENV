#!/usr/bin/env python3
"""Read-only admission checks with controlled host-boundary failures."""
from contextlib import ExitStack
import hashlib
import importlib.util
import json
import os
from pathlib import Path
from types import SimpleNamespace
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

MODULE = Path(__file__).with_name('dragonball_m4_preflight.py')


class PreflightTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(MODULE.exists(), 'shared read-only M4 preflight is missing')
        spec = importlib.util.spec_from_file_location('m4_preflight', MODULE)
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.dict(sys.modules, {"m4_preflight": self.module}))
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        self.cgroup = self.root / 'cgroup'
        self.cgroup.mkdir()
        (self.cgroup / 'cgroup.subtree_control').write_text('cpu memory pids')
        self.devices = self.root / 'dev'
        self.devices.mkdir()
        self.profile = {'disk_path': str(self.root), 'reserve_bytes': 10,
                        'planned_bytes': 50, 'retained_bytes': 40,
                        'required_env': ['EXPLICIT_INPUT'], 'socket_paths': ['/tmp/a.sock']}
        self.env = {'EXPLICIT_INPUT': 'present'}
        for name, value in (('platform.system', lambda: 'Linux'), ('os.geteuid', lambda: 0),
                            ('pwd.getpwnam', lambda _: SimpleNamespace(pw_uid=1000, pw_name='aenv')),
                            ('shutil.which', lambda name: '/usr/bin/' + name),
                            ('CGROUP_ROOT', self.cgroup), ('DEVICE_ROOT', self.devices)):
            self.stack.enter_context(patch.object(self.module, name, value) if '.' not in name
                                     else patch('m4_preflight.' + name, value))
        self.real_run = subprocess.run
        self.run = self.stack.enter_context(patch.object(self.module.subprocess, 'run',
            side_effect=self.successful_probe))
        self.stack.enter_context(patch.object(self.module.retention.shutil, 'disk_usage',
                                             return_value=(200, 100, 100)))

    def successful_probe(self, argv, **_):
        value = hashlib.sha256(b'{"EXPLICIT_INPUT":"present"}').hexdigest()
        output = json.dumps({'missing': [], 'sha256': value}) if len(argv) == 8 else ''
        return subprocess.CompletedProcess(argv, 0, output, '')

    def check(self):
        return self.module.check(self.profile, env=self.env)

    def test_nonroot_build_profile_does_not_require_vm_or_root_orchestration(self):
        self.profile.update(mode='build', run_user='aenv', required_tools=[], required_env=[])
        with patch.object(self.module.os, 'geteuid', return_value=1000):
            report = self.check()
        self.assertTrue(report['ok'], report)
        self.assertNotIn('kvm', report['checks'])
        self.assertNotIn('controllers', report['checks'])

    def test_input_files_are_checked_as_the_workload_account(self):
        self.profile['input_files'] = {'json': ['/frozen/assets.json']}
        with patch('dragonball_m4_inputs.check', return_value={'ok': False, 'errors': ['unreadable']}) as check:
            report = self.check()
        check.assert_called_once_with(self.profile['input_files'], run_user='aenv')
        self.assertFalse(report['ok'])
        self.assertIn('input_files', ' '.join(report['errors']))

    def test_nonroot_profile_refuses_another_workload_account(self):
        self.profile.update(mode='build', required_tools=[])
        with patch.object(self.module.os, 'geteuid', return_value=1001):
            report = self.check()
        self.assertFalse(report['ok'])
        self.assertIn('account', ' '.join(report['errors']))

    def test_complete_profile_passes_without_mutating_files(self):
        before = sorted((str(p.relative_to(self.root)), p.read_bytes())
                        for p in self.root.rglob('*') if p.is_file())
        report = self.check()
        self.assertTrue(report['ok'], report)
        self.assertEqual(report['errors'], [])
        self.assertEqual(report['checks']['disk']['details']['required_bytes'], 100)
        self.assertEqual(before, sorted((str(p.relative_to(self.root)), p.read_bytes())
                                       for p in self.root.rglob('*') if p.is_file()))
        argv = self.run.call_args.args[0]
        self.assertEqual(argv[:5], ['/usr/bin/runuser', '-u', 'aenv', '--', sys.executable])
        self.assertFalse(any('docker' in arg for call in self.run.call_args_list for arg in call.args[0]))

    def test_unknown_profile_keys_and_malformed_lists_are_refused(self):
        invalid = [dict(self.profile, required_environ=['EXPLICIT_INPUT']),
                   dict(self.profile, required_env='EXPLICIT_INPUT'),
                   dict(self.profile, required_tools=['ip', None]),
                   dict(self.profile, socket_paths='/tmp/a.sock'),
                   dict(self.profile, docker='false')]
        for profile in invalid:
            with self.subTest(profile=profile):
                report = self.module.check(profile, env=self.env)
                self.assertFalse(report['ok'])
                self.assertIn('profile:', ' '.join(report['errors']))
        self.run.assert_not_called()

    def test_missing_environment_is_reported_without_exposing_values(self):
        self.env = {'UNRELATED_SECRET': 'do-not-print'}
        report = self.check()
        self.assertFalse(report['ok'])
        self.assertIn('EXPLICIT_INPUT', ' '.join(report['errors']))
        self.assertNotIn('do-not-print', json.dumps(report))

    def test_runuser_boundary_rejects_dropped_empty_or_changed_values_without_exposure(self):
        self.env = {'EXPLICIT_INPUT': 'private-source-value'}
        for delegated_env in ({}, {'EXPLICIT_INPUT': ''}, {'EXPLICIT_INPUT': 'private-other-value'}):
            with self.subTest(delegated_env=delegated_env):
                def run(argv, **kwargs):
                    if len(argv) == 8:
                        # Exercise the actual child script, replacing only the
                        # unavailable runuser switch on a developer's machine.
                        return self.real_run(argv[4:], env=delegated_env, **kwargs)
                    return subprocess.CompletedProcess(argv, 0, '', '')
                self.run.side_effect = run
                report = self.check()
                self.assertFalse(report['ok'])
                self.assertIn('runuser_environment', ' '.join(report['errors']))
                rendered = json.dumps(report) + repr(self.run.call_args_list)
                self.assertNotIn('private-source-value', rendered)
                self.assertNotIn('private-other-value', rendered)

    def test_delegated_failure_does_not_echo_environment_values(self):
        self.env = {'EXPLICIT_INPUT': 'private-source-value'}
        self.run.side_effect = None
        self.run.return_value = subprocess.CompletedProcess([], 1, '', 'private-source-value')
        report = self.check()
        self.assertFalse(report['ok'])
        self.assertNotIn('private-source-value', json.dumps(report))

    def test_actual_child_environment_digest_accepts_preserved_values(self):
        def run(argv, **kwargs):
            if len(argv) == 8:
                return self.real_run(argv[4:], env=self.env, **kwargs)
            return subprocess.CompletedProcess(argv, 0, '', '')
        self.run.side_effect = run
        report = self.check()
        self.assertTrue(report['ok'], report)
        self.assertEqual(report['checks']['runuser_environment']['details']['boundary'], 'runuser')

    def test_docker_is_opt_in_and_failure_refuses(self):
        self.profile['docker'] = True
        def run(argv, **_):
            if any(arg.endswith('/docker') for arg in argv):
                return subprocess.CompletedProcess(argv, 1, '', 'denied')
            return self.successful_probe(argv)
        self.run.side_effect = run
        report = self.check()
        self.assertFalse(report['ok'])
        self.assertIn('docker', ' '.join(report['errors']))

    def test_socket_limit_counts_encoded_bytes_and_terminator(self):
        for path, accepted in [('/' + 'a' * 106, True), ('/' + 'a' * 107, False),
                               ('/' + '\u00e9' * 54, False)]:
            with self.subTest(path=path):
                self.profile['socket_paths'] = [path]
                self.assertEqual(self.check()['ok'], accepted)

    def test_disk_budgets_refuse_one_byte_short(self):
        self.profile['planned_bytes'] = 51
        report = self.check()
        self.assertFalse(report['ok'])
        self.assertIn('insufficient disk', ' '.join(report['errors']))

    def test_root_workload_account_is_refused_without_delegated_probes(self):
        with patch.object(self.module.pwd, 'getpwnam', return_value=SimpleNamespace(pw_uid=0)):
            report = self.check()
        self.assertFalse(report['ok'])
        self.assertIn('non-root', ' '.join(report['errors']))
        self.run.assert_not_called()

    def test_missing_tools_controllers_and_residual_resources_collect_errors(self):
        (self.cgroup / 'cgroup.subtree_control').write_text('cpu')
        (self.devices / 'ublkb7').touch()
        with patch.object(self.module.shutil, 'which', side_effect=lambda name: None if name == 'nft' else '/bin/' + name):
            report = self.check()
        self.assertFalse(report['ok'])
        errors = ' '.join(report['errors'])
        for value in ('nft', 'memory', 'ublkb7'):
            self.assertIn(value, errors)

    def test_platform_python_or_orchestrator_mismatch_refuses(self):
        for target, value in (('platform.system', lambda: 'Darwin'),
                              ('os.geteuid', lambda: 1000), ('sys.version_info', (3, 10))):
            with self.subTest(target=target), patch('m4_preflight.' + target, value):
                self.assertFalse(self.check()['ok'])

    def test_kvm_denial_and_probe_timeout_are_reported(self):
        for result in (subprocess.CompletedProcess([], 1, '', 'Permission denied'),
                       subprocess.TimeoutExpired(['runuser'], 10)):
            with self.subTest(result=result):
                if isinstance(result, Exception):
                    self.run.side_effect = result
                else:
                    self.run.side_effect = None
                    self.run.return_value = result
                self.assertFalse(self.check()['ok'])


class PreflightCliTests(unittest.TestCase):
    def test_real_cli_reports_refusal_as_json_and_does_not_create_files(self):
        with tempfile.TemporaryDirectory() as directory:
            child = subprocess.run([sys.executable, str(MODULE), '--run-user', 'root',
                                    '--disk-path', directory, '--reserve-bytes', '0',
                                    '--planned-bytes', '0', '--retained-bytes', '0'],
                                   capture_output=True, text=True, timeout=15)
            self.assertEqual(child.returncode, 2, child.stderr)
            report = json.loads(child.stdout)
            self.assertFalse(report['ok'])
            self.assertIn('non-root', ' '.join(report['errors']))
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == '__main__':
    unittest.main(verbosity=2)
