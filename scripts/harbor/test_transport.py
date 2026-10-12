"""Connect wire contract tests: reject truncation/errors; retain nonzero exits."""
import importlib.util
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import re
import signal
import struct
import subprocess
import sys
import tempfile
import tarfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

MODULE = Path(__file__).with_name('agentenv_environment.py')


def frame(message, flag=0):
    body = json.dumps(message).encode()
    return struct.pack('>BI', flag, len(body)) + body


class TransportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not MODULE.exists():
            return
        spec = importlib.util.spec_from_file_location('agentenv_environment', MODULE)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def decoder(self):
        self.assertTrue(MODULE.exists(), 'AgentENV Harbor adapter is missing')
        return self.module.decode_process

    def test_nonzero_exit_and_split_utf8_output(self):
        decode = self.decoder()
        body = frame({'event': {'data': {'stdout': '5A=='}}})
        body += frame({'event': {'data': {'stdout': 'uK0='}}})
        body += frame({'event': {'data': {'stderr': 'YmFk'}}})
        body += frame({'event': {'end': {'exitCode': 7}}}) + frame({}, 2)
        result = decode(body)
        self.assertEqual((result.stdout, result.stderr, result.return_code), ('中', 'bad', 7))

    def test_truncation_or_missing_exit_is_not_success(self):
        decode = self.decoder()
        for body in (b'\x00', frame({'event': {'end': {'exitCode': 0}}})[:-1], frame({}, 2)):
            with self.subTest(body=body), self.assertRaises(ValueError):
                decode(body)

    def test_connect_error_is_not_guest_exit(self):
        decode = self.decoder()
        with self.assertRaises(ValueError):
            decode(frame({'error': {'code': 'unavailable', 'message': 'guest unavailable'}}, 2))

    def test_messages_after_terminal_are_rejected(self):
        decode = self.decoder()
        with self.assertRaises(ValueError):
            decode(frame({'event': {'end': {'exitCode': 0}}}) + frame({}, 2) + frame({}, 0))


