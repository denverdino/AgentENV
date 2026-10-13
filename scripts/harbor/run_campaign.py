#!/usr/bin/env python3
"""One persistent Dragonball Lab, native single-trial Harbor Jobs, continuous refill."""
import argparse
import asyncio
import copy
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

from continuous import run_continuous
from disk_budget import DiskBudget, GIB, task_disk_budget


def native_job(base, trial, prepared, output):
    if not re.fullmatch(r'[A-Za-z0-9_.-]+', str(trial['id'])):
        raise ValueError('Invalid native trial directory identity')
    job = copy.deepcopy(base)
    job.update(job_name='trial-' + str(trial['id']), jobs_dir=str(output / str(trial['id']) / 'jobs'),
               n_concurrent_trials=1, n_attempts=1, retry={'max_retries': 0},
               tasks=[{'path': str(prepared / 'tasks' / trial['task'])}], agents=[trial['agent']])
    return job


def verify_trial_cleanup(directory, live_sandboxes):
    directory = Path(directory)
    receipts = [json.loads(p.read_text()) for p in directory.rglob('agentenv.json')]
    if not receipts and any((directory / marker).exists() for marker in ('spawned.json', 'probe-spawned.json')):
        raise ValueError('Native Job has no sandbox deletion receipt; creation state is unknown')
    live = {s['sandboxID'] for s in live_sandboxes}
    if any(r['state'] != 'deleted' or r['sandbox_id'] in live for r in receipts):
        raise ValueError('Native Job sandbox deletion is not confirmed')
    return True


def trial_result_paths(directory):
    return sorted(Path(directory).glob('jobs/*/*/result.json'))


def native_trial_errors(directory, trial):
    paths = trial_result_paths(directory)
    if len(paths) != 1:
        return [{'error': 'Expected exactly one native trial result', 'paths': list(map(str, paths))}]
    row = json.loads(paths[0].read_text())
    if row.get('task_name', '').split('/')[-1] != trial['task']:
        return [{'error': 'Native result belongs to another task', 'path': str(paths[0])}]
    if row.get('exception_info'):
        return [{'path': str(paths[0]), 'exception_info': row['exception_info']}]
    verifier = row.get('verifier_result') or {}
    expected = trial.get('agent', {}).get('name')
    allowed = (1,) if expected == 'oracle' else (0,) if expected == 'nop' else (0, 1)
    if (verifier.get('rewards') or {}).get('reward') not in allowed:
        return [{'error': 'Missing or invalid native verifier reward', 'path': str(paths[0])}]
    return []


def run_process(command, *, env, log, timeout, cancelled, grace=60):
    if cancelled(): return {'exit_code': None, 'interruption': 'cancelled', 'not_started': True}
    with Path(log).open('w') as stream:
        child = subprocess.Popen(command, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + timeout; terminated = None; reason = None
        try:
            while child.poll() is None:
                if cancelled() or time.monotonic() > deadline:
                    if terminated is None:
                        reason = 'cancelled' if cancelled() else 'deadline'
                        os.killpg(child.pid, signal.SIGTERM); terminated = time.monotonic()
                if terminated is not None and time.monotonic() - terminated > grace:
                    os.killpg(child.pid, signal.SIGKILL)
                time.sleep(.1)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)
    return {'exit_code': child.returncode, 'interruption': reason}


def registry_environment(environment, config_path):
    # These commands address only the Lab registry. The public/model proxy is
    # deliberately outside its namespace and cannot route the registry address.
    proxies = {'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'}
    return {**{key: value for key, value in environment.items() if key not in proxies},
            'REGCTL_CONFIG': str(config_path)}


def stop_campaign(child, output):
    # Node.spawn may return nsenter's parent PID. Its SIGTERM does not forward
    # to the coordinator in the guest PID namespace. Keep that wrapper alive
    # until the worker has boundedly stopped Jobs and saved its native report.
    (Path(output) / 'cancel-request.json').write_text(json.dumps({'time': time.time(), 'reason': 'cancelled'}))
    try: child.wait(timeout=75)
    except subprocess.TimeoutExpired: child.kill(); child.wait(timeout=5)


