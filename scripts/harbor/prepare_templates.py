#!/usr/bin/env python3
"""Prepare clean, resource-bound Dragonball snapshots within one isolated lab."""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shlex
import time

from dependencies import json_bytes, task_digest
from scope import filter_microvm_tasks


def launch_key(binding):
    return hashlib.sha256(json_bytes({name: binding[name] for name in
        ('task_sha256', 'image_digest', 'resources', 'assets')})).hexdigest()


def validate_launch_binding(binding, image, resources, run_id, assets, source_hash, *, repository=None):
    expected = {'image_digest': image['source_platform_digest'],
                'resources': resources, 'assets': assets, 'task_sha256': source_hash}
    if binding.get('scope') == 'shared':
        if repository is None or binding.get('repository') != repository:
            raise ValueError('Shared template belongs to another repository')
    elif binding.get('scope', 'batch') == 'batch':
        expected['run_id'] = run_id
    else:
        raise ValueError('Unknown template binding scope')
    if (any(binding.get(name) != value for name, value in expected.items()) or
            not binding.get('snapshot_id') or binding.get('key') != launch_key(binding)):
        raise ValueError('Template binding has a stale run, source, resource or runtime identity')


def template_disk_bytes(plan, dependencies, *, mode='prepare'):
    if mode == 'reuse':
        # No builder: reserve lazy memory materialization only for active consumers.
        sizes = [row['effective']['memory_mb'] * 1024**2 for row in plan['tasks'].values()]
        templates = sum(sorted(sizes, reverse=True)[:plan['max_active_trials']])
    elif mode == 'prepare':
        templates = sum((row['effective']['memory_mb'] + (row['declared'].get('storage_mb') or 10240)) * 1024**2
                        for row in plan['tasks'].values())
    else:
        raise ValueError('Unknown template budget mode')
    return templates + dependency_disk_bytes(plan, dependencies)


def dependency_disk_bytes(plan, dependencies):
    # The controls-only job has one Oracle and one nop per selected task.
    copies = []
    for name in plan['tasks']:
        binding = dependencies['tasks'].get(name, {}).get('verifier')
        if binding:
            size = binding['spec']['max_bundle_bytes'] + binding['spec']['max_unpacked_bytes']
            copies.extend([size, size])
    return sum(sorted(copies, reverse=True)[:plan['max_active_trials']])


async def workspace_digest(environment, workdir):
    if not workdir or not workdir.startswith('/') or workdir == '/':
        raise ValueError('Reviewed template tasks require an explicit non-root workdir')
    result = await environment.exec("set -e -o pipefail; (find . -xdev -printf '%y %m %p %l\\0' | sort -z; "
        'find . -xdev -type f -print0 | sort -z | xargs -0 -r sha256sum) | sha256sum',
        cwd=workdir, user='root', timeout_sec=120)
    digest = result.stdout.strip().split()[0] if result.stdout.strip() else ''
    if result.return_code or len(digest) != 64:
        raise RuntimeError('Pristine task workspace digest failed')
    return digest


def make_environment(job, task_path, trial_dir):
    from agentenv_environment import AgentENVEnvironment
    from harbor.models.task.task import Task
    from harbor.models.trial.paths import TrialPaths
    from prepare_tasks import task_environment_name
    task = Task(task_path)
    config = task.config.environment
    overrides = {name: value for name, value in job['environment'].items()
                 if name in ('override_cpus', 'override_memory_mb')}
    # A builder creates pristine image state; environment files are uploaded only
    # when the independent trial starts, through the normal adapter lifecycle.
    return AgentENVEnvironment(environment_dir=task_path / 'environment',
        environment_name=task_environment_name(task_path), session_id='template-builder',
        task_env_config=config, network_policy=config.resolve_baseline(),
        trial_paths=TrialPaths(trial_dir), persistent_env=job['environment'].get('env', {}),
        **overrides, **job['environment']['kwargs'])