class ExecutionTests(unittest.TestCase):
    def test_successful_transfer_reports_cleanup_failure_inside_callers_exception_handler(self):
        from agentenv_environment import AgentENVEnvironment
        for operation, cancellation in ((operation, cancellation)
                for operation in ('upload_dir', 'download_dir') for cancellation in (False, True)):
            with self.subTest(operation=operation, cancellation=cancellation), tempfile.TemporaryDirectory() as directory:
                environment = object.__new__(AgentENVEnvironment)

                async def upload(*args):
                    pass

                async def download(source, target):
                    with tarfile.open(target, 'w:gz'):
                        pass

                async def execute(command):
                    if command.startswith('rm -f '):
                        if cancellation:
                            await asyncio.sleep(10)
                        raise RuntimeError('archive cleanup failure')

                environment.upload_file = upload
                environment.download_file = download
                environment._checked_exec = execute
                target = '/tests' if operation == 'upload_dir' else Path(directory) / 'download'

                async def run():
                    async with asyncio.timeout(0.01):
                        await getattr(environment, operation)(directory, target)

                try:
                    raise ValueError('unrelated caller exception')
                except ValueError:
                    if cancellation:
                        with self.assertRaises(TimeoutError):
                            asyncio.run(run())
                    else:
                        with self.assertRaisesRegex(RuntimeError, 'archive cleanup failure'):
                            asyncio.run(getattr(environment, operation)(directory, target))

    def test_transfer_cancellation_bounds_archive_cleanup(self):
        from agentenv_environment import AgentENVEnvironment
        import agentenv_environment
        for operation in ('upload_dir', 'download_dir'):
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                environment = object.__new__(AgentENVEnvironment)
                commands = []

                async def upload(*args):
                    pass

                async def execute(command):
                    commands.append(command)
                    await asyncio.sleep(10)

                environment.upload_file = upload
                environment._checked_exec = execute

                async def run():
                    async with asyncio.timeout(0.01):
                        await getattr(environment, operation)(directory, '/tests')

                started = time.monotonic()
                with mock.patch.object(agentenv_environment, 'TRANSFER_CLEANUP_TIMEOUT_SEC', 0.02, create=True):
                    with self.assertRaises(TimeoutError):
                        asyncio.run(asyncio.wait_for(run(), 0.2))
                self.assertLess(time.monotonic() - started, 0.15)
                self.assertTrue(commands[-1].startswith('rm -f '))

    def test_transfer_failure_is_not_replaced_by_archive_cleanup_failure(self):
        from agentenv_environment import AgentENVEnvironment
        for operation in ('upload_dir', 'download_dir'):
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                environment = object.__new__(AgentENVEnvironment)

                async def upload(*args):
                    pass

                async def execute(command):
                    if command.startswith('rm -f '):
                        raise RuntimeError('archive cleanup failure')
                    raise ValueError('original transfer failure')

                environment.upload_file = upload
                environment._checked_exec = execute
                with self.assertRaisesRegex(ValueError, 'original transfer failure'):
                    asyncio.run(getattr(environment, operation)(directory, '/tests'))

    def test_startup_cancellation_bounds_internal_delete(self):
        from agentenv_environment import AgentENVEnvironment
        import agentenv_environment
        environment = object.__new__(AgentENVEnvironment)
        environment.sandbox = {'sandboxID': 'startup-timeout'}
        environment.request_timeout_sec = 600
        closed = []

        async def create():
            await asyncio.sleep(10)

        async def request(*args, **kwargs):
            await asyncio.sleep(10)

        async def close():
            closed.append(True)

        environment._create_sandbox = create
        environment.client = SimpleNamespace(request=request, aclose=close)

        async def run():
            async with asyncio.timeout(0.01):
                await environment.start(False)

        started = time.monotonic()
        with mock.patch.object(agentenv_environment, 'START_FAILURE_CLEANUP_TIMEOUT_SEC', 0.02, create=True):
            with self.assertRaises(TimeoutError):
                asyncio.run(asyncio.wait_for(run(), 0.2))
        self.assertLess(time.monotonic() - started, 0.15, 'Internal cleanup bypassed its deadline')
        self.assertEqual(closed, [True], 'Internal DELETE did not reach its cleanup deadline')
        self.assertIsNotNone(environment.sandbox, 'Failed DELETE must retain sandbox ownership')

    def test_startup_failure_is_not_replaced_by_delete_failure(self):
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)

        async def create():
            raise ValueError('original startup failure')

        async def stop(delete):
            raise RuntimeError('delete failure')

        environment._create_sandbox = create
        environment.stop = stop
        with self.assertRaisesRegex(ValueError, 'original startup failure'):
            asyncio.run(environment.start(False))

    def test_download_streams_chunks_and_keeps_existing_file_on_interruption(self):
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)
        environment.request_timeout_sec = 10
        environment.sandbox = {'sandboxID': 'stream-probe'}
        interrupt = [False]

        class Response:
            def raise_for_status(self):
                pass

            async def aiter_bytes(self):
                yield b'first'
                if interrupt[0]:
                    raise RuntimeError('interrupted response')
                yield b'second'

        class Stream:
            async def __aenter__(self):
                return Response()

            async def __aexit__(self, *args):
                pass

        environment.client = SimpleNamespace(stream=lambda *args, **kwargs: Stream())

        async def forbidden(*args, **kwargs):
            raise AssertionError('Download must stream instead of buffering the response')

        environment._request = forbidden
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'download'
            asyncio.run(environment.download_file('/tmp/source', target))
            self.assertEqual(target.read_bytes(), b'firstsecond')
            interrupt[0] = True
            with self.assertRaises(RuntimeError):
                asyncio.run(environment.download_file('/tmp/source', target))
            self.assertEqual(target.read_bytes(), b'firstsecond')
            self.assertEqual(list(Path(directory).iterdir()), [target])

    def test_upload_file_uses_an_open_stream_instead_of_loading_all_bytes(self):
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)
        chunks = []

        async def request(method, path, **kwargs):
            stream = kwargs['files']['file'][1]
            self.assertTrue(hasattr(stream, 'read'), 'Upload loaded the whole file into memory')
            while block := stream.read(7):
                chunks.append(block)

        environment._request = request
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'binary'
            source.write_bytes(bytes(range(256)))
            asyncio.run(environment.upload_file(source, '/tmp/binary'))
        self.assertEqual(b''.join(chunks), bytes(range(256)))

    @unittest.skipUnless(sys.platform == 'linux', 'Guest command semantics require Linux')
    def test_command_without_timeout_restores_inherited_sigint(self):
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)
        environment.environment_name = 'signal-probe'
        environment.images = {'signal-probe': {}}
        environment.task_env_config = SimpleNamespace(workdir=None)
        environment.request_timeout_sec = 10
        environment._resolve_user = lambda user: user
        environment._merge_env = lambda env: env

        async def execute(method, path, **kwargs):
            payload = kwargs['content'][5:]
            process = json.loads(payload)['process']
            result = subprocess.run([process['cmd'], *process['args']],
                cwd=process.get('cwd'), env=os.environ, capture_output=True, timeout=5,
                preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_IGN))
            return SimpleNamespace(content=frame({'event': {'end': {'exitCode': result.returncode}}}) + frame({}, 2))

        environment._request = execute
        probe = 'import signal; assert signal.getsignal(signal.SIGINT) != signal.SIG_IGN'
        result = asyncio.run(environment.exec(shlex.quote(sys.executable) + ' -c ' + shlex.quote(probe)))
        self.assertEqual(result.return_code, 0, 'Command inherited ignored SIGINT from envd')