def allocated_bytes(path):
    """Physical blocks of an owned tree; never count virtual file lengths."""
    path = Path(path)
    if not path.exists(): return 0
    seen = set(); total = 0
    for p in [path, *path.rglob('*')]:
        if p.is_symlink(): raise ValueError('Owned disk evidence contains a symlink')
        try: s = p.stat()
        except FileNotFoundError: continue
        key = (s.st_dev, s.st_ino)
        if key not in seen:
            seen.add(key); total += s.st_blocks * 512
    return total


def template_layers(store, binding):
    body = store.client.get_object(Bucket=store.bucket,
        Key=store.prefix + '/catalog/records/' + binding['snapshot_id'] + '.json')['Body']
    try: raw = body.read(1024**2 + 1)
    finally: body.close()
    if len(raw) > 1024**2: raise ValueError('Snapshot metadata exceeds budget')
    record = json.loads(raw)
    if record['id'] != binding['snapshot_id'] or record['resources'] != {
            'cpu_count': binding['resources']['cpus'], 'memory_mib': binding['resources']['memory_mb'],
            'disk_size_mib': 65536}:
        raise ValueError('Snapshot resource identity changed')
    committed = record['committed']
    if committed.get('attached_drives') or committed.get('volume_snapshots'):
        raise ValueError('Harbor pristine templates must not have additional disks')
    layers = [row['Managed'] for row in committed['rootfs_layers'] if 'Managed' in row]
    if len(layers) != len(committed['rootfs_layers']):
        raise ValueError('Campaign currently requires durable managed rootfs layers')
    layers += committed['memory_layers']
    result = {}
    for row in layers:
        digest, size = row['digest'], row['size']
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', digest) or type(size) is not int or size <= 0:
            raise ValueError('Invalid committed layer size/identity')
        if digest in result and result[digest] != size: raise ValueError('Conflicting layer sizes')
        result[digest] = size
    return result


