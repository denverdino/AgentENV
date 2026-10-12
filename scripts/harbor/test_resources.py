import argparse
import contextlib
import io
import hashlib
import json
from pathlib import Path
import unittest


class ResourceTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('resources.py').is_file(),
                        'Harbor resource and concurrency planning is missing')
        import resources
        return resources

    def plan(self, options=(), count=8):
        module = self.module()
        parser = argparse.ArgumentParser()
        module.add_resource_arguments(parser)
        args = parser.parse_args(options)
        tasks = {f'task-{i}': {'cpus': 1, 'memory_mb': 2048, 'storage_mb': 10240}
                 for i in range(count)}
        return module.plan_resources(args, tasks)

    def test_eight_trials_receive_individual_resources_and_shared_capacity(self):
        plan = self.plan()
        self.assertEqual(plan['overrides'], {'override_cpus': 4, 'override_memory_mb': 8192})
        self.assertEqual(plan['concurrency'], 8)
        self.assertEqual(plan['max_active_trials'], 8)
        self.assertEqual(plan['node'], {'cpu_cores': 64, 'memory_gib': 128, 'pids': 8192})
        self.assertEqual(plan['required_host_memory_bytes'], 176 * 1024**3)
        self.assertEqual(plan['required_disk_bytes'], 100 * 1024**3)
        for task in plan['tasks'].values():
            self.assertEqual(task['declared'], {'cpus': 1, 'memory_mb': 2048, 'storage_mb': 10240})
            self.assertEqual(task['effective'], {'cpus': 4, 'memory_mb': 8192})

    def test_single_task_admission_uses_two_control_trials(self):
        plan = self.plan(count=1)
        self.assertEqual(plan['max_active_trials'], 2)
        self.assertEqual(plan['node']['cpu_cores'], 64)
        self.assertEqual(plan['node']['memory_gib'], 128)
        self.assertEqual(plan['required_disk_bytes'], 40 * 1024**3)

    def test_node_capacity_is_independent_of_selected_task_count(self):
        full = self.plan(count=8)
        for count in (1, 2, 4):
            with self.subTest(count=count):
                plan = self.plan(count=count)
                self.assertEqual(plan['node'], full['node'])
                self.assertEqual(plan['required_host_memory_bytes'], full['required_host_memory_bytes'])
                self.assertEqual(plan['max_active_trials'], min(count * 2, 8))

    def test_model_only_uses_the_same_node_capacity_as_controls(self):
        module = self.module()
        parser = argparse.ArgumentParser()
        module.add_resource_arguments(parser)
        plan = module.plan_resources(parser.parse_args([]),
            {'one': {'cpus': 4, 'memory_mb': 8192}}, controls=False)
        self.assertEqual(plan['node'], self.plan()['node'])
        self.assertEqual(plan['max_active_trials'], 1)
        self.assertEqual(plan['required_host_memory_bytes'], 176 * 1024**3)

    def test_trials_cannot_exceed_the_fixed_node_capacity(self):
        for options in (['--n-concurrent-trials', '32'],
                        ['--override-cpus', '16'], ['--override-memory-mb', '32768']):
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, 'node capacity'):
                self.plan(options, count=32)

    def test_ninth_sandbox_is_refused_even_when_cpu_and_ram_fit(self):
        with self.assertRaisesRegex(ValueError, 'eight|8'):
            self.plan(['--n-concurrent-trials', '9'], count=9)

    def test_original_task_resources_and_explicit_overrides(self):
        original = self.plan(['--task-resources', '--n-concurrent-trials', '1'])
        self.assertEqual(original['overrides'], {})
        self.assertEqual(original['tasks']['task-0']['effective'], {'cpus': 1, 'memory_mb': 2048})
        custom = self.plan(['--override-cpus', '2', '--override-memory-mb', '4096'])
        self.assertEqual(custom['tasks']['task-0']['effective'], {'cpus': 2, 'memory_mb': 4096})

    def test_invalid_resources_fail_before_environment_start(self):
        for options in (['--override-cpus', '0'], ['--override-memory-mb', '-1'],
                        ['--n-concurrent-trials', '0'], ['--task-resources', '--override-cpus', '4']):
            with self.subTest(options=options), contextlib.redirect_stderr(io.StringIO()), self.assertRaises((ValueError, SystemExit)):
                self.plan(options)

    def test_capacity_admission_checks_cpu_memory_and_disk(self):
        module = self.module()
        plan = self.plan()
        module.check_capacity(plan, cpu_count=104, available_memory_bytes=180 * 1024**3,
                              free_disk_bytes=173 * 1024**3)
        for values in ({'cpu_count': 63}, {'available_memory_bytes': 175 * 1024**3},
                       {'free_disk_bytes': 41 * 1024**3}):
            capacity = {'cpu_count': 104, 'available_memory_bytes': 180 * 1024**3,
                        'free_disk_bytes': 173 * 1024**3, **values}
            with self.subTest(values=values), self.assertRaises(RuntimeError):
                module.check_capacity(plan, **capacity)

    def test_selection_rejects_duplicates_and_unknown_tasks(self):
        module = self.module()
        self.assertEqual(module.select_tasks(['b', 'a'], {'a', 'b'}), ('b', 'a'))
        for names in ([], ['a', 'a'], ['../a'], ['missing']):
            with self.subTest(names=names), self.assertRaises(ValueError):
                module.select_tasks(names, {'a', 'b'})

    def test_outer_budget_accounts_for_all_trial_waves(self):
        module = self.module()
        self.assertEqual(module.job_budget(300, 8, 2, 8), 600)
        self.assertEqual(module.job_budget(300, 1, 2, 8), 300)
        self.assertEqual(module.job_budget(300, 9, 2, 8), 900)

    def test_shared_environment_content_does_not_confuse_task_allocations(self):
        module = self.module()
        plan = {'tasks': {'compile': {'effective': {'cpus': 2, 'memory_mb': 4096}},
                          'torch': {'effective': {'cpus': 1, 'memory_mb': 4096}}}}
        images = {name: {'environment_id': 'shared-content', 'environment_name': name}
                  for name in plan['tasks']}
        receipts = [{'image': images[name], 'cpu_count': cpus, 'memory_mb': 4096}
                    for name, cpus in [('compile', 2), ('torch', 1)]]
        module.validate_receipt_resources(plan, images, receipts)
        receipts[1]['cpu_count'] = 2
        with self.assertRaises(ValueError):
            module.validate_receipt_resources(plan, images, receipts)

    def test_archive_reservation_is_bound_to_the_pinned_manifest(self):
        module = self.module()
        raw = json.dumps({'config': {'size': 1000}, 'layers': [{'size': 2 * 1024**3},
                                                            {'size': 3 * 1024**3}]}).encode()
        digest = 'sha256:' + hashlib.sha256(raw).hexdigest()
        archive = module.archive_budget_bytes(raw, digest)
        self.assertEqual(archive, 5 * 1024**3 + 1000 + 16 * 1024**2)
        self.assertGreater(self.plan()['required_disk_bytes'] + archive, 105 * 1024**3)
        with self.assertRaises(ValueError):
            module.archive_budget_bytes(raw, 'sha256:' + '0' * 64)


if __name__ == '__main__':
    unittest.main()

class RuntimePrerequisiteTests(unittest.TestCase):
    def test_missing_ublk_is_refused_before_service_readiness_wait(self):
        import resources
        from pathlib import Path
        import tempfile
        self.assertTrue(hasattr(resources, 'check_ublk'), 'Host setup omission only fails after service readiness timeout')
        with tempfile.TemporaryDirectory() as directory:
            module = Path(directory) / 'ublk_drv'
            with self.assertRaisesRegex(RuntimeError, 'host setup'):
                resources.check_ublk(module, Path('/dev/null'))
            module.mkdir()
            control = Path(directory) / 'control'; control.touch()
            with self.assertRaises(RuntimeError): resources.check_ublk(module, control)
            resources.check_ublk(module, Path('/dev/null'))