@unittest.skipUnless(sys.platform == 'linux' and shutil.which('systemd-tmpfiles'),
                     'Guest boot cleanup requires Linux systemd-tmpfiles')
class BootCleanupTransferTests(unittest.TestCase):
    def fixture(self, root):
        from agentenv_environment import AgentENVEnvironment
        environment = object.__new__(AgentENVEnvironment)
        guest = root / 'guest'
        for name in ('tmp', 'var/tmp', 'tests', 'usr/lib/tmpfiles.d', 'etc'):
            (guest / name).mkdir(parents=True, exist_ok=True)
        # Exact policy found in the affected Chromium task guest.
        (guest / 'usr/lib/tmpfiles.d/tmp.conf').write_text('D /tmp 1777 root root -\n')
        (guest / 'etc/passwd').write_text('root:x:0:0:root:/root:/bin/sh\n')
        (guest / 'etc/group').write_text('root:x:0:\n')

        def boot_cleanup():
            result = subprocess.run(['systemd-tmpfiles', '--root=' + str(guest), '--remove', '--boot'],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)

        async def execute(command):
            translated = re.sub(r'/var/tmp|/tmp|/tests',
                                lambda match: str(guest / match[0].lstrip('/')), command)
            result = subprocess.run(['/bin/bash', '-c', translated], capture_output=True, text=True)
            if result.returncode:
                raise RuntimeError(result.stderr)

        async def upload(source, target):
            path = guest / target.lstrip('/')
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, path)
            boot_cleanup()

        async def download(source, target):
            boot_cleanup()
            shutil.copyfile(guest / source.lstrip('/'), target)

        environment._checked_exec = execute
        environment.upload_file = upload
        environment.download_file = download
        return environment, guest

    def test_upload_survives_guest_boot_tmp_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment, guest = self.fixture(root)
            source = root / 'source'
            source.mkdir()
            (source / 'binary').write_bytes(bytes(range(256)))
            asyncio.run(environment.upload_dir(source, '/tests'))
            self.assertEqual((guest / 'tests/binary').read_bytes(), bytes(range(256)))
            self.assertEqual(list((guest / 'var/tmp').iterdir()), [])

    def test_download_survives_guest_boot_tmp_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment, guest = self.fixture(root)
            (guest / 'tests/binary').write_bytes(bytes(range(256)))
            target = root / 'downloaded'
            asyncio.run(environment.download_dir('/tests', target))
            self.assertEqual((target / 'binary').read_bytes(), bytes(range(256)))
            self.assertEqual(list((guest / 'var/tmp').iterdir()), [])