def worker_main(path):
    if os.geteuid() == 0: raise ValueError('Campaign workers must run as the non-root test account')
    spec = json.loads(Path(path).read_text()); output = Path(spec['output'])
    model_key = os.environ.pop('OPENAI_API_KEY', None)
    os.environ.pop('DASHSCOPE_API_KEY', None)
    from run_lab import sha
    from shared_templates import open_store
    from prepare_templates import prepare_templates, launch_key, validate_launch_binding
    from dependencies import task_digest
    import httpx
    stop = threading.Event()
    def cancelled():
        if (output / 'cancel-request.json').exists(): stop.set()
        return stop.is_set()
    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, lambda *_: stop.set())
    budget = DiskBudget(20 * GIB, lambda: shutil.disk_usage(output).free)
    events_path = output / 'events.jsonl'; imported = {}; lock = threading.Lock()
    prepared = spec['prepared']; base = spec['base']; reports = {}; layers_by_task = {}
    client = httpx.Client(base_url=base['environment']['kwargs']['api_url'],
                         headers={'X-API-Key': os.environ['AENV_API_KEY']}, timeout=30, trust_env=False)
    def live():
        r = client.get('/sandboxes'); r.raise_for_status(); return r.json()
    def record(event):
        with events_path.open('a') as stream: stream.write(json.dumps(event) + '\n')
    try:
        # Read immutable metadata before VM admission. Reuse never invokes a builder.
        with_store = open_store(spec['oss_env'], spec['repository'])
        try:
            for name in prepared['task_names']:
                if stop.is_set(): break
                image = prepared['images'][name]; resource = spec['resource_plan']['tasks'][name]['effective']
                identity = {'task_sha256': task_digest(Path(spec['prepared_path']) / 'tasks' / name),
                            'image_digest': image['source_platform_digest'], 'resources': resource,
                            'assets': spec['runtime_assets']}
                binding = with_store.lookup(launch_key(identity))
                if binding is None: raise ValueError('Shared template missing: ' + name)
                validate_launch_binding(binding, image, resource, spec['run_id'], spec['runtime_assets'],
                                        identity['task_sha256'], repository=spec['repository'])
                reports[name] = binding; layers_by_task[name] = template_layers(with_store, binding)
        finally: with_store.client.close()
        trials = []
        agents = spec['agents']
        items = [(name, agent) for name in prepared['task_names'] for agent in agents]
        if spec.get('acceptance_trials'):
            if len(prepared['task_names']) != 1: raise ValueError('Acceptance repeats require exactly one frozen task')
            items = [(prepared['task_names'][0], {'import_path': 'harness_control:HarnessControl',
                'kwargs': {'delay_seconds': 1 if i in (0, 8) else 45}})
                for i in range(spec['acceptance_trials'])]
        for index, (name, agent) in enumerate(items):
            image = prepared['images'][name]; resource = spec['resource_plan']['tasks'][name]
            details = task_disk_budget(resource['declared'], memory_mb=resource['effective']['memory_mb'],
                template_mode='reuse', archive_bytes=0, cached_bytes=0)
            # Runtime COW/materialization is private. Committed source layers and
            # OCI imports are content-keyed shared allocations, with exact sizes.
            shared = {'layer:' + digest: size + 1024**2 for digest, size in layers_by_task[name].items()}
            shared['oci:' + image['source_platform_digest']] = Path(image['archive']).stat().st_size + 16 * 1024**2
            trials.append({'id': f'{index:05d}-{name}', 'task': name, 'agent': agent,
                           'disk_bytes': details['future_bytes'], 'shared': shared, 'disk_budget': details})
        def prepare(trial):
            name = trial['task']; directory = output / trial['id']; directory.mkdir()
            image = prepared['images'][name]
            with lock:
                if image['source_platform_digest'] not in imported:
                    if sha(image['archive']) != image['archive_sha256']: raise ValueError('OCI archive changed')
                    before = allocated_bytes(spec['registry_path'])
                    ref = f"10.241.0.1:5443/harbor/{name}:candidate"
                    command = [spec['regctl'], 'image', 'import', ref, image['archive']]
                    imported_result = run_process(command, timeout=1800, grace=5,
                        env=registry_environment(os.environ, spec['regctl_config']), log=directory / 'regctl.log',
                        cancelled=lambda: cancelled() or budget.snapshot()['low_watermark'])
                    if imported_result['exit_code'] != 0: raise RuntimeError('OCI import interrupted or failed')
                    digest = subprocess.check_output([spec['regctl'], 'image', 'digest', ref], timeout=60,
                        env=registry_environment(os.environ, spec['regctl_config']), text=True).strip()
                    if digest != image['source_platform_digest']: raise ValueError('Imported OCI digest changed')
                    imported[digest] = ref.split(':candidate')[0] + '@' + digest
                    materialized = max(0, allocated_bytes(spec['registry_path']) - before)
                    budget.materialized('oci:' + digest, min(materialized, trial['shared']['oci:' + digest]))
                    record({'event': 'imported', 'id': trial['id'], 'time': time.time(), 'active_slots': 0,
                            'digest': digest, 'owned_registry_delta_bytes': materialized})
            images = {image['environment_name']: {**image, 'image': imported[image['source_platform_digest']]}}
            images_path = directory / 'images.json'; images_path.write_text(json.dumps(images, indent=2))
            template_path = directory / 'templates.json'
            template_path.write_text(json.dumps({'schema': 1, 'passed': True, 'builders': [],
                'cache_hits': [name], 'tasks': {image['environment_name']: reports[name]}}))
            job = native_job(base, trial, Path(spec['prepared_path']), output)
            if job['verifier'].get('kwargs', {}).get('downloads_path'):
                bindings = json.loads(Path(job['verifier']['kwargs']['downloads_path']).read_text())
                matching = {url: row for url, row in bindings['downloads'].items() if row['task_name'] == name}
                download_path = directory / 'downloads.json'
                download_path.write_text(json.dumps({'schema': 1, 'downloads': matching}, indent=2))
                job['verifier']['kwargs']['downloads_path'] = str(download_path)
            cfg = spec['task_configs'][name]
            phase_budget = cfg['environment']['build_timeout_sec'] + (cfg['agent']['timeout_sec'] or 900) + cfg['verifier']['timeout_sec'] + 900
            job['environment']['kwargs'].update(images_path=str(images_path), templates_path=str(template_path),
                template_run_id=spec['run_id'], runtime_assets=spec['runtime_assets'], template_repository=spec['repository'],
                request_timeout_sec=phase_budget, sandbox_timeout_sec=int(phase_budget + 300))
            config_path = directory / 'job.json'; config_path.write_text(json.dumps(job, indent=2))
            return {'job': config_path, 'budget': phase_budget}
        def execute(trial, p, stopping):
            directory = output / trial['id']; env = dict(os.environ)
            if 'model_name' in trial['agent']:
                if not model_key: raise ValueError('Model credential missing')
                env['OPENAI_API_KEY'] = model_key
            if trial.get('purpose') == 'probe':
                marker = 'probe-spawned.json'
                from probe import probe_outer_timeout
                command = [str(Path(spec['harbor']).parent / 'python'), '-B',
                           str(Path(__file__).with_name('probe.py')), str(p['job'])]
                if spec.get('pause_taskvm_during_inference'):
                    command.append('--pause-resume')
                timeout = probe_outer_timeout(json.loads(p['job'].read_text()))
            else:
                marker = 'spawned.json'; timeout = p['budget']
                command = [spec['harbor'], 'run', '--config', str(p['job'])]
            (directory / marker).write_text(json.dumps({'started': time.time()}))
            result = run_process(command, env=env, log=directory / 'harbor.log', timeout=timeout,
                                 cancelled=lambda: stopping.is_set() or cancelled())
            if result.get('not_started'): (directory / marker).unlink()
            result['results'] = [str(path) for path in trial_result_paths(directory)]
            if trial.get('purpose') == 'probe':
                result['probe'] = json.loads((directory / 'transport-probe/checks.json').read_text())
            elif not result['interruption']:
                result['native_errors'] = native_trial_errors(directory, trial)
            if trial.get('purpose') != 'probe' and not result['results'] and not result['interruption']:
                raise RuntimeError('Native Job produced no result')
            return result
        def cleanup(trial):
            directory = output / trial['id']; directory.mkdir(exist_ok=True)
            verified = verify_trial_cleanup(directory, live())
            # Credit only verified committed cache directories, never global free
            # space deltas or private uppers. Unavailable proof stays reserved.
            for digest, size in layers_by_task[trial['task']].items():
                cache = Path(spec['node_home']) / 'image-cache/commits' / digest.replace(':', '-')
                if cache.exists():
                    key = 'layer:' + digest
                    with budget.lock:
                        current = budget.leases[key]['materialized']
                        budget.materialized(key, max(current, min(allocated_bytes(cache), size + 1024**2)))
            (directory / 'cleanup.json').write_text(json.dumps({'verified': verified, 'time': time.time(),
                'capacity': budget.snapshot()}, indent=2))
            return verified
        probe_trial = {**trials[0], 'id': 'preflight-' + trials[0]['task'], 'purpose': 'probe', 'agent': {'name': 'nop'}}
        preflight = run_continuous([probe_trial], concurrency=1, prepare=prepare, execute=execute,
            verify_cleanup=cleanup, capacity=budget, record=record, cancelled=cancelled)
        (output / 'preflight.json').write_text(json.dumps(preflight, indent=2))
        if (preflight['reason'] != 'completed' or preflight['cleanup_errors'] or
                any(row.get('error') or row.get('result', {}).get('exit_code') != 0 or
                    not row.get('result', {}).get('probe', {}).get('passed') for row in preflight['completed'])):
            preflight.update(passed=False, pending=trials, remaining_sandboxes=live())
            (output / 'campaign.json').write_text(json.dumps(preflight, indent=2))
            return 1
        result = run_continuous(trials, concurrency=spec['resource_plan']['concurrency'],
            prepare=prepare, execute=execute, verify_cleanup=cleanup, capacity=budget, record=record, cancelled=cancelled)
        result['passed'] = (result['reason'] == 'completed' and not result['cleanup_errors'] and not result['interrupted'] and
            all(not row.get('error') and not row.get('result', {}).get('native_errors') and row.get('result', {}).get('exit_code') == 0 for row in result['completed']))
        result['trial_errors'] = [error for row in result['completed'] for error in row.get('result', {}).get('native_errors', [])]
        result['remaining_sandboxes'] = live()
        if result['remaining_sandboxes']: result['passed'] = False
        (output / 'campaign.json').write_text(json.dumps(result, indent=2))
        return 0 if result['passed'] else 1
    finally: client.close()