async def prepare_templates(job, run_id, assets, output, *, environment_factory=None, shared_store=None, shared_mode=None):
    output = Path(output)
    output.mkdir()
    images = json.loads(Path(job['environment']['kwargs']['images_path']).read_text())
    report = {'schema': 1, 'run_id': run_id, 'assets': assets, 'started': time.time(),
              'tasks': {}, 'builders': [], 'cache_hits': [], 'passed': False}
    if (shared_store is None) != (shared_mode is None) or shared_mode not in (None, 'prepare', 'reuse'):
        raise ValueError('Shared template store and mode must be specified together')
    factory = environment_factory or make_environment
    try:
        for entry in job['tasks']:
            task = Path(entry['path'])
            if filter_microvm_tasks([task.name])[1]:
                raise ValueError('Template task is excluded from the MicroVM scope')
            if shared_store:
                from prepare_tasks import task_environment_name
                image_name = task_environment_name(task)
                image = images[image_name]
                overrides = job['environment']
                if not all(name in overrides for name in ('override_cpus', 'override_memory_mb')):
                    from harbor.models.task.task import Task
                    declared = Task(task).config.environment
                    resources = {'cpus': overrides.get('override_cpus', declared.cpus or 1),
                                 'memory_mb': overrides.get('override_memory_mb', declared.memory_mb or 2048)}
                else:
                    resources = {'cpus': overrides['override_cpus'], 'memory_mb': overrides['override_memory_mb']}
                identity = {'task_sha256': task_digest(task), 'image_digest': image['source_platform_digest'],
                            'resources': resources, 'assets': assets}
                binding = await asyncio.to_thread(shared_store.lookup, launch_key(identity))
                if binding:
                    validate_launch_binding(binding, image, resources, run_id, assets,
                        identity['task_sha256'], repository=shared_store.repository)
                    report['tasks'][image_name] = binding
                    report['cache_hits'].append(task.name)
                    continue
                if shared_mode == 'reuse':
                    raise ValueError('Shared template missing; prepare before starting consumers: ' + task.name)
            environment = factory(job, task, output / task.name)
            row = {'task_name': task.name, 'started': time.time(), 'cleanup': 'pending'}
            report['builders'].append(row)
            try:
                async with asyncio.timeout(600):
                    await environment._create_sandbox()
                    row['sandbox_id'] = environment.sandbox['sandboxID']
                    identity = await environment.exec('printf %s "$E2B_SANDBOX_ID"', user='root', timeout_sec=15)
                    if identity.return_code or identity.stdout != row['sandbox_id']:
                        raise RuntimeError('Template builder guest identity mismatch')
                    clean = await environment.exec('test ! -e /solution && test ! -e /tests && '
                        'test ! -e /logs/verifier/reward.txt && '
                        'for name in DASHSCOPE_API_KEY OPENAI_API_KEY DOCKER_AUTH_CONFIG ACR_PASSWORD; '
                        'do test -z "${!name+x}" || exit 1; done', user='root', timeout_sec=15)
                    if task.name == 'fix-ocaml-gc':
                        original = await environment.exec('test ! -e /app/ocaml-original', user='root', timeout_sec=15)
                        if original.return_code:
                            raise RuntimeError('Template builder contains OCaml verifier data')
                    if clean.return_code:
                        raise RuntimeError('Template builder contains forbidden trial data or credentials')
                    image = images[environment.environment_name]
                    workspace = await workspace_digest(environment, image.get('workdir'))
                    resources = {'cpus': environment._effective_cpus, 'memory_mb': environment._effective_memory_mb}
                    capture_started = time.monotonic()
                    response = await environment._request('POST', '/sandboxes/' + row['sandbox_id'] + '/snapshots', json={})
                    snapshot = response.json()
                    row['snapshot_id'] = snapshot['snapshotID']
                    if snapshot['cpuCount'] != resources['cpus'] or snapshot['memoryMB'] != resources['memory_mb']:
                        raise ValueError('Captured template resource mismatch')
                    binding = {'run_id': run_id, 'task_name': task.name, 'task_sha256': task_digest(task),
                        'image_digest': image['source_platform_digest'], 'resources': resources, 'assets': assets,
                        'snapshot_id': row['snapshot_id'], 'source_sandbox_id': row['sandbox_id'],
                        'workspace_sha256': workspace}
                    binding['key'] = launch_key(binding)
                    if shared_store:
                        binding.update(scope='shared', repository=shared_store.repository)
                        await asyncio.to_thread(shared_store.publish, binding)
                    report['tasks'][environment.environment_name] = binding
                    row['capture_seconds'] = time.monotonic() - capture_started
            finally:
                await environment.stop(delete=True)
                row.update(cleanup='passed', finished=time.time())
        report['passed'] = True
        return report
    finally:
        report['finished'] = time.time()
        (output / 'receipt.json').write_text(json.dumps(report, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--job', type=Path, required=True)
    parser.add_argument('--assets', type=Path, required=True)
    parser.add_argument('--run-id', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--oss-env', type=Path)
    parser.add_argument('--repository', type=Path, help='Public shared OSS repository identity')
    parser.add_argument('--shared-mode', choices=('prepare', 'reuse'))
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error('Template preparation must run as the non-root test account')
    store = None
    if args.shared_mode:
        if not args.oss_env or not args.repository:
            parser.error('Shared templates require private OSS input and public repository identity')
        from shared_templates import open_store
        store = open_store(args.oss_env, json.loads(args.repository.read_text()))
    try:
        report = asyncio.run(prepare_templates(json.loads(args.job.read_text()), args.run_id,
            json.loads(args.assets.read_text()), args.output, shared_store=store, shared_mode=args.shared_mode))
    finally:
        if store:
            store.client.close()
    print(json.dumps({'passed': report['passed'], 'template_count': len(report['tasks'])}))


if __name__ == '__main__':
    main()