class TemplateAdapterTests(unittest.TestCase):
    def fixture(self, root):
        from agentenv_environment import AgentENVEnvironment, NetworkMode
        from dependencies import task_digest
        from prepare_templates import launch_key
        task = root / 'openssl-selfsigned-cert'
        task.mkdir()
        (task / 'task.toml').write_text('fixture')
        (task / 'environment').mkdir()
        (task / 'environment/Dockerfile').write_text('FROM ubuntu:24.04\n')

        class Adapter(AgentENVEnvironment):
            @property
            def _effective_cpus(self):
                return 4

            @property
            def _effective_memory_mb(self):
                return 8192

        environment = object.__new__(Adapter)
        environment.environment_name = task.name
        environment.environment_dir = task / 'environment'
        environment.task_env_config = SimpleNamespace(docker_image=None, workdir='/app')
        environment.images = {task.name: {'environment_id': environment.environment_id,
            'source_platform_digest': 'sha256:' + 'a' * 64,
            'image': 'registry/task@sha256:' + 'a' * 64, 'workdir': '/app'}}
        environment.task_source_dir = task
        environment.template_run_id = 'batch-a'
        environment.runtime_assets = {'runner': 'b' * 64}
        binding = {'run_id': 'batch-a', 'task_sha256': task_digest(task),
            'image_digest': environment.images[task.name]['source_platform_digest'],
            'resources': {'cpus': 4, 'memory_mb': 8192}, 'assets': environment.runtime_assets,
            'snapshot_id': 'snapshot-a', 'source_sandbox_id': 'builder-source',
            'workspace_sha256': 'c' * 64}
        binding['key'] = launch_key(binding)
        environment.templates = {'schema': 1, 'passed': True, 'tasks': {task.name: binding}}
        environment.sandbox_timeout_sec = 600
        environment.sandbox = None
        environment.session_id = 'trial'
        environment._network_policy = SimpleNamespace(network_mode=NetworkMode.NO_NETWORK)
        environment._startup_env = lambda: {}
        environment._mount_targets = lambda **kwargs: []
        environment._receipt = lambda state: None
        calls = []

        async def noop(*args, **kwargs):
            pass

        async def request(method, path, **kwargs):
            calls.append((method, path, kwargs))
            return SimpleNamespace(json=lambda: {'sandboxID': 'new-child', 'templateID': 'snapshot-a'})

        async def execute(*args, **kwargs):
            return SimpleNamespace(return_code=0, stdout='new-child', stderr='')

        environment.ensure_dirs = noop
        environment._upload_environment_dir_after_start = noop
        environment.client = SimpleNamespace(aclose=noop)
        environment._request = request
        environment.exec = execute
        return environment, binding, calls

    def test_template_start_uses_warm_api_and_verifies_pristine_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            environment, binding, calls = self.fixture(Path(directory))

            async def workspace(*args):
                return binding['workspace_sha256']

            with unittest.mock.patch('prepare_templates.workspace_digest', workspace):
                asyncio.run(environment.start(False))
            self.assertEqual(calls[0][1], '/sandboxes')
            self.assertEqual(calls[0][2]['json']['templateID'], 'snapshot-a')
            self.assertNotIn('image', calls[0][2]['json'])
            self.assertEqual(environment.initial_workspace_sha256, binding['workspace_sha256'])

    def test_shared_template_launch_on_another_run_retains_task_isolation(self):
        with tempfile.TemporaryDirectory() as directory:
            environment, binding, calls = self.fixture(Path(directory))
            repository = {'endpoint': 'https://oss.example.test', 'bucket': 'shared-bucket',
                          'region': 'test-1', 'prefix': 'harbor/shared-templates-v1'}
            binding.update(scope='shared', repository=repository)
            environment.template_repository = repository
            environment.template_run_id = 'consumer-run'
            async def workspace(*args):
                return binding['workspace_sha256']
            with unittest.mock.patch('prepare_templates.workspace_digest', workspace):
                asyncio.run(environment.start(False))
            self.assertEqual(calls[0][1], '/sandboxes')
            self.assertEqual(environment.sandbox['sandboxID'], 'new-child')
            self.assertEqual(environment.initial_workspace_sha256, binding['workspace_sha256'])
            environment.template_repository = {**repository, 'prefix': 'wrong'}
            with self.assertRaises(ValueError):
                environment._template_binding()

    def test_stale_run_fails_before_api_and_workspace_mismatch_deletes_child(self):
        with tempfile.TemporaryDirectory() as directory:
            environment, binding, calls = self.fixture(Path(directory))
            environment.template_run_id = 'another-batch'
            with self.assertRaises(ValueError):
                asyncio.run(environment.start(False))
            self.assertEqual(calls, [])
        with tempfile.TemporaryDirectory() as directory:
            environment, binding, calls = self.fixture(Path(directory))

            async def workspace(*args):
                return 'f' * 64

            with unittest.mock.patch('prepare_templates.workspace_digest', workspace), self.assertRaises(RuntimeError):
                asyncio.run(environment.start(False))
            self.assertEqual(calls[-1][0], 'DELETE')
            self.assertIsNone(environment.sandbox)

    def test_explicit_empty_template_artifact_never_falls_back_to_cold(self):
        for artifact in ({}, [], None):
            with self.subTest(artifact=artifact), tempfile.TemporaryDirectory() as directory:
                environment, _, calls = self.fixture(Path(directory))
                environment.template_start_requested = True
                environment.templates = artifact
                with self.assertRaises(ValueError):
                    asyncio.run(environment.start(False))
                self.assertEqual(calls, [])


