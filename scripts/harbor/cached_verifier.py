"""Verifier-phase cache preparation; scoring is delegated to pinned Harbor."""
import asyncio
import json
from pathlib import Path
import shlex
import time
import uuid

from failure_evidence import EvidenceVerifier as Verifier
from dependencies import task_digest, verify_bundle


class CachedVerifier(Verifier):
    def __init__(self, *args, dependencies_path, local_git_only=True, **kwargs):
        super().__init__(*args, **kwargs)
        self.dependencies_path = Path(dependencies_path)
        self.local_git_only = local_git_only
        if self.step_name is not None:
            raise ValueError('Dependency cache currently supports single-step shared verifiers')

    async def verify(self):
        data = json.loads(Path(self.dependencies_path).read_text())
        name = self.task.name.split('/')[-1]
        binding = data.get('tasks', {}).get(name)
        if (data.get('schema') != 1 or not binding or
                task_digest(self.task.paths.task_dir) != binding['task_sha256']):
            raise ValueError('Verifier dependency task binding changed or is missing')
        dependency = binding['verifier']
        if dependency is None:
            return await super().verify()
        spec = dependency['spec']
        if (spec['phase'] != 'verifier' or spec['task_name'] != name or
                spec['task_sha256'] != binding['task_sha256'] or
                spec['image_digest'] != binding['image_digest']):
            raise ValueError('Verifier dependency has the wrong task, phase or source image')
        bundle = verify_bundle(dependency['bundle_root'], spec)
        if (bundle['manifest_sha256'] != dependency['manifest_sha256'] or
                bundle['bundle_sha256'] != dependency['bundle_sha256']):
            raise ValueError('Verifier dependency content differs from the frozen job binding')
        remote = '/tmp/hb-verifier-' + uuid.uuid4().hex
        merged = {**self.task.config.verifier.env, **(self.verifier_env or {}), **self.override_env}
        count = int(merged.get('GIT_CONFIG_COUNT', '0'))
        if count < 0 or any('GIT_CONFIG_' + kind + '_' + str(i) not in merged
                            for i in range(count) for kind in ('KEY', 'VALUE')):
            raise ValueError('Existing Git runtime configuration is incomplete')
        if (merged.get('GIT_CONFIG_PARAMETERS') or
                (self.local_git_only and merged.get('GIT_ALLOW_PROTOCOL', 'file') != 'file')):
            raise ValueError('Existing Git environment has a dependency cache conflict')
        for i in range(count):
            key = merged['GIT_CONFIG_KEY_' + str(i)].lower()
            value = merged['GIT_CONFIG_VALUE_' + str(i)]
            if key.startswith('url.') and key.endswith('.insteadof') and spec['source'].startswith(value):
                raise ValueError('Existing Git rewrite has a dependency cache conflict')
        injected = {'GIT_CONFIG_COUNT': str(count + 1),
            'GIT_CONFIG_KEY_' + str(count): 'url.file://' + remote + '/ocaml.git.insteadOf',
            'GIT_CONFIG_VALUE_' + str(count): spec['source']}
        if self.local_git_only:
            injected['GIT_ALLOW_PROTOCOL'] = 'file'
        original_env = self.override_env
        receipt = {'phase': 'verifier', 'image': dependency['image'],
            'manifest_sha256': bundle['manifest_sha256'], 'bundle_sha256': bundle['bundle_sha256'],
            'commit': spec['commit'], 'bundle_bytes': bundle['bundle_bytes'],
            'started': time.time(), 'cleanup': 'pending', 'local_git_only': self.local_git_only}

        async def checked(command):
            result = await self.environment.exec(command, user='root', timeout_sec=120)
            if result.return_code:
                raise RuntimeError('Verifier cache preparation command failed')

        active_error = False
        try:
            await checked('mkdir -m 700 ' + shlex.quote(remote) + ' && mkdir ' + shlex.quote(remote + '/ocaml.git'))
            transfer_started = time.monotonic()
            await self.environment.upload_file(Path(bundle['root']) / 'bundle.tar', remote + '/bundle.tar')
            receipt['transfer_seconds'] = time.monotonic() - transfer_started
            await checked('test "$(sha256sum ' + shlex.quote(remote + '/bundle.tar') +
                " | cut -d ' ' -f1)\" = " + shlex.quote(bundle['bundle_sha256']) +
                ' && tar xf ' + shlex.quote(remote + '/bundle.tar') + ' -C ' + shlex.quote(remote + '/ocaml.git'))
            receipt['prepared_at'] = time.time()
            self.override_env = {**original_env, **injected}
            return await super().verify()
        except BaseException:
            active_error = True
            raise
        finally:
            self.override_env = original_env
            try:
                result = await asyncio.wait_for(self.environment.exec('rm -rf ' + shlex.quote(remote),
                    user='root', timeout_sec=3), timeout=5)
                if result.return_code:
                    raise RuntimeError('Verifier cache cleanup command failed')
                receipt['cleanup'] = 'passed'
            except BaseException as error:
                receipt['cleanup'] = 'failed'
                receipt['cleanup_error_type'] = type(error).__name__
                if not active_error:
                    raise
            finally:
                receipt['finished'] = time.time()
                (self.trial_paths.trial_dir / 'dependencies.json').write_text(json.dumps(receipt, indent=2))
