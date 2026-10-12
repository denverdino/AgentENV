"""Guest overrides and admission for the shared Harbor lab."""
import math
import hashlib
import json
from pathlib import Path
import stat


def select_tasks(names, available):
    if not names or len(set(names)) != len(names) or not set(names).issubset(available):
        raise ValueError('Select a nonempty set of unique tasks from the frozen suite')
    return tuple(names)


def job_budget(phase_budget, task_count, agent_count, concurrency):
    return phase_budget * math.ceil(task_count * agent_count / concurrency)


def validate_receipt_resources(plan, images, receipts):
    by_name = {entry['environment_name']: task for task, entry in images.items()}
    for receipt in receipts:
        task = by_name[receipt['image']['environment_name']]
        expected = plan['tasks'][task]['effective']
        if receipt['cpu_count'] != expected['cpus'] or receipt['memory_mb'] != expected['memory_mb']:
            raise ValueError('Sandbox resource receipt does not match the planned task allocation')


def archive_budget_bytes(raw_manifest, expected_digest):
    if 'sha256:' + hashlib.sha256(raw_manifest).hexdigest() != expected_digest:
        raise ValueError('Archive size manifest does not match the frozen image digest')
    manifest = json.loads(raw_manifest)
    # The export retains compressed OCI blobs. Reserve tar/index overhead too.
    return sum(blob['size'] for blob in [manifest['config'], *manifest['layers']]) + 16 * 1024**2


def add_resource_arguments(parser):
    parser.add_argument('--override-cpus', type=int, help='Guest CPUs; default 4')
    parser.add_argument('--override-memory-mb', type=int, help='Guest MiB; default 8192')
    parser.add_argument('--n-concurrent-trials', type=int, default=8,
                        help='Maximum concurrent Harbor trials on this host; default 8')
    parser.add_argument('--task-resources', action='store_true',
                        help='Use original task CPU/memory instead of the 4c8g profile')


def plan_resources(args, tasks, controls=True):
    if not tasks:
        raise ValueError('No tasks selected')
    if args.task_resources and (args.override_cpus is not None or args.override_memory_mb is not None):
        raise ValueError('--task-resources cannot be combined with resource overrides')
    for value in (args.override_cpus, args.override_memory_mb, args.n_concurrent_trials):
        if value is not None and value <= 0:
            raise ValueError('CPU, memory and concurrency must be positive integers')
    if args.n_concurrent_trials > 8:
        raise ValueError('The fixed Harbor node capacity allows at most 8 concurrent sandboxes')
    overrides = {} if args.task_resources else {
        'override_cpus': args.override_cpus or 4,
        'override_memory_mb': args.override_memory_mb or 8192}
    planned = {name: {'declared': dict(config), 'effective': {
        'cpus': overrides.get('override_cpus', config.get('cpus') or 1),
        'memory_mb': overrides.get('override_memory_mb', config.get('memory_mb') or 2048)}}
        for name, config in tasks.items()}
    slots = min(args.n_concurrent_trials, len(tasks) * (2 if controls else 1))
    cpu = max(row['effective']['cpus'] for row in planned.values())
    memory = max(row['effective']['memory_mb'] for row in planned.values())
    storage = max(row['declared'].get('storage_mb') or 10240 for row in planned.values())
    # Node capacity is a host profile, independent of the selected batch.
    node = {'cpu_cores': 64, 'memory_gib': 128, 'pids': 8192}
    if slots * cpu + 4 > node['cpu_cores'] or math.ceil(slots * memory / 1024) + 16 > node['memory_gib']:
        raise ValueError('Concurrent trials exceed the fixed Harbor node capacity')
    planned_disk = max(20 * 1024**3, slots * storage * 1024**2)
    return {'overrides': overrides, 'concurrency': args.n_concurrent_trials,
            'max_active_trials': slots, 'tasks': planned,
            'node': node,
            'services_memory_gib': 32,
            'required_host_memory_bytes': (node['memory_gib'] + 32 + 16) * 1024**3,
            'reserve_disk_bytes': 20 * 1024**3, 'planned_disk_bytes': planned_disk,
            'required_disk_bytes': 20 * 1024**3 + planned_disk}


def check_capacity(plan, *, cpu_count, available_memory_bytes, free_disk_bytes):
    for name, available, required in (
            ('CPU', cpu_count, plan['node']['cpu_cores']),
            ('memory', available_memory_bytes, plan['required_host_memory_bytes']),
            ('disk', free_disk_bytes, plan['required_disk_bytes'])):
        if available < required:
            raise RuntimeError(f'Harbor {name} admission refused: available={available}, required={required}')


def effective_profile(plan, node_limits, *, agents):
    """Reject quota drift and smoke budgets before starting benchmark trials."""
    try:
        quota, period = map(int, node_limits['cpu.max'].split())
        actual = {'cpu_cores': quota / period,
                  'memory_gib': int(node_limits['memory.max']) / 1024**3,
                  'pids': int(node_limits['pids.max'])}
    except (KeyError, ValueError, ZeroDivisionError) as error:
        raise ValueError('Harbor effective node limits are missing or invalid') from error
    if actual != plan['node']:
        raise ValueError('Harbor effective node limits differ from the declared profile')
    profiles = []
    for agent in agents:
        kwargs = agent.get('kwargs') or {}
        terminus = (agent.get('name') == 'terminus-2' or
                    agent.get('import_path') == 'dashscope_agent:DashScopeTerminus2')
        if (agent.get('override_timeout_sec') is not None or
                agent.get('override_timeout_multiplier', 1) != 1 or
                (terminus and kwargs.get('max_turns') not in (None, 1000000))):
            raise ValueError('Harbor agent budget differs from the original phase policy')
        profiles.append({'name': agent.get('name'), 'import_path': agent.get('import_path'),
                         'model_name': agent.get('model_name'), 'max_turns': kwargs.get('max_turns'),
                         'max_tokens': (kwargs.get('llm_call_kwargs') or {}).get('max_tokens'),
                         'enable_summarize': kwargs.get('enable_summarize'),
                         'enable_thinking': ((kwargs.get('llm_call_kwargs') or {}).get('extra_body') or {}).get('enable_thinking')})
    return {'schema': 1, 'node': dict(plan['node']), 'guest_overrides': dict(plan['overrides']),
            'concurrency': plan['concurrency'], 'agents': profiles}


def check_ublk(module=Path('/sys/module/ublk_drv'), control=Path('/dev/ublk-control')):
    if not module.is_dir() or not control.exists() or not stat.S_ISCHR(control.stat().st_mode):
        raise RuntimeError('Harbor requires ublk host setup before creating the Lab; use the setup/restore controller')
