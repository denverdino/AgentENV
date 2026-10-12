import importlib
import json
from pathlib import Path
import tempfile
import unittest


class CampaignTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('run_campaign.py').exists(), 'Persistent native Job entrypoint is missing')
        return importlib.import_module('run_campaign')

    def test_single_native_job_preserves_budgets_and_independent_paths(self):
        m = self.module(); base = {'environment': {'override_cpus': 4, 'override_memory_mb': 8192,
            'kwargs': {'api_url': 'http://test', 'images_path': '/images'}, 'env': {'HTTP_PROXY': 'http://proxy'}},
            'verifier': {'import_path': 'failure_evidence:EvidenceVerifier'}}
        trial = {'id': 'a', 'task': 'task', 'agent': {'name': 'nop'}}
        original = json.loads(json.dumps(base))
        c = m.native_job(base, trial, Path('/prepared'), Path('/out'))
        self.assertEqual(c['n_concurrent_trials'], 1); self.assertEqual(c['n_attempts'], 1)
        self.assertEqual(c['retry']['max_retries'], 0)
        self.assertEqual(c['tasks'], [{'path': '/prepared/tasks/task'}])
        self.assertNotIn('override_timeout_sec', c['verifier'])
        self.assertNotIn('override_timeout_sec', c['agents'][0]); self.assertEqual(base, original)
        self.assertNotEqual(c['jobs_dir'], m.native_job(base, {**trial, 'id': 'b'}, Path('/prepared'), Path('/out'))['jobs_dir'])
        for part in (c['environment']['env'], c['environment']['kwargs']):
            self.assertNotIn('OPENAI_API_KEY', part); self.assertNotIn('DASHSCOPE_API_KEY', part)

    def test_cli_success_does_not_replace_deletion_proof(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as root:
            p = Path(root); (p / 'spawned.json').write_text('{}')
            with self.assertRaises(ValueError): m.verify_trial_cleanup(p, [])
            receipt = p / 'agentenv.json'
            receipt.write_text(json.dumps({'sandbox_id': 'a', 'state': 'running'}))
            with self.assertRaises(ValueError): m.verify_trial_cleanup(p, [])
            receipt.write_text(json.dumps({'sandbox_id': 'a', 'state': 'deleted'}))
            with self.assertRaises(ValueError): m.verify_trial_cleanup(p, [{'sandboxID': 'a'}])
            self.assertTrue(m.verify_trial_cleanup(p, [{'sandboxID': 'another-worker'}]))

    def test_preparation_failure_without_job_does_not_need_a_fake_sandbox_receipt(self):
        m = self.module()
        with tempfile.TemporaryDirectory() as root:
            self.assertTrue(m.verify_trial_cleanup(Path(root), []))


if __name__ == '__main__': unittest.main()

class CampaignResultTests(unittest.TestCase):
    def test_aggregate_job_result_is_not_counted_as_a_native_trial(self):
        import run_campaign as m
        with tempfile.TemporaryDirectory() as root:
            p = Path(root); job = p / 'jobs/job'; trial = job / 'task__id'; trial.mkdir(parents=True)
            (job / 'result.json').write_text('{"stats": {}}')
            (trial / 'result.json').write_text('{"agent_info": {"name": "nop"}}')
            self.assertTrue(hasattr(m, 'trial_result_paths'), 'Aggregate Job results are mixed with native trials')
            self.assertEqual(m.trial_result_paths(p), [trial / 'result.json'])

    def test_probe_unknown_creation_is_not_considered_clean(self):
        import run_campaign as m
        with tempfile.TemporaryDirectory() as root:
            p = Path(root); (p / 'probe-spawned.json').write_text('{}')
            with self.assertRaises(ValueError): m.verify_trial_cleanup(p, [])

class CampaignNativeErrorsTests(unittest.TestCase):
    def test_native_exit_zero_cannot_hide_trial_exception(self):
        import run_campaign as m
        with tempfile.TemporaryDirectory() as root:
            p = Path(root); job = p / 'jobs/job/task__id'; job.mkdir(parents=True)
            path = job / 'result.json'
            path.write_text(json.dumps({'task_name': 'task', 'exception_info': {'exception_type': 'EnvironmentStartError'},
                                        'verifier_result': None, 'agent_info': {'name': 'nop'}}))
            self.assertTrue(hasattr(m, 'native_trial_errors'), 'Exit zero hides native trial exceptions')
            errors = m.native_trial_errors(p, {'task': 'task'})
            self.assertEqual(errors[0]['exception_info']['exception_type'], 'EnvironmentStartError')

class RegistryRoutingTests(unittest.TestCase):
    def test_local_registry_commands_do_not_inherit_model_egress_proxy(self):
        import run_campaign as m
        self.assertTrue(hasattr(m, 'registry_environment'), 'Worker routes local registry import through the external proxy')
        original = {'HTTP_PROXY': 'http://outside', 'https_proxy': 'http://outside', 'ALL_PROXY': 'socks://outside',
                    'NO_PROXY': 'localhost,10.241.0.11', 'AENV_API_KEY': 'test-only'}
        env = m.registry_environment(original, '/local/regctl.json')
        self.assertFalse(any(k in env for k in ('HTTP_PROXY', 'https_proxy', 'ALL_PROXY')))
        self.assertEqual(env['REGCTL_CONFIG'], '/local/regctl.json'); self.assertEqual(original['HTTP_PROXY'], 'http://outside')


class CampaignCancellationTests(unittest.TestCase):
    def test_namespace_wrapper_cancellation_waits_for_worker_report(self):
        import run_campaign as m
        self.assertTrue(hasattr(m, 'stop_campaign'), 'Cancellation only terminates nsenter, losing worker reports')
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            class NamespaceWrapper:
                def wait(inner, timeout):
                    self.assertTrue((output / 'cancel-request.json').exists())
                    self.assertEqual(timeout, 75)
                    (output / 'campaign.json').write_text('{"reason":"cancelled","pending":["ninth"]}')
                    return 1
                def terminate(inner): self.fail('SIGTERM only stops the namespace wrapper')
                def kill(inner): self.fail('Cooperative cancellation must finish before fallback kill')
            m.stop_campaign(NamespaceWrapper(), output)
            self.assertEqual(json.loads((output / 'campaign.json').read_text())['pending'], ['ninth'])