class AcceptanceTests(unittest.TestCase):
    def test_vmm_observation_uses_binary_identity_instead_of_basename(self):
        import run_lab
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = root / 'renamed-runner'
            runner.write_bytes(b'frozen candidate')
            proc = root / '123'
            proc.mkdir()
            (proc / 'exe').symlink_to(runner)
            (proc / 'comm').write_text('renamed-runner\n')
            (proc / 'status').write_text('Uid:\t999\t999\t999\t999\n')
            row = run_lab.observe_vmm(proc, runner, 999)
            self.assertEqual((row['pid'], row['uid'], row['name']), (123, 999, 'renamed-runner'))
            other = root / 'another-binary'
            other.write_bytes(b'other')
            self.assertIsNone(run_lab.observe_vmm(proc, other, 999))
            with self.assertRaises(AssertionError):
                run_lab.observe_vmm(proc, runner, 0)

    def validator(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'validate_results'), 'Exact trial/step validation is missing')
        return run_lab.validate_results

    def results(self):
        rows = []
        for task in ('hello-workdir', 'hello-user', 'hello-multi-step-simple'):
            for agent in ('oracle', 'nop'):
                verifier = {'rewards': {'reward': 1 if agent == 'oracle' else 0}}
                steps = [{'step_name': name, 'verifier_result': verifier} for name in
                         ('create-file', 'append-content')] if task.endswith('simple') else None
                rows.append({'task_name': task, 'agent_info': {'name': agent},
                             'verifier_result': verifier, 'step_results': steps})
        return rows

    def test_exact_trials_and_steps_are_accepted(self):
        self.validator()(self.results())

    def test_duplicate_trial_does_not_replace_missing_task(self):
        validate = self.validator()
        rows = self.results()
        rows[0] = rows[1]
        with self.assertRaises(ValueError):
            validate(rows)

    def test_missing_step_or_wrong_negative_score_fails(self):
        validate = self.validator()
        rows = self.results()
        rows[-1]['step_results'].pop()
        with self.assertRaises(ValueError):
            validate(rows)
        rows = self.results()
        rows[1]['verifier_result'] = {'rewards': {'reward': 1}}
        with self.assertRaises(ValueError):
            validate(rows)

    def test_model_failure_score_is_valid_but_positive_nop_is_not(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'validate_benchmark_results'),
                        'Benchmark model/control score validation is missing')
        rows = []
        for task in ('openssl-selfsigned-cert', 'log-summary-date-ranges'):
            for agent in ('oracle', 'nop', 'terminus-2'):
                rows.append({'task_name': 'terminal-bench/' + task,
                    'agent_info': {'name': agent}, 'verifier_result': {
                    'rewards': {'reward': 1 if agent == 'oracle' else 0}}})
        run_lab.validate_benchmark_results(rows, model=True)
        rows[1]['verifier_result']['rewards']['reward'] = 1
        with self.assertRaises(ValueError):
            run_lab.validate_benchmark_results(rows, model=True)

    def test_full_suite_cell_rejects_wrong_task_duplicate_and_exception(self):
        import run_lab
        rows = [{'task_name': 'adaptive-rejection-sampler', 'agent_info': {'name': agent},
                 'verifier_result': {'rewards': {'reward': score}}}
                for agent, score in [('oracle', 1), ('nop', 0), ('terminus-2', 0)]]
        validate = lambda data: run_lab.validate_benchmark_results(
            data, model=True, task_names=('adaptive-rejection-sampler',))
        validate(rows)
        for invalid in (rows[:-1], rows + [rows[0]],
                [dict(rows[0], task_name='another-task'), *rows[1:]],
                [*rows[:2], dict(rows[2], exception_info={'exception_type': 'AgentTimeoutError'})]):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                validate(invalid)

    def test_oracle_diagnostic_does_not_suppress_full_suite_model_trial(self):
        import run_lab
        rows = [{'task_name': 'build-cython-ext', 'agent_info': {'name': agent},
                 'verifier_result': {'rewards': {'reward': 0}}}
                for agent in ('oracle', 'nop', 'terminus-2')]
        with self.assertRaises(ValueError):
            run_lab.validate_benchmark_results(rows, model=True, task_names=('build-cython-ext',))
        run_lab.validate_benchmark_results(rows, model=True, task_names=('build-cython-ext',), strict_oracle=False)
        run_lab.validate_benchmark_results(rows[2:], model=True, task_names=('build-cython-ext',), controls=False)
        rows[1]['verifier_result']['rewards']['reward'] = 1
        with self.assertRaises(ValueError):
            run_lab.validate_benchmark_results(rows, model=True, task_names=('build-cython-ext',), strict_oracle=False)