def run_trials(args, lab, node, inputs, prepared, plan, configs, runtime_assets, proxy_env, repository, model_key, runner_path):
    from run_lab import model_agent_config
    base = {'environment': {'import_path': 'agentenv_environment:AgentENVEnvironment', **plan['overrides'],
        'env': proxy_env, 'kwargs': {'api_url': f'http://{node.address}:8000'}},
        'verifier': {'import_path': 'failure_evidence:EvidenceVerifier'}}
    if args.model:
        base['environment']['kwargs'].update(material_manifest=args.material_manifest,
                                            material_required=True)
    if args.downloads: base['verifier']['kwargs'] = {'downloads_path': str(args.output / 'downloads.json')}
    spec = {'output': str(args.output / 'trials'), 'prepared': prepared, 'prepared_path': str(args.prepared),
        'base': base, 'resource_plan': plan, 'task_configs': configs, 'runtime_assets': runtime_assets,
        'repository': repository, 'oss_env': str(args.oss_env), 'run_id': lab.root.name,
        'regctl': inputs['regctl'], 'regctl_config': str(node.root / 'regctl.json'),
        'registry_path': str(lab.root / 'repo/registry'), 'node_home': str(node.root / 'home'),
        'harbor': str(args.harbor), 'acceptance_trials': args.acceptance_trials,
        'pause_taskvm_during_inference': args.pause_taskvm_during_inference,
        'agents': ([model_agent_config(args.model, args.pause_taskvm_during_inference)]
                   if args.model else [{'name': 'oracle'}, {'name': 'nop'}])}
    output = Path(spec['output']); output.mkdir();
    if output.stat().st_dev != lab.root.stat().st_dev: raise ValueError('Campaign output and runtime must share the budget filesystem')
    os.chown(output, lab.account.pw_uid, lab.account.pw_gid)
    path = args.output / 'campaign-input.json'; path.write_text(json.dumps(spec, indent=2)); path.chmod(0o644)
    command = ['env', 'AENV_API_KEY=' + __import__('dragonball_m4_scenarios').API_KEY,
        'PYTHONPATH=' + str(Path(__file__).parent), 'HOME=' + lab.account.pw_dir,
        *(key + '=' + value for key, value in proxy_env.items()),
        str(args.harbor.parent / 'python'), '-B', __file__, '--worker-input', str(path)]
    if args.model: os.environ['OPENAI_API_KEY'] = model_key
    try: child = node.spawn(command, args.output / 'campaign.log')
    finally: os.environ.pop('OPENAI_API_KEY', None)
    peak = 0; observed = []; cancelled = False
    free_start = shutil.disk_usage(lab.root).free; min_free = free_start
    try:
        while child.poll() is None:
            min_free = min(min_free, shutil.disk_usage(lab.root).free)
            from run_lab import observe_vmm
            live = [row for pid in (node.cgroup / 'cgroup.procs').read_text().split()
                if (row := observe_vmm(Path('/proc') / pid, runner_path, lab.account.pw_uid))]
            peak = max(peak, len(live))
            for row in live:
                if row['pid'] not in {item['pid'] for item in observed}: observed.append(row)
            if peak > 8: raise RuntimeError('Campaign exceeded eight live VMMs')
            time.sleep(.1)
    except BaseException:
        cancelled = True; stop_campaign(child, output)
        raise
    finally:
        (args.output / 'campaign-monitor.json').write_text(json.dumps({'max_live_vmm_count': peak,
            'vmm_observed': observed, 'cancelled': cancelled, 'free_start_bytes': free_start,
            'min_free_bytes': min_free, 'observed_host_disk_delta_bytes': max(0, free_start - min_free)}, indent=2))
    result = json.loads((output / 'campaign.json').read_text())
    receipts = [json.loads(p.read_text()) for p in output.rglob('agentenv.json')]
    from resources import validate_receipt_resources
    validate_receipt_resources(plan, prepared['images'], receipts)
    return {'continuous': result, 'passed': result['passed'], 'max_live_vmm_count': peak, 'vmm_observed': observed,
        'sandbox_receipts': receipts, 'results': [json.loads(p.read_text()) for directory in output.iterdir() if directory.is_dir() for p in trial_result_paths(directory)],
        'harbor_exit_codes': {'campaign': child.returncode}, 'remaining_sandboxes': result['remaining_sandboxes']}


def main():
    if len(sys.argv) == 3 and sys.argv[1] == '--worker-input': return worker_main(sys.argv[2])
    from run_lab import main as run_lab
    return run_lab([*sys.argv[1:], '--continuous', '--template-start'])


if __name__ == '__main__': raise SystemExit(main())
