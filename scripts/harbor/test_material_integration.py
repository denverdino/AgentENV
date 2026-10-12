"""Material admission at the real CLI and environment startup boundaries."""
import asyncio
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import material_inputs


class MaterialEntryTests(unittest.TestCase):
    def tasks(self, root):
        tasks = {}
        for name in ('extract-moves-from-video', 'normal-task'):
            task = root / name
            task.mkdir()
            (task / 'instruction.md').write_text('Use the original input.')
            tasks[name] = task
        return tasks

    def test_model_gate_keeps_original_selection_and_only_blocks_required_task(self):
        with tempfile.TemporaryDirectory() as directory:
            tasks = self.tasks(Path(directory))
            manifest = material_inputs.load_manifest(None, tasks)
            gate = asyncio.run(material_inputs.select_material_tasks(manifest, model_required=True))
        self.assertEqual(gate['selected_task_names'], ['extract-moves-from-video', 'normal-task'])
        self.assertEqual(gate['blocked_task_names'], ['extract-moves-from-video'])
        self.assertEqual(gate['eligible_task_names'], ['normal-task'])
        self.assertEqual(gate['selected_count'], 2)

    def test_controls_cli_records_unchecked_and_keeps_both_tasks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tasks = self.tasks(root)
            job = root / 'job.json'
            job.write_text(json.dumps({'tasks': {name: str(path) for name, path in tasks.items()},
                                      'model_required': False}))
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                self.assertEqual(material_inputs.main(['--job', str(job)]), 0)
            gate = json.loads(stdout.getvalue())['material_gate']
        self.assertEqual(gate['blocked_task_names'], [])
        self.assertEqual(gate['eligible_count'], 2)
        self.assertEqual(gate['checks']['extract-moves-from-video']['status'], 'not_checked')


