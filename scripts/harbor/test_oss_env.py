"""Harbor-only dotenv input, private credentials and no-MinIO selection."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace


class OssEnvTests(unittest.TestCase):
    def test_server_oss_proxy_bypasses_local_registry(self):
        import oss_env
        import urllib.request
        with patch.dict(os.environ, oss_env.server_proxy_env(), clear=True):
            self.assertTrue(urllib.request.proxy_bypass('10.241.0.1'))
            self.assertFalse(urllib.request.proxy_bypass('oss-ap-southeast-5-internal.aliyuncs.com'))

    def test_server_guest_control_requests_do_not_reach_external_proxy(self):
        import oss_env
        import shutil
        import subprocess
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        if not shutil.which('curl'):
            self.fail('curl is required to verify the server proxy boundary')
        requests = []
        class Sentinel(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                self.send_error(502, 'Guest control request reached external proxy')
            def log_message(self, *args):
                pass
        with ThreadingHTTPServer(('127.0.0.1', 0), Sentinel) as proxy:
            threading.Thread(target=proxy.serve_forever, daemon=True).start()
            env = {**os.environ, **oss_env.server_proxy_env()}
            for name in ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY'):
                env[name] = 'http://127.0.0.1:' + str(proxy.server_port)
            try:
                # No origin is needed: even a refused direct connection must
                # bypass the outbound proxy for the guest control address.
                subprocess.run(['curl', '--silent', '--connect-timeout', '1',
                    '--max-time', '2', 'http://10.11.0.1:49999/health'],
                    env=env, capture_output=True, timeout=5)
                self.assertEqual(requests, [])
            finally:
                proxy.shutdown()

    def test_git_guard_rejects_a_forced_private_env_addition(self):
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            (root / '.env').write_text('PRIVATE=never-print-this')
            subprocess.run(['git', 'add', '-f', '.env'], cwd=root, check=True)
            script = Path(__file__).with_name('check_env_files.py')
            result = subprocess.run([sys.executable, str(script)], cwd=root, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('never-print-this', result.stdout + result.stderr)
    def test_private_env_files_are_rejected_even_when_forced_into_git_index(self):
        import check_env_files
        self.assertEqual(check_env_files.forbidden(['scripts/harbor/.env', '.env.local',
            'scripts/harbor/.env.example', 'config/default.toml']), ['scripts/harbor/.env', '.env.local'])

    def test_external_store_does_not_spawn_minio(self):
        import run_lab
        import dragonball_m4_scenarios as scenarios
        services = SimpleNamespace(spawn=lambda *args: self.fail('MinIO was started'))
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(scenarios, 'command', side_effect=RuntimeError('registry setup reached')):
            lab = SimpleNamespace(root=Path(directory))
            with self.assertRaisesRegex(RuntimeError, 'registry setup reached'):
                scenarios.fixtures(lab, {}, services, snapshot_config={'bucket': 'example'})

    def test_generated_server_config_replaces_static_minio_credentials(self):
        import pwd
        import tomllib
        import run_lab
        import dragonball_m4_scenarios as scenarios
        import oss_env
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            deps = root / 'deps'
            for name in ('overlaybd', 'regctl'):
                (deps / name).mkdir(parents=True)
            asset = root / 'asset'; asset.write_text('fixture'); asset.chmod(0o755)
            node_root = root / 'node'; node_root.mkdir()
            node = SimpleNamespace(root=node_root, name='a', address='fixture', lab=SimpleNamespace(
                root=root, run_user=pwd.getpwuid(os.getuid()).pw_name,
                account=SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())),
                run=lambda argv: SimpleNamespace(stdout='999' if argv[0] == 'id' else
                    json.dumps({'backend': 'dragonball', 'dragonball': True, 'firecracker': False})),
                spawn=lambda *args: SimpleNamespace(poll=lambda: None))
            inputs = {name: str(asset) for name in ('kernel_fc', 'server', 'firecracker', 'daemon')}
            inputs['deps'] = str(deps)
            assets = {name: str(asset) for name in ('kernel_path', 'runner_path', 'tools_path')}
            config = oss_env.snapshot_config(self.write_env(root), 'run-123', uid=os.getuid())
            with patch.object(scenarios, 'wait_http'):
                scenarios.start_server(node, inputs, asset, assets, snapshot_config=config)
            text = (node_root / 'config.toml').read_text()
            self.assertEqual(tomllib.loads(text)['backend']['oss'], config)
            self.assertNotIn('test-secret', text)
            self.assertNotIn('access_key_secret', text)

    def write_env(self, root, extra=''):
        path = root / '.env'
        path.write_text('HARBOR_OSS_ENDPOINT=https://oss-ap-southeast-5-internal.aliyuncs.com\n'
            'HARBOR_OSS_BUCKET=example-eval\nHARBOR_OSS_REGION=ap-southeast-5\n'
            'HARBOR_OSS_PREFIX=harbor/integration\nHARBOR_OSS_ACCESS_KEY_ID=test-id\n'
            'HARBOR_OSS_ACCESS_KEY_SECRET="test-secret"\n' + extra)
        path.chmod(0o600)
        return path

    def test_configuration_contains_only_credential_process_and_run_prefix(self):
        import oss_env
        with tempfile.TemporaryDirectory() as directory:
            path = self.write_env(Path(directory))
            config = oss_env.snapshot_config(path, 'run-123', uid=os.getuid())
            self.assertEqual(config['prefix'], 'harbor/integration/run-123')
            self.assertEqual(config['addressing_style'], 'virtual')
            self.assertIn(str(path), config['credential_process'])
            self.assertNotIn('test-secret', json.dumps(config))
            self.assertNotIn('access_key_secret', config)
            self.assertEqual(oss_env.credentials(path)['AccessKeySecret'], 'test-secret')

    def test_invalid_input_is_rejected_without_secret_values_in_errors(self):
        import oss_env
        for extra in ['HARBOR_OSS_REGION=other\n', 'UNKNOWN=test-secret\n',
                      'HARBOR_OSS_SECURITY_TOKEN=$(test-secret)\n']:
            with self.subTest(extra=extra), tempfile.TemporaryDirectory() as directory:
                path = self.write_env(Path(directory), extra)
                with self.assertRaises(ValueError) as caught:
                    oss_env.snapshot_config(path, 'run-123', uid=os.getuid())
                self.assertNotIn('test-secret', str(caught.exception))

    def test_world_readable_or_symlink_env_is_rejected(self):
        import oss_env
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);path = self.write_env(root)
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                oss_env.snapshot_config(path, 'run-123', uid=os.getuid())
            path.chmod(0o600)
            link = root / 'linked.env';link.symlink_to(path)
            with self.assertRaises(ValueError):
                oss_env.snapshot_config(link, 'run-123', uid=os.getuid())

    def test_missing_credentials_fail_before_creating_a_lab(self):
        import oss_env
        with tempfile.TemporaryDirectory() as directory:
            path = self.write_env(Path(directory))
            path.write_text(path.read_text().replace('test-id', '').replace('"test-secret"', ''))
            with self.assertRaises(ValueError):
                oss_env.snapshot_config(path, 'run-123', uid=os.getuid())

    def test_guest_disk_receipt_reports_filesystem_capacity_without_claiming_a_quota(self):
        import asyncio
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)
        async def execute(*args, **kwargs):
            return SimpleNamespace(return_code=0, stdout='Filesystem 1024-blocks Used Available Capacity Mounted on\n'
                '/dev/vda 67108864 1048576 66060288 2% /\n')
        environment.exec = execute
        asyncio.run(environment._record_guest_disk('started'))
        self.assertEqual(environment.guest_disk['started']['total_bytes'], 64 * 1024**3)
        self.assertEqual(environment.guest_disk['started']['used_bytes'], 1024**3)


if __name__ == '__main__':
    unittest.main()