class SourceTests(unittest.TestCase):
    def test_full_delivery_rejects_missing_image_and_changed_task(self):
        from prepare_acr import REVISION, verify_delivery
        from harbor.environments.definition import environment_content_hash
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files, images = {}, {}
            for i in range(89):
                name = f'task-{i}'
                task = root / 'tasks' / name
                (task / 'environment').mkdir(parents=True)
                for relative, data in [('task.toml', 'schema_version = "1.0"\n'),
                                       ('environment/Dockerfile', 'FROM ubuntu:24.04\n')]:
                    (task / relative).write_text(data)
                    files[name + '/' + relative] = hashlib.sha256(data.encode()).hexdigest()
                digest = 'sha256:' + 'a' * 64
                images[name] = {'environment_name': name,
                    'environment_id': environment_content_hash(task / 'environment'),
                    'platform': 'linux/amd64', 'source_digest': digest,
                    'source_platform_digest': digest, 'destination_tag': 'registry/ns/' + name + ':fixed',
                    'image': 'registry/ns/' + name + '@' + digest}
            (root / 'task-source-manifest.json').write_text(json.dumps({'revision': REVISION, 'files': files}))
            mapping = root / 'images.json'
            mapping.write_text(json.dumps(images))
            verify_delivery(root)
            mapping.write_text(json.dumps({key: value for key, value in images.items() if key != 'task-0'}))
            with self.assertRaises(ValueError):
                verify_delivery(root)
            mapping.write_text(json.dumps(images))
            (root / 'tasks/task-0/environment/Dockerfile').write_text('FROM different\n')
            with self.assertRaises(ValueError):
                verify_delivery(root)

    def test_task_short_name_matches_harbor_for_namespaced_task(self):
        import prepare_tasks
        self.assertTrue(hasattr(prepare_tasks, 'task_environment_name'), 'Task image naming missing')
        with tempfile.TemporaryDirectory() as directory:
            task = Path(directory) / 'directory-name'
            task.mkdir()
            (task / 'task.toml').write_text('schema_version = "1.1"\n[task]\nname = "computer-1/click-calibration"\n')
            self.assertEqual(prepare_tasks.task_environment_name(task), 'click-calibration')
            (task / 'task.toml').write_text('schema_version = "1.0"\n')
            self.assertEqual(prepare_tasks.task_environment_name(task), 'directory-name')

    def test_computer_scores_require_positive_negative_and_exact_trial_set(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'validate_computer_results'), 'Computer score validation missing')
        rows = [{'task_name': 'computer-1/click-calibration', 'agent_info': {'name': agent},
                 'verifier_result': {'rewards': {'reward': score}}}
                for agent, score in [('computer-calibration-control', 1), ('nop', 0), ('computer-1', 0)]]
        run_lab.validate_computer_results(rows, model=True)
        for invalid in (rows[:-1], rows + [rows[0]],
                [dict(rows[0], exception_info={'exception_type': 'TimeoutError'}), *rows[1:]],
                [rows[0], dict(rows[1], verifier_result={'rewards': {'reward': 1}}), rows[2]]):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                run_lab.validate_computer_results(invalid, model=True)

    def test_added_step_setup_is_rejected(self):
        from prepare_tasks import HARBOR_REVISION, TASKS, verify_source
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = {'pyproject.toml': '[project]', 'src/harbor.py': '# pinned'}
            files.update({'examples/tasks/' + task + '/instruction.md': 'pinned' for task in TASKS})
            hashes = {}
            for name, content in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
                hashes[name] = hashlib.sha256(content.encode()).hexdigest()
            manifest = root / 'manifest.json'
            manifest.write_text(json.dumps({'revision': HARBOR_REVISION, 'files': hashes}))
            verify_source(root, manifest)
            extra = root / 'examples/tasks/hello-multi-step-simple/steps/create-file/workdir/setup.sh'
            extra.parent.mkdir(parents=True)
            extra.write_text('exit 0')
            with self.assertRaises(ValueError):
                verify_source(root, manifest)