class MaterialStartupTests(unittest.TestCase):
    def environment(self, root, *, required):
        from agentenv_environment import AgentENVEnvironment
        task = root / 'extract-moves-from-video'
        (task / 'environment').mkdir(parents=True)
        (task / 'instruction.md').write_text('Original input is required.')
        environment = object.__new__(AgentENVEnvironment)
        environment.environment_dir = task / 'environment'
        environment.material_manifest = None
        environment.material_required = required
        environment.trial_paths = SimpleNamespace(trial_dir=root / 'trial')
        environment.sandbox = {'sandboxID': 'new-id'}
        stopped = []

        async def nothing(*args, **kwargs):
            pass

        async def execute(command, **kwargs):
            return SimpleNamespace(return_code=0, stdout='new-id', stderr='')

        async def stop(delete):
            stopped.append(delete)

        environment._create_sandbox = nothing
        environment.exec = execute
        environment._record_guest_disk = nothing
        environment._template_binding = lambda: None
        environment._mount_targets = lambda **kwargs: []
        environment.ensure_dirs = nothing
        environment._upload_environment_dir_after_start = nothing
        environment.stop = stop
        return environment, stopped

    def test_model_start_requires_materials_before_return_and_deletes_failed_sandbox(self):
        with tempfile.TemporaryDirectory() as directory:
            environment, stopped = self.environment(Path(directory), required=True)
            with self.assertRaisesRegex(ValueError, 'materials'):
                asyncio.run(environment.start(False))
            receipt = json.loads((environment.trial_paths.trial_dir / 'material-inputs.json').read_text())
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['phase'], 'guest')
        self.assertEqual(stopped, [True])

    def test_controls_start_does_not_require_missing_video(self):
        with tempfile.TemporaryDirectory() as directory:
            environment, stopped = self.environment(Path(directory), required=False)
            asyncio.run(environment.start(False))
        self.assertEqual(stopped, [])

    def test_host_http_binding_survives_serialized_job_and_is_consumed_in_guest_start(self):
        from agentenv_environment import AgentENVEnvironment
        from harbor.environments.definition import environment_content_hash
        from harbor.models.task.config import EnvironmentConfig
        from harbor.models.trial.paths import TrialPaths
        import test_material_inputs
        fixtures = test_material_inputs.MaterialInputTests()
        with tempfile.TemporaryDirectory() as directory, fixtures.server() as url:
            root = Path(directory)
            path, task, _ = fixtures.manifest(root, row=fixtures.http_row(url))
            (task / 'environment').mkdir()
            host_job = root / 'host-job.json'
            host_job.write_text(json.dumps({'material_manifest': str(path),
                'tasks': {task.name: str(task)}, 'model_required': True}))
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                self.assertEqual(material_inputs.main(['--job', str(host_job)]), 0)
            host = json.loads(stdout.getvalue())
            self.assertEqual(host['material_gate']['eligible_task_names'], [task.name])
            job = root / 'job.json'
            job.write_text(json.dumps({'environment': {'kwargs': {
                'material_manifest': host['manifest'], 'material_required': True}}}))
            images = root / 'images.json'
            images.write_text(json.dumps({task.name: {'image': 'image@sha256:' + 'b' * 64,
                'environment_id': environment_content_hash(task / 'environment')}}))
            config = EnvironmentConfig(workdir='/app')
            with patch.dict(os.environ, {'AENV_API_KEY': 'private-test-sentinel'}):
                environment = AgentENVEnvironment(environment_dir=task / 'environment',
                    environment_name=task.name, session_id='material-fixture', trial_paths=TrialPaths(root / 'trial'),
                    task_env_config=config, network_policy=config.resolve_baseline(),
                    api_url='http://127.0.0.1:8000', images_path=str(images),
                    **json.loads(job.read_text())['environment']['kwargs'])

            async def create():
                environment.sandbox = {'sandboxID': 'new-id'}

            async def nothing(*args, **kwargs):
                pass

            async def execute(command, **kwargs):
                if command.startswith('printf '):
                    return SimpleNamespace(return_code=0, stdout='new-id', stderr='')
                script = shlex.split(command)[2]
                process = await asyncio.create_subprocess_exec(sys.executable, '-B', '-c', script,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
                body, error = await process.communicate()
                return SimpleNamespace(return_code=process.returncode, stdout=body.decode(), stderr=error.decode())

            environment._create_sandbox = create
            environment.exec = execute
            environment._record_guest_disk = nothing
            environment.ensure_dirs = nothing
            environment._upload_environment_dir_after_start = nothing
            try:
                asyncio.run(environment.start(False))
                receipt = json.loads((environment.trial_paths.trial_dir / 'material-inputs.json').read_text())
            finally:
                asyncio.run(environment.client.aclose())
        self.assertTrue(receipt['passed'])
        self.assertEqual(receipt['inputs'][0]['scope'], 'guest')
        self.assertNotIn('private-test-sentinel', json.dumps(receipt))


class LabMaterialEntryTests(unittest.TestCase):
    def test_mixed_controls_model_validation_keeps_controls_on_blocked_task(self):
        import run_lab
        rows = [{'task_name': task, 'agent_info': {'name': agent},
                 'verifier_result': {'rewards': {'reward': 1 if agent == 'oracle' else 0}}}
                for task, agents in [('extract-moves-from-video', ['oracle', 'nop']),
                    ('normal-task', ['oracle', 'nop', 'terminus-2'])] for agent in agents]
        run_lab.validate_benchmark_results(rows, model=True,
            task_names=('extract-moves-from-video', 'normal-task'), model_task_names=('normal-task',))
        with self.assertRaises(ValueError):
            run_lab.validate_benchmark_results(rows[1:], model=True,
                task_names=('extract-moves-from-video', 'normal-task'), model_task_names=('normal-task',))

    def test_effective_profile_agent_selection_includes_controls_and_model(self):
        import run_lab
        args = SimpleNamespace(model='qwen3.8-flash', model_only=False)
        agents = run_lab.evaluation_agents(args)
        self.assertEqual([row.get('name') for row in agents[:2]], ['oracle', 'nop'])
        self.assertEqual(agents[2]['import_path'], 'dashscope_agent:DashScopeTerminus2')

    def test_real_main_all_blocked_writes_denominator_receipt_before_any_lab(self):
        import run_lab
        import dragonball_m4_host
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            prepared = root / 'prepared'
            task = prepared / 'tasks/extract-moves-from-video'
            (task / 'environment').mkdir(parents=True)
            (task / 'instruction.md').write_text('Original required video.')
            (task / 'task.toml').write_text('[environment]\ndocker_image="ubuntu:24.04"\n')
            (task / 'tests').mkdir()
            (task / 'tests/test.sh').write_text('#!/bin/sh\nexit 0\n')
            (prepared / 'prepared.json').write_text(json.dumps({
                'source_root': str(root), 'source_manifest': str(root / 'source.json'),
                'acr_delivery': str(root / 'delivery'), 'benchmark': {'revision': material_inputs.TASK_REVISION},
                'task_names': ['extract-moves-from-video'], 'images': {}}))
            inputs = root / 'inputs.json'
            inputs.write_text('{}')
            manifest = material_inputs.load_manifest(None, {task.name: task})
            gate = asyncio.run(material_inputs.select_material_tasks(manifest, model_required=True))
            output = root / 'output'
            with patch.object(run_lab.os, 'geteuid', return_value=0), \
                    patch.object(run_lab.signal, 'signal'), \
                    patch.object(run_lab, 'verify_source', return_value={'files': {}}), \
                    patch.object(run_lab, 'verify_delivery', return_value=({'files': {}}, {task.name: {}})), \
                    patch.object(run_lab, 'verify_tree'), \
                    patch.object(dragonball_m4_host, 'require_clean_host'), \
                    patch.object(run_lab, 'run_material_preflight',
                                 return_value={'manifest': manifest, 'material_gate': gate}), \
                    patch.object(run_lab, 'Lab', side_effect=AssertionError('must not create a Lab')), \
                    patch.dict(os.environ, {'DASHSCOPE_API_KEY': 'private-model-sentinel'}):
                code = run_lab._main(['--inside', '--inputs', str(inputs), '--prepared', str(prepared),
                    '--output', str(output), '--harbor', '/fixtures/bin/harbor',
                    '--model', 'qwen3.8-flash', '--model-only'])
            receipt = json.loads((output / 'receipt.json').read_text())
        self.assertEqual(code, 0)
        self.assertFalse(receipt['passed'])
        self.assertEqual(receipt['status'], 'material-blocked')
        self.assertEqual(receipt['selected_task_names'], [task.name])
        self.assertEqual(receipt['material_gate']['eligible_task_names'], [])
        self.assertEqual(receipt['material_gate']['blocked_task_names'], [task.name])
        self.assertEqual(receipt['results'], [])
        self.assertNotIn('private-model-sentinel', json.dumps(receipt))


if __name__ == '__main__':
    unittest.main()
