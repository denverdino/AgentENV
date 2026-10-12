import asyncio
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

from dependencies import build_bundle, task_digest


class Environment:
    def __init__(self):
        self.calls = []
        self.cancel_upload = False

    async def upload_file(self, source, target):
        self.calls.append(('upload', target))
        if self.cancel_upload:
            raise asyncio.CancelledError()

    async def exec(self, command, **kwargs):
        self.calls.append(('exec', command))
        return SimpleNamespace(return_code=0, stdout='', stderr='')


class CachedVerifierTests(unittest.TestCase):
    def test_successful_cache_cleanup_failure_is_visible_inside_callers_exception_handler(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier, task, bundle = self.fixture(Path(directory))
            async def delegate(instance):
                return 'success'
            async def execute(command, **kwargs):
                if command.startswith('rm -rf'):
                    raise RuntimeError('cleanup failed')
                return SimpleNamespace(return_code=0, stdout='', stderr='')
            verifier.environment.exec = execute
            try:
                raise ValueError('unrelated caller exception')
            except ValueError:
                with patch.object(module.Verifier, 'verify', delegate):
                    with self.assertRaisesRegex(RuntimeError, 'cleanup failed'):
                        asyncio.run(verifier.verify())

    def fixture(self, root, dependency=True):
        self.assertTrue(Path(__file__).with_name('cached_verifier.py').exists(),
                        'Verifier-only dependency adapter is missing')
        import cached_verifier
        task = root / 'fix-ocaml-gc'
        task.mkdir()
        (task / 'task.toml').write_text('task fixture')
        tree = root / 'mirror'
        tree.mkdir()
        (tree / 'HEAD').write_text('ref: refs/heads/main')
        spec = {'task_name': task.name, 'task_sha256': task_digest(task),
                'image_digest': 'sha256:' + 'b' * 64, 'phase': 'verifier',
                'source': 'https://github.com/sadiqj/ocaml/', 'commit': 'c' * 40,
                'max_unpacked_bytes': 1024, 'max_bundle_bytes': 1024**2}
        bundle = build_bundle(spec, tree, root / 'bundle')
        binding = {'image': 'acr/deps@sha256:' + 'd' * 64, 'spec': spec,
                   'bundle_root': bundle['root'], 'manifest_sha256': bundle['manifest_sha256'],
                   'bundle_sha256': bundle['bundle_sha256']}
        payload = {'schema': 1, 'tasks': {task.name: {'task_sha256': spec['task_sha256'],
                   'image_digest': spec['image_digest'], 'verifier': binding if dependency else None}}}
        path = root / 'dependencies.json'
        path.write_text(json.dumps(payload))
        verifier = object.__new__(cached_verifier.CachedVerifier)
        verifier.dependencies_path = path
        verifier.task = SimpleNamespace(name=task.name, paths=SimpleNamespace(task_dir=task),
                         config=SimpleNamespace(verifier=SimpleNamespace(env={'TASK_ENV': 'keep'})))
        verifier.trial_paths = SimpleNamespace(trial_dir=root / 'trial')
        verifier.trial_paths.trial_dir.mkdir()
        verifier.environment = Environment()
        verifier.override_env = {'ORIGINAL': 'keep', 'GIT_CONFIG_COUNT': '1',
                                  'GIT_CONFIG_KEY_0': 'core.autocrlf', 'GIT_CONFIG_VALUE_0': 'false'}
        verifier.verifier_env = {'PHASE_ENV': 'keep'}
        verifier.local_git_only = True
        return cached_verifier, verifier, task, bundle

    def test_dependencies_are_injected_at_verify_and_original_verifier_runs_once(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier, task, bundle = self.fixture(Path(directory))
            original = dict(verifier.override_env)
            called = []

            async def delegate(instance):
                called.append(instance)
                self.assertTrue(any(c[0] == 'upload' for c in instance.environment.calls))
                self.assertEqual(instance.override_env['ORIGINAL'], 'keep')
                self.assertEqual(instance.override_env['GIT_CONFIG_KEY_0'], 'core.autocrlf')
                self.assertEqual(instance.override_env['GIT_CONFIG_COUNT'], '2')
                self.assertEqual(instance.override_env['GIT_CONFIG_VALUE_1'], 'https://github.com/sadiqj/ocaml/')
                self.assertEqual(instance.override_env['GIT_ALLOW_PROTOCOL'], 'file')
                return 'original-result'

            self.assertEqual(verifier.environment.calls, [])
            with patch.object(module.Verifier, 'verify', delegate):
                self.assertEqual(asyncio.run(verifier.verify()), 'original-result')
            self.assertEqual(called, [verifier])
            self.assertEqual(verifier.override_env, original)
            self.assertIn('rm -rf', verifier.environment.calls[-1][1])
            receipt = json.loads((verifier.trial_paths.trial_dir / 'dependencies.json').read_text())
            self.assertEqual(receipt['cleanup'], 'passed')
            self.assertEqual(receipt['phase'], 'verifier')

    def test_task_without_verifier_dependencies_delegates_without_transfers(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier, task, bundle = self.fixture(Path(directory), dependency=False)

            async def delegate(instance):
                return 1

            with patch.object(module.Verifier, 'verify', delegate):
                self.assertEqual(asyncio.run(verifier.verify()), 1)
            self.assertEqual(verifier.environment.calls, [])

    def test_conflicting_git_rewrite_or_protocol_is_rejected_before_transfer(self):
        for conflict in ({'GIT_ALLOW_PROTOCOL': 'https'},
                         {'GIT_CONFIG_KEY_0': 'url.https://mirror.example/.insteadOf',
                          'GIT_CONFIG_VALUE_0': 'https://github.com/sadiqj/ocaml/'},
                         {'GIT_CONFIG_PARAMETERS': "'url.other.insteadOf=https://github.com/'"}):
            with self.subTest(conflict=conflict), tempfile.TemporaryDirectory() as directory:
                module, verifier, task, bundle = self.fixture(Path(directory))
                verifier.override_env.update(conflict)
                async def delegate(instance):
                    return 'unexpected delegation'

                with patch.object(module.Verifier, 'verify', delegate):
                    with self.assertRaisesRegex(ValueError, 'conflict'):
                        asyncio.run(verifier.verify())
                self.assertEqual(verifier.environment.calls, [])

    def test_changed_task_and_corrupt_bundle_fail_before_guest_transfer(self):
        for corrupt in (True, False):
            with self.subTest(corrupt=corrupt), tempfile.TemporaryDirectory() as directory:
                module, verifier, task, bundle = self.fixture(Path(directory))
                if corrupt:
                    (Path(bundle['root']) / 'bundle.tar').write_bytes(b'bad')
                else:
                    (task / 'task.toml').write_text('changed')
                with self.assertRaises(ValueError):
                    asyncio.run(verifier.verify())
                self.assertEqual(verifier.environment.calls, [])

    def test_cancelled_upload_and_delegate_error_preserve_exception_and_cleanup(self):
        for cancellation in (True, False):
            with self.subTest(cancellation=cancellation), tempfile.TemporaryDirectory() as directory:
                module, verifier, task, bundle = self.fixture(Path(directory))
                original = dict(verifier.override_env)
                verifier.environment.cancel_upload = cancellation
                called = []

                async def delegate(instance):
                    called.append(instance)
                    raise RuntimeError('original verifier failed')

                with patch.object(module.Verifier, 'verify', delegate):
                    with self.assertRaises(asyncio.CancelledError if cancellation else RuntimeError):
                        asyncio.run(verifier.verify())
                self.assertEqual(verifier.override_env, original)
                self.assertEqual(len(called), 0 if cancellation else 1)
                self.assertIn('rm -rf', verifier.environment.calls[-1][1])


if __name__ == '__main__':
    unittest.main()
