"""Cancellation preserves guest evidence without changing verifier results."""
import asyncio
import json
import logging
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
import subprocess
import time
from unittest.mock import patch


class FailureEvidenceTests(unittest.TestCase):
    def environment_fixture(self, root, execute):
        from agentenv_environment import AgentENVEnvironment
        from harbor.models.trial.paths import TrialPaths
        environment = object.__new__(AgentENVEnvironment)
        environment.trial_paths = TrialPaths(root)
        environment.sandbox = {'sandboxID': 'timeout-guest'}
        environment.exec = execute
        events = []
        async def disk(phase):
            events.append('disk')
        async def request(method, path):
            events.append(method)
            receipt = root / 'failure-evidence/receipt.json'
            if (root / 'exception.txt').exists():
                self.assertTrue(receipt.exists(), 'DELETE ran before failure evidence')
                self.assertIn('finished', json.loads(receipt.read_text()))
        async def close():
            events.append('close')
        environment._record_guest_disk = disk
        environment._request = request
        environment._receipt = lambda state: None
        environment.client = SimpleNamespace(aclose=close)
        return environment, events

    def test_native_agent_timeout_and_zero_reward_capture_before_delete(self):
        import failure_evidence
        from harbor.trial.single_step import SingleStepTrial
        from harbor.trial.errors import AgentTimeoutError
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            async def execute(command, **kwargs):
                return SimpleNamespace(return_code=0, stdout='guest diagnostic', stderr='')
            environment, events = self.environment_fixture(root, execute)
            _, verifier = self.fixture(root)
            verifier.environment = environment
            trial = object.__new__(SingleStepTrial)
            trial.paths = environment.trial_paths
            trial.logger = logging.getLogger('timeout-evidence-test')
            trial._result = SimpleNamespace(exception_info=None, agent_result=None)
            trial.config = SimpleNamespace(trial_name='timeout-fixture', environment=SimpleNamespace(delete=True))
            trial.task = SimpleNamespace(instruction='fixture', config=SimpleNamespace(
                agent=SimpleNamespace(user=None),
                verifier=SimpleNamespace(environment_mode=None, environment=None)))
            trial._agent_timeout_sec = 900
            trial._load_trajectory = None
            trial._is_agent_environment_stopped = False
            trial.agent_environment = environment
            async def timeout(**kwargs):
                raise AgentTimeoutError('Original 900 second agent budget')
            async def noop(*args, **kwargs):
                pass
            async def verify():
                trial.result.verifier_result = await verifier.verify()
            async def zero_reward(instance):
                return SimpleNamespace(rewards={'reward': 0})
            trial._run_agent_phase = timeout
            trial._sync_agent_output = noop
            trial._upload_agent_logs = noop
            trial._collect_artifacts = noop
            trial._run_verifier = verify
            with patch.object(failure_evidence.Verifier, 'verify', zero_reward):
                asyncio.run(trial._run())
            self.assertTrue((root / 'failure-evidence/receipt.json').exists(),
                            'Native timeout + verifier reward zero lost evidence before DELETE')
            receipt = json.loads((root / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['primary_error_type'], 'AgentTimeoutError')
            self.assertEqual(receipt['budget_sec'], 15)
            self.assertEqual(trial.result.exception_info.exception_type, 'AgentTimeoutError')
            self.assertEqual(trial.result.exception_info.exception_message, 'Original 900 second agent budget')
            self.assertEqual(trial.result.verifier_result.rewards, {'reward': 0})
            self.assertEqual(events, ['disk', 'DELETE', 'close'])
            self.assertIsNone(environment.sandbox)

    def test_normal_zero_reward_delete_does_not_collect_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            async def forbidden(*args, **kwargs):
                self.fail('Normal trial entered failure diagnostics')
            environment, events = self.environment_fixture(root, forbidden)
            asyncio.run(environment.stop(delete=True))
            self.assertEqual(events, ['disk', 'DELETE', 'close'])
            self.assertFalse((root / 'failure-evidence').exists())

    def test_unresponsive_guest_keeps_receipt_and_reaches_delete_within_budget(self):
        import failure_evidence
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exception.txt').write_text('harbor.trial.errors.AgentTimeoutError: original\n')
            async def unresponsive(*args, **kwargs):
                pending = json.loads((root / 'failure-evidence/receipt.json').read_text())
                self.assertEqual(pending['status'], 'pending')
                await asyncio.sleep(10)
            environment, events = self.environment_fixture(root, unresponsive)
            started = time.monotonic()
            with patch.object(failure_evidence, 'CAPTURE_TIMEOUT_SEC', .02):
                asyncio.run(asyncio.wait_for(environment.stop(delete=True), .2))
            self.assertLess(time.monotonic() - started, .15)
            self.assertEqual(events, ['disk', 'DELETE', 'close'])
            receipt = json.loads((root / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['capture_error_type'], 'TimeoutError')

    def test_capture_cancellation_still_deletes_and_closes_client(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exception.txt').write_text('asyncio.exceptions.CancelledError\n')
            started = asyncio.Event()
            async def cancelled(*args, **kwargs):
                started.set()
                await asyncio.sleep(10)
            environment, events = self.environment_fixture(root, cancelled)
            async def cancel_capture():
                task = asyncio.create_task(environment.stop(delete=True))
                await started.wait()
                task.cancel()
                await task
            with self.assertRaises(asyncio.CancelledError):
                asyncio.run(cancel_capture())
            self.assertEqual(events, ['disk', 'DELETE', 'close'])
            self.assertIsNone(environment.sandbox)

    def test_keep_environment_does_not_capture_or_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exception.txt').write_text('harbor.trial.errors.AgentTimeoutError: original\n')
            async def forbidden(*args, **kwargs):
                self.fail('Keep mode ran diagnostics')
            environment, events = self.environment_fixture(root, forbidden)
            asyncio.run(environment.stop(delete=False))
            self.assertFalse((root / 'failure-evidence').exists())
            self.assertEqual(events, [])
            self.assertIsNotNone(environment.sandbox)

    def test_existing_verifier_evidence_is_preserved_on_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exception.txt').write_text('harbor.trial.errors.AgentTimeoutError: original\n')
            output = root / 'failure-evidence'
            output.mkdir()
            original = '{"status": "partial", "finished": 123, "budget_sec": 15}'
            (output / 'receipt.json').write_text(original)
            async def forbidden(*args, **kwargs):
                self.fail('DELETE repeated verifier failure diagnostics')
            environment, events = self.environment_fixture(root, forbidden)
            asyncio.run(environment.stop(delete=True))
            self.assertEqual((output / 'receipt.json').read_text(), original)
            self.assertEqual(events, ['disk', 'DELETE', 'close'])

    def test_logs_are_redacted_and_bounded_as_utf8_bytes(self):
        import failure_evidence
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            secret_log = ('界' * (256 * 1024) + '\nAuthorization: Bearer auth-secret\n'
                '"api_key": "json-secret"\nAWS_SECRET_ACCESS_KEY=aws-secret\n'
                'https://user:password@example.com/?token=url-secret\nsk-proj-hidden\n')
            async def execute(command, **kwargs):
                return SimpleNamespace(return_code=0, stdout=secret_log, stderr='')
            environment = SimpleNamespace(exec=execute, sandbox={'envdAccessToken': 'bare-guest-secret'},
                                          client=SimpleNamespace(headers={'X-API-Key': 'bare-api-secret'}))
            secret_log += 'bare-guest-secret bare-api-secret bare-model-secret\n'
            with patch.dict('os.environ', {'OPENAI_API_KEY': 'bare-model-secret'}):
                asyncio.run(failure_evidence.collect_failure(environment, root, TimeoutError()))
            files = list((root / 'failure-evidence').glob('*.txt'))
            self.assertTrue(files)
            for path in files:
                self.assertLessEqual(path.stat().st_size, 256 * 1024)
                text = path.read_text()
                for secret in ('auth-secret', 'json-secret', 'aws-secret', 'password', 'url-secret',
                               'sk-proj-hidden', 'bare-guest-secret', 'bare-api-secret', 'bare-model-secret'):
                    self.assertNotIn(secret, text)
                self.assertIn('[REDACTED]', text)

    def test_failed_log_capture_does_not_block_other_diagnostics(self):
        import failure_evidence
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            async def execute(command, **kwargs):
                if '/logs/verifier' in command:
                    raise OSError('secret capture exception')
                return SimpleNamespace(return_code=0, stdout='diagnostic data', stderr='')
            asyncio.run(failure_evidence.collect_failure(SimpleNamespace(exec=execute), root, TimeoutError()))
            receipt = json.loads((root / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['status'], 'partial')
            self.assertEqual(receipt['commands']['verifier-tail.txt']['error_type'], 'OSError')
            self.assertTrue((root / 'failure-evidence/resources.txt').exists())
            self.assertTrue((root / 'failure-evidence/dmesg.txt').exists())
            self.assertTrue((root / 'failure-evidence/opam-coq-tail.txt').exists())
            self.assertNotIn('secret capture exception', json.dumps(receipt))

    def test_real_linux_commands_keep_partial_verifier_log(self):
        import sys
        if sys.platform != 'linux':
            self.skipTest('Guest diagnostics require Linux')
        import failure_evidence
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);logs = root / 'guest-logs';logs.mkdir()
            (logs / 'test-stdout.txt').write_text('download 10 percent\n')
            opam = root / 'opam_coq.log'
            opam.write_text('old prefix\n' + 'x' * (512 * 1024) + '\nCoq compile still running\n'
                            'Authorization: Bearer actual-log-secret\n')
            async def execute(command, **kwargs):
                command = command.replace('/logs/verifier', str(logs)).replace('/tmp/opam_coq.log', str(opam))
                result = subprocess.run(['/bin/bash', '-c', command],
                                        cwd=root, capture_output=True, text=True, timeout=8)
                return SimpleNamespace(return_code=result.returncode, stdout=result.stdout, stderr=result.stderr)
            environment = SimpleNamespace(exec=execute)
            asyncio.run(failure_evidence.collect_failure(environment, root, TimeoutError()))
            self.assertIn('download 10 percent', (root / 'failure-evidence/verifier-tail.txt').read_text())
            self.assertIn('MemTotal:', (root / 'failure-evidence/resources.txt').read_text())
            text = (root / 'failure-evidence/opam-coq-tail.txt').read_text()
            self.assertIn('Coq compile still running', text)
            self.assertNotIn('old prefix', text)
            self.assertNotIn('actual-log-secret', text)
            self.assertLessEqual((root / 'failure-evidence/opam-coq-tail.txt').stat().st_size, 256 * 1024)
            self.assertIn('dmesg.txt', json.loads((root / 'failure-evidence/receipt.json').read_text())['commands'])

    def fixture(self, root):
        import failure_evidence
        verifier = object.__new__(failure_evidence.EvidenceVerifier)
        verifier.trial_paths = SimpleNamespace(trial_dir=root)
        verifier.override_env = {}
        async def execute(command, **kwargs):
            return SimpleNamespace(return_code=0, stdout='partial output\n', stderr='')
        verifier.environment = SimpleNamespace(exec=execute)
        return failure_evidence, verifier

    def test_success_keeps_original_result_without_diagnostic_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier = self.fixture(Path(directory))
            async def delegate(instance):
                return 'original result'
            with patch.object(module.Verifier, 'verify', delegate):
                self.assertEqual(asyncio.run(verifier.verify()), 'original result')
            self.assertFalse((Path(directory) / 'failure-evidence').exists())

    def test_cancelled_verifier_saves_partial_output_before_propagating_cancel(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, verifier = self.fixture(root)
            async def delegate(instance):
                raise asyncio.CancelledError()
            with patch.object(module.Verifier, 'verify', delegate):
                with self.assertRaises(asyncio.CancelledError):
                    asyncio.run(verifier.verify())
            receipt = json.loads((root / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['primary_error_type'], 'CancelledError')
            self.assertEqual(receipt['status'], 'passed')
            self.assertIn('partial output', (root / 'failure-evidence/verifier-tail.txt').read_text())
            self.assertFalse((root / 'verifier/reward.txt').exists())

    def test_capture_failure_does_not_replace_original_error(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier = self.fixture(Path(directory))
            async def delegate(instance):
                raise ValueError('original verifier failure')
            async def execute(*args, **kwargs):
                raise RuntimeError('capture failure')
            verifier.environment.exec = execute
            with patch.object(module.Verifier, 'verify', delegate):
                with self.assertRaisesRegex(ValueError, 'original verifier failure'):
                    asyncio.run(verifier.verify())
            receipt = json.loads((Path(directory) / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['status'], 'failed')

    def test_capture_has_its_own_deadline(self):
        with tempfile.TemporaryDirectory() as directory:
            module, verifier = self.fixture(Path(directory))
            async def delegate(instance):
                raise ValueError('original verifier failure')
            async def execute(*args, **kwargs):
                await asyncio.sleep(1)
            verifier.environment.exec = execute
            with patch.object(module.Verifier, 'verify', delegate), \
                    patch.object(module, 'CAPTURE_TIMEOUT_SEC', .01):
                with self.assertRaisesRegex(ValueError, 'original verifier failure'):
                    asyncio.run(verifier.verify())
            receipt = json.loads((Path(directory) / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['status'], 'failed')
            self.assertEqual(receipt['capture_error_type'], 'TimeoutError')

    def test_evidence_does_not_depend_on_the_task_working_directory(self):
        import failure_evidence
        with tempfile.TemporaryDirectory() as directory:
            async def execute(*args, **kwargs):
                self.assertEqual(kwargs.get('cwd'), '/')
                return SimpleNamespace(return_code=0, stdout='captured', stderr='')
            asyncio.run(failure_evidence.collect_failure(SimpleNamespace(exec=execute),
                                                         directory, TimeoutError()))
            receipt = json.loads((Path(directory) / 'failure-evidence/receipt.json').read_text())
            self.assertEqual(receipt['status'], 'passed')


if __name__ == '__main__':
    unittest.main()