class CredentialTests(unittest.TestCase):
    def test_computer_config_selects_safe_vision_agent_without_shell_tools(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'computer_agent_config'), 'Computer Use configuration missing')
        from harbor.agents.factory import AgentFactory
        from harbor.models.trial.config import AgentConfig
        from dashscope_agent import DashScopeComputer1
        config = AgentConfig.model_validate(run_lab.computer_agent_config('qwen3.8-flash'))
        self.assertIs(AgentFactory.get_agent_class_from_config(config), DashScopeComputer1)
        self.assertTrue(config.kwargs['enable_images'])
        self.assertEqual(config.kwargs['extra_tools'], [])
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory, patch.dict('os.environ', {'OPENAI_API_KEY': 'test-secret-value'}):
            agent = AgentFactory.create_agent_from_config(config, logs_dir=Path(directory))
        self.assertEqual(agent._provider_name, 'litellm')
        self.assertTrue(agent._enable_images)
        self.assertFalse(agent._enable_bash)

    def test_model_config_selects_the_credential_safe_agent(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'model_agent_config'), 'Model agent configuration is not exposed')
        from harbor.agents.factory import AgentFactory
        from harbor.models.trial.config import AgentConfig
        from dashscope_agent import DashScopeTerminus2
        config = AgentConfig.model_validate(run_lab.model_agent_config('qwen3.8-flash'))
        self.assertIs(AgentFactory.get_agent_class_from_config(config), DashScopeTerminus2)

    def test_real_model_trial_is_not_stopped_at_the_smoke_turn_limit(self):
        import run_lab
        from harbor.agents.factory import AgentFactory
        from harbor.models.trial.config import AgentConfig
        from unittest.mock import patch
        config = AgentConfig.model_validate(run_lab.model_agent_config('qwen3.8-flash'))
        with tempfile.TemporaryDirectory() as directory, patch.dict('os.environ', {'OPENAI_API_KEY': 'test-secret-value'}):
            agent = AgentFactory.create_agent_from_config(config, logs_dir=Path(directory))
        self.assertGreater(agent._max_episodes, 25)

    def test_nested_model_logging_redacts_authorization(self):
        script = Path(__file__).with_name('dashscope_agent.py')
        self.assertTrue(script.exists(), 'Credential-safe model logger is missing')
        from dashscope_agent import redact
        payload = {'headers': {'Authorization': 'Bearer test-secret-value'},
                   'nested': [{'api_key': 'test-secret-value'}], 'usage': {'total_tokens': 30}}
        clean = redact(payload, 'test-secret-value')
        self.assertEqual(clean, {'headers': {'Authorization': 'Bearer [REDACTED]'},
            'nested': [{'api_key': '[REDACTED]'}], 'usage': {'total_tokens': 30}})
        self.assertEqual(payload['nested'][0]['api_key'], 'test-secret-value')


if __name__ == '__main__':
    unittest.main()
