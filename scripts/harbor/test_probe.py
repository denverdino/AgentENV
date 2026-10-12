"""Preflight resources, phase deadlines, diagnostics and cleanup."""
import asyncio
import json
import shlex
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import probe
from harbor.models.task.config import EnvironmentConfig


class ProbeTests(unittest.TestCase):
    def job(self):
        return {'tasks': [{'path': '/fixtures/task'}], 'environment': {
            'override_cpus': 4, 'override_memory_mb': 8192,
            'env': {}, 'kwargs': {'api_url': 'http://127.0.0.1:8000', 'images_path': '/images.json'}}}

    def task(self, startup=600):
        return SimpleNamespace(config=SimpleNamespace(environment=EnvironmentConfig(
            cpus=1, memory_mb=2048, workdir='/app', build_timeout_sec=startup)))

    def test_probe_uses_task_policy_and_job_resource_overrides(self):
        task = self.task()
        job = self.job()
        shared = {'templates_path': '/templates.json', 'template_run_id': 'consumer',
                  'runtime_assets': {'runner': 'a' * 64},
                  'template_repository': {'bucket': 'shared', 'prefix': 'harbor/shared'}}
        job['environment']['kwargs'].update(shared)
        with patch.object(probe, 'Task', return_value=task), \
             patch.object(probe, 'task_environment_name', return_value='task'), \
             patch.object(probe, 'AgentENVEnvironment') as adapter:
            probe.make_probe_environment(job, Path('/output'))
        kwargs = adapter.call_args.kwargs
        self.assertIs(kwargs['task_env_config'], task.config.environment)
        self.assertEqual(kwargs['override_cpus'], 4)
        self.assertEqual(kwargs['override_memory_mb'], 8192)
        self.assertEqual(kwargs['network_policy'], task.config.environment.resolve_baseline())
        for name, value in shared.items():
            self.assertEqual(kwargs[name], value)

    def test_outer_deadline_includes_build_transport_and_cleanup(self):
        with patch.object(probe, 'Task', return_value=self.task()):
            self.assertEqual(probe.probe_outer_timeout(self.job()), 855)

    def test_startup_timeout_records_phase_and_deletes_sandbox(self):
        stopped = []
        environment = SimpleNamespace(_effective_cpus=4, _effective_memory_mb=8192)

        async def start(force_build):
            await asyncio.sleep(1)

        async def stop(delete):
            stopped.append(delete)

        environment.start = start
        environment.stop = stop
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(probe, 'Task', return_value=self.task(0.01)), \
             patch.object(probe, 'make_probe_environment', return_value=environment):
            output = Path(directory) / 'probe'
            with self.assertRaises(TimeoutError):
                asyncio.run(probe.run_probe(self.job(), output))
            receipt = json.loads((output / 'checks.json').read_text())
        self.assertEqual(receipt['failed_phase'], 'startup')
        self.assertFalse(receipt['passed'])
        self.assertEqual(receipt['cleanup'], 'passed')
        self.assertEqual(stopped, [True])

    def test_guest_python_egress_script_compiles(self):
        async def execute(command, **kwargs):
            if command.startswith('python3 -c '):
                compile(shlex.split(command)[2], '<guest-egress-probe>', 'exec')
                raise RuntimeError('stop-after-compile')
            return SimpleNamespace(return_code=0, stdout='/usr/bin/python3', stderr='')

        environment = SimpleNamespace(exec=execute,
            network_policy=SimpleNamespace(network_mode=SimpleNamespace(value='all')))
        job = self.job()
        job['environment']['env']['https_proxy'] = 'http://198.18.0.1:3128'
        with self.assertRaisesRegex(RuntimeError, 'stop-after-compile'):
            asyncio.run(probe.check_transport(environment, job, Path('/output'), {}))

    def test_cleanup_failure_does_not_replace_startup_failure(self):
        async def start(force_build):
            raise ValueError('original startup failure')

        async def stop(delete):
            raise RuntimeError('cleanup failure')

        environment = SimpleNamespace(start=start, stop=stop,
            _effective_cpus=4, _effective_memory_mb=8192)
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(probe, 'Task', return_value=self.task()), \
             patch.object(probe, 'make_probe_environment', return_value=environment):
            output = Path(directory) / 'probe'
            with self.assertRaisesRegex(ValueError, 'original startup failure'):
                asyncio.run(probe.run_probe(self.job(), output))
            receipt = json.loads((output / 'checks.json').read_text())
        self.assertEqual(receipt['failed_phase'], 'startup')
        self.assertEqual(receipt['cleanup'], 'failed')
        self.assertEqual(receipt['cleanup_error_type'], 'RuntimeError')


if __name__ == '__main__':
    unittest.main()
