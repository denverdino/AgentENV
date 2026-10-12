import asyncio
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest

from dependencies import task_digest


class TemplateTests(unittest.TestCase):
    def test_reuse_disk_budget_only_reserves_active_snapshot_materialization(self):
        module = self.module()
        plan = {'max_active_trials': 8, 'tasks': {str(i): {
            'declared': {'storage_mb': 10240}, 'effective': {'memory_mb': 8192}}
            for i in range(20)}}
        try:
            size = module.template_disk_bytes(plan, {'tasks': {}}, mode='reuse')
        except TypeError:
            self.fail('Reuse still follows the full template builder disk budget')
        self.assertEqual(size, 64 * 1024**3)

    def test_model_reuses_shared_template_without_enabling_cached_verifier(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'validate_template_options'),
                        'Template reuse is still coupled to verifier qualification')
        args = SimpleNamespace(shared_templates='reuse', oss_env=Path('/private/.env'),
                               template_start=True, dependencies=None, model='qwen3.8-flash')
        run_lab.validate_template_options(args, full=True)
        for changed in ({'shared_templates': 'prepare'}, {'oss_env': None},
                        {'template_start': False}, {'dependencies': Path('/dependencies.json')}):
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                run_lab.validate_template_options(SimpleNamespace(**{**vars(args), **changed}), full=True)
        with self.assertRaises(ValueError):
            run_lab.validate_template_options(args, full=False)

    def module(self):
        self.assertTrue(Path(__file__).with_name('prepare_templates.py').exists(),
                        'Batch template preparation is missing')
        import prepare_templates
        return prepare_templates

    def test_launch_binding_rejects_another_run_resources_assets_or_task(self):
        module = self.module()
        image = {'source_platform_digest': 'sha256:' + 'a' * 64}
        resource = {'cpus': 4, 'memory_mb': 8192}
        assets = {'runner': 'b' * 64}
        binding = {'run_id': 'batch-a', 'task_sha256': 'c' * 64,
                   'image_digest': image['source_platform_digest'], 'resources': resource,
                   'assets': assets, 'snapshot_id': 'snapshot-a'}
        binding['key'] = module.launch_key(binding)
        module.validate_launch_binding(binding, image, resource, 'batch-a', assets, 'c' * 64)
        for changes in ({'run_id': 'batch-b'}, {'resources': {'cpus': 1, 'memory_mb': 2048}},
                        {'assets': {'runner': 'd' * 64}}, {'task_sha256': 'e' * 64}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                module.validate_launch_binding({**binding, **changes}, image, resource, 'batch-a', assets, 'c' * 64)

    def test_shared_template_accepts_another_run_only_in_its_repository(self):
        module = self.module()
        image = {'source_platform_digest': 'sha256:' + 'a' * 64}
        resources = {'cpus': 4, 'memory_mb': 8192}
        assets = {'runner': 'b' * 64}
        repository = {'endpoint': 'https://oss.example.test', 'region': 'test-1',
                      'bucket': 'test-bucket', 'prefix': 'harbor/shared-templates-v1'}
        binding = {'scope': 'shared', 'repository': repository,
            'run_id': 'producer-run', 'task_sha256': 'c' * 64,
            'image_digest': image['source_platform_digest'], 'resources': resources,
            'assets': assets, 'snapshot_id': 'snapshot-a'}
        binding['key'] = module.launch_key(binding)
        try:
            module.validate_launch_binding(binding, image, resources, 'consumer-run',
                assets, 'c' * 64, repository=repository)
        except (TypeError, ValueError) as error:
            self.fail('Repository-bound shared templates cannot be reused: ' + str(error))
        for repo, changed in ((None, {}), ({**repository, 'prefix': 'other'}, {}),
                              (repository, {'resources': {'cpus': 1, 'memory_mb': 2048}}),
                              (repository, {'assets': {'runner': 'd' * 64}})):
            with self.subTest(repo=repo, changed=changed), self.assertRaises(ValueError):
                module.validate_launch_binding({**binding, **changed}, image, resources,
                    'consumer-run', assets, 'c' * 64, repository=repo)

    def test_shared_preparation_reuses_committed_binding_without_a_builder(self):
        from test_shared_templates import ObjectStore
        self.assertTrue(Path(__file__).with_name('shared_templates.py').exists())
        from shared_templates import SharedTemplateStore
        module = self.module()
        repository = {'endpoint': 'https://oss.example.test', 'region': 'test-1',
                      'bucket': 'test-bucket', 'prefix': 'harbor/shared-templates-v1'}
        client = ObjectStore();store = SharedTemplateStore(client, repository)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);task = root / 'openssl-selfsigned-cert';task.mkdir()
            (task / 'task.toml').write_text('schema_version = "1.1"\n[task]\nname = "test/short-name"\n')
            images = {'short-name': {'source_platform_digest': 'sha256:' + 'a' * 64, 'workdir': '/app'}}
            image_path = root / 'images.json';image_path.write_text(json.dumps(images))
            job = {'tasks': [{'path': str(task)}], 'environment': {'kwargs': {'images_path': str(image_path)},
                'override_cpus': 4, 'override_memory_mb': 8192}}
            assets = {'runner': 'b' * 64}
            binding = {'scope': 'shared', 'repository': repository, 'run_id': 'producer',
                'task_name': task.name, 'task_sha256': task_digest(task),
                'image_digest': images['short-name']['source_platform_digest'],
                'resources': {'cpus': 4, 'memory_mb': 8192}, 'assets': assets,
                'snapshot_id': '01a123ba-af7d-7042-8623-4a7bd7181fbb',
                'source_sandbox_id': 'builder-source', 'workspace_sha256': 'f' * 64}
            binding['key'] = module.launch_key(binding)
            client.objects[repository['prefix'] + '/catalog/records/' + binding['snapshot_id'] + '.json'] = b'{}'
            store.publish(binding)
            def no_builder(*args):
                self.fail('Cache hit created a builder')
            try:
                result = asyncio.run(module.prepare_templates(job, 'consumer', assets, root / 'templates',
                    environment_factory=no_builder, shared_store=store, shared_mode='reuse'))
            except TypeError as error:
                self.fail('Shared preparation is not implemented: ' + str(error))
            self.assertEqual(result['tasks']['short-name'], binding)
            self.assertEqual(result['builders'], [])
            self.assertEqual(result['cache_hits'], [task.name])
            # Missing bindings in reuse mode must fail without cold-starting.
            client.objects.clear()
            with self.assertRaises(ValueError):
                asyncio.run(module.prepare_templates(job, 'consumer', assets, root / 'missing',
                    environment_factory=no_builder, shared_store=store, shared_mode='reuse'))

    def test_template_disk_budget_includes_full_memory_rootfs_and_guest_copies(self):
        module = self.module()
        plan = {'max_active_trials': 4, 'tasks': {name: {
            'declared': {'storage_mb': 10240}, 'effective': {'cpus': 4, 'memory_mb': 8192}}
            for name in ['openssl-selfsigned-cert', 'fix-ocaml-gc']}}
        dependencies = {'tasks': {'fix-ocaml-gc': {'verifier': {'spec': {
            'max_bundle_bytes': 2 * 1024**3, 'max_unpacked_bytes': 2 * 1024**3}}}}}
        self.assertEqual(module.template_disk_bytes(plan, dependencies), 44 * 1024**3)
        self.assertEqual(module.dependency_disk_bytes(plan, dependencies), 8 * 1024**3)

    def test_unselected_dependencies_do_not_reserve_copies_for_unrelated_tasks(self):
        module = self.module()
        plan = {'max_active_trials': 8, 'tasks': {'prove-plus-comm': {}}}
        dependencies = {'tasks': {'fix-ocaml-gc': {'verifier': {'spec': {
            'max_bundle_bytes': 2 * 1024**3, 'max_unpacked_bytes': 2 * 1024**3}}}}}
        self.assertEqual(module.dependency_disk_bytes(plan, dependencies), 0)

    def test_preparation_captures_once_and_deletes_builder_on_success_or_failure(self):
        module = self.module()
        for fail, task_name in ((False, 'openssl-selfsigned-cert'), (True, 'openssl-selfsigned-cert'),
                                (False, 'prove-plus-comm')):
            with self.subTest(fail=fail, task_name=task_name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                task = root / task_name
                task.mkdir()
                (task / 'task.toml').write_text('fixture')
                images = {task_name: {'source_platform_digest': 'sha256:' + 'a' * 64,
                           'workdir': '/app'}}
                image_path = root / 'images.json'
                image_path.write_text(json.dumps(images))
                job = {'tasks': [{'path': str(task)}], 'environment': {'kwargs': {
                    'images_path': str(image_path)}, 'override_cpus': 4, 'override_memory_mb': 8192}}
                calls = []

                class Environment:
                    environment_name = task_name
                    sandbox = None
                    _effective_cpus = 4
                    _effective_memory_mb = 8192

                    async def _create_sandbox(self):
                        calls.append('create')
                        self.sandbox = {'sandboxID': 'builder-source'}

                    async def exec(self, command, **kwargs):
                        calls.append('exec')
                        return SimpleNamespace(return_code=0, stdout='builder-source' if 'E2B_SANDBOX_ID' in command else 'f' * 64, stderr='')

                    async def _request(self, method, path, **kwargs):
                        calls.append('capture')
                        if fail:
                            raise RuntimeError('capture failed')
                        return SimpleNamespace(json=lambda: {'snapshotID': 'snapshot-a', 'cpuCount': 4, 'memoryMB': 8192})

                    async def stop(self, delete):
                        self.assert_delete = delete
                        calls.append('delete')

                factory = lambda *args: Environment()
                if fail:
                    with self.assertRaises(RuntimeError):
                        asyncio.run(module.prepare_templates(job, 'batch-a', {'runner': 'b' * 64},
                                    root / 'templates', environment_factory=factory))
                else:
                    result = asyncio.run(module.prepare_templates(job, 'batch-a', {'runner': 'b' * 64},
                                        root / 'templates', environment_factory=factory))
                    self.assertEqual(result['tasks'][task_name]['snapshot_id'], 'snapshot-a')
                    self.assertEqual(result['tasks'][task_name]['task_sha256'], task_digest(task))
                self.assertEqual(calls.count('capture'), 1)
                self.assertEqual(calls[-1], 'delete')
                self.assertTrue((root / 'templates/receipt.json').exists())


if __name__ == '__main__':
    unittest.main()
