"""Reject misleading qualifications even when the underlying job says passed."""
import copy
import unittest


class QualificationTests(unittest.TestCase):
    def fixture(self):
        from prepare_templates import launch_key
        binding = {'task_name': 'task', 'task_sha256': 'a' * 64, 'image_digest': 'sha256:' + 'b' * 64,
                   'assets': {'runner': 'c' * 64}, 'resources': {'cpus': 4, 'memory_mb': 8192},
                   'snapshot_id': 'snapshot', 'source_sandbox_id': 'source', 'workspace_sha256': 'd' * 64,
                   'scope': 'shared', 'repository': {'bucket': 'bucket', 'prefix': 'shared'}}
        binding['key'] = launch_key(binding)
        def sandbox(identity):
            return {'sandbox_id': identity, 'template_id': 'snapshot', 'template_key': binding['key'],
                    'initial_workspace_sha256': binding['workspace_sha256'], 'start_mode': 'template',
                    'state': 'deleted', 'cpu_count': 4, 'memory_mb': 8192}
        receipt = {'passed': True, 'cleanup': 'passed', 'root': '/lab',
                   'shared_templates': {'mode': 'reuse'},
                   'snapshot_store': {'backend': 'aliyun-oss', **binding['repository']},
                   'runtime_sha256': binding['assets'],
                   'template_preparation': {'passed': True, 'builders': [], 'cache_hits': ['task'],
                                            'tasks': {'task': binding}},
                   'transport_probe_start_mode': 'template', 'cache_before_first_restore': {'image-cache/metadata/CURRENT': 16},
                   'transport_probe': {'passed': True, 'cleanup': 'passed'},
                   'sandbox_receipts': [sandbox('oracle'), sandbox('nop')],
                   'vmm_observed': [{'uid': 999}],
                   'results': [{'task_name': 'task', 'agent_info': {'name': agent}, 'exception_info': None,
                                'verifier_result': {'rewards': {'reward': score}}}
                               for agent, score in [('oracle', 1), ('nop', 0)]]}
        return receipt, sandbox('probe')

    def test_same_snapshot_different_sandboxes_qualify_across_runs(self):
        from qualification import validate_shared_runs
        first, probe = self.fixture();second = copy.deepcopy(first);second['root'] = '/another-lab'
        for row in second['sandbox_receipts']:row['sandbox_id'] += '-peer'
        other_probe = {**probe, 'sandbox_id': 'probe-peer'}
        result = validate_shared_runs([(first, probe), (second, other_probe)], runtime_uid=999)
        self.assertTrue(result['passed'])
        self.assertEqual(result['trial_count'], 4)
        self.assertEqual(result['snapshot_ids'], ['snapshot'])

    def test_false_cold_cache_rebuild_identity_resources_score_and_exception_are_rejected(self):
        from qualification import validate_shared_runs
        changes = [lambda r,p:r.update(transport_probe_start_mode='cold'),
                   lambda r,p:r['cache_before_first_restore'].update({'snapshot-local-cache/layers/a': 123}),
                   lambda r,p:r['template_preparation']['builders'].append({'sandbox_id': 'source'}),
                   lambda r,p:r['sandbox_receipts'][0].update(sandbox_id='source'),
                   lambda r,p:r['sandbox_receipts'][0].update(cpu_count=1),
                   lambda r,p:r['sandbox_receipts'][0].update(initial_workspace_sha256='wrong'),
                   lambda r,p:r['results'][0]['verifier_result']['rewards'].update(reward=0),
                   lambda r,p:r['results'][0].update(exception_info={'error': 'timeout'}),
                   lambda r,p:r['vmm_observed'][0].update(uid=0),
                   lambda r,p:p.update(template_id='another-snapshot')]
        for change in changes:
            receipt, probe = self.fixture();change(receipt, probe)
            with self.subTest(change=change), self.assertRaises(ValueError):
                validate_shared_runs([(receipt, probe)], runtime_uid=999)

    def test_cross_run_duplicate_id_or_different_snapshot_is_rejected(self):
        from qualification import validate_shared_runs
        first, probe = self.fixture();second = copy.deepcopy(first);second['root'] = '/another-lab'
        for change in ('same-id', 'another-snapshot'):
            with self.subTest(change=change), self.assertRaises(ValueError):
                if change == 'another-snapshot':
                    second['template_preparation']['tasks']['task']['snapshot_id'] = 'another'
                validate_shared_runs([(first, probe), (second, {**probe, 'sandbox_id': 'peer'})], runtime_uid=999)

    def test_different_but_internally_consistent_resource_plans_cannot_be_compared(self):
        from qualification import validate_shared_runs
        from prepare_templates import launch_key
        first, probe = self.fixture();second = copy.deepcopy(first);second['root'] = '/another-lab'
        binding = second['template_preparation']['tasks']['task'];binding['resources']['cpus'] = 2
        binding['key'] = launch_key(binding)
        for row in second['sandbox_receipts']:
            row.update(sandbox_id=row['sandbox_id'] + '-peer', cpu_count=2, template_key=binding['key'])
        other_probe = {**probe, 'sandbox_id': 'probe-peer', 'cpu_count': 2, 'template_key': binding['key']}
        with self.assertRaises(ValueError):
            validate_shared_runs([(first, probe), (second, other_probe)], runtime_uid=999)

    def test_each_task_must_have_its_own_two_restored_trial_receipts(self):
        from qualification import validate_shared_runs
        from prepare_templates import launch_key
        receipt, probe = self.fixture();second = copy.deepcopy(receipt['template_preparation']['tasks']['task'])
        second.update(task_name='another-task', task_sha256='e' * 64, snapshot_id='second-snapshot')
        second['key'] = launch_key(second)
        receipt['template_preparation']['tasks']['another-task'] = second
        receipt['template_preparation']['cache_hits'].append('another-task')
        receipt['results'].extend([{**row, 'task_name': 'another-task'} for row in list(receipt['results'])])
        receipt['sandbox_receipts'].extend([{**row, 'sandbox_id': row['sandbox_id'] + '-second'} for row in list(receipt['sandbox_receipts'])])
        with self.assertRaises(ValueError):validate_shared_runs([(receipt, probe)], runtime_uid=999)
