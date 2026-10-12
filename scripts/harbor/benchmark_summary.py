#!/usr/bin/env python3
"""Read-only scalar summaries of native Harbor results and measured trial slots."""
import argparse
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import sys
import tempfile

PHASES = ('environment_setup', 'agent_setup', 'agent_execution', 'verifier')
USAGE = ('n_input_tokens', 'n_cache_tokens', 'n_output_tokens', 'cost_usd')
SLOT_EVENTS = {'admitted', 'disk_wait', 'stopping', 'cleaned', 'cleanup_error', 'finished'}
BINDINGS = ('effective_profile_sha256', 'candidate_profile_sha256', 'candidate_sha256',
            'profile_sha256', 'driver_sha256', 'runtime_sha256', 'harbor_revision')
PROTECTED_SOURCE_NAMES = {'result.json', 'continuous-events.jsonl', 'events.jsonl',
                          'campaign-monitor.json', 'resource-monitor.json', 'resource_monitor.json'}


def _read(path):
    raw = Path(path).read_bytes()
    return json.loads(raw), hashlib.sha256(raw).hexdigest()


def _number(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def _name(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9_.:-]{1,256}', value):
        raise ValueError('Invalid scalar identity')
    return value


def _duration(row):
    if not isinstance(row, dict) or not row.get('started_at') or not row.get('finished_at'):
        return None
    try:
        start = datetime.fromisoformat(row['started_at'].replace('Z', '+00:00'))
        end = datetime.fromisoformat(row['finished_at'].replace('Z', '+00:00'))
        seconds = (end - start).total_seconds()
        return seconds if _number(seconds) else None
    except (TypeError, ValueError, AttributeError):
        return None


def _binding(report, metadata):
    output = {}
    for source in (report, metadata or {}):
        for key in BINDINGS:
            if key not in source:
                continue
            value = source[key]
            hashes = value.values() if isinstance(value, dict) else [value]
            width = 40 if key == 'harbor_revision' else 64
            if any(not isinstance(item, str) or not re.fullmatch(r'[0-9a-f]{' + str(width) + '}', item)
                   for item in hashes):
                raise ValueError('Invalid profile binding digest')
            if isinstance(value, dict):
                for name in value:
                    _name(name)
            if key in output and output[key] != value:
                raise ValueError('Conflicting profile binding')
            output[key] = value
    return output


def _model_agent(agent):
    return agent.get('name') in ('terminus-2', 'computer-1') or agent.get('import_path') in (
        'dashscope_agent:DashScopeTerminus2', 'dashscope_agent:DashScopeComputer1')


def _statuses(trials):
    counts = dict.fromkeys(('passed', 'normal_zero_reward', 'exception', 'unknown'), 0)
    for row in trials:
        status = ('exception' if row['exception_type'] else 'passed' if row['reward'] == 1
                  else 'normal_zero_reward' if row['reward'] == 0 else 'unknown')
        counts[status] += 1
    return counts


def _caches(report):
    result = {}
    for key in ('dependencies', 'downloads'):
        binding = (report.get(key) or {}).get('binding_sha256')
        result[key] = {'prepared': None, 'bound': True if isinstance(binding, str) and
                       re.fullmatch(r'[0-9a-f]{64}', binding) else None, 'consumed': None}
    receipts = report.get('sandbox_receipts')
    rows = [row for row in (receipts or []) if row.get('start_mode') == 'template' and row.get('sandbox_id')]
    launches = {row['sandbox_id'] for row in rows}
    verified = {row['sandbox_id'] for row in rows if isinstance(row.get('initial_workspace_sha256'), str)
                and re.fullmatch(r'[0-9a-f]{64}', row['initial_workspace_sha256'])}
    preparation = (report.get('template_preparation') or {}).get('passed')
    result['templates'] = {'prepared': preparation if type(preparation) is bool else None,
        'bound': True if launches or (report.get('shared_templates') or {}).get('mode') in ('reuse', 'prepare') else None,
        'consumed': True if launches else None,
        'actual_distinct_launches': len(launches) if isinstance(receipts, list) else None,
        'initial_workspace_verified_launches': len(verified) if isinstance(receipts, list) else None}
    return result


def _slots(root, concurrency):
    paths = [root / 'continuous-events.jsonl', root / 'trials/events.jsonl', root / 'events.jsonl']
    events = []; source_sha = None
    for path in paths:
        if not path.is_file():
            continue
        raw = path.read_bytes(); digest = hashlib.sha256(raw).hexdigest()
        if source_sha is not None and source_sha != digest:
            raise ValueError('Conflicting continuous event sources')
        if source_sha is None:
            events = [json.loads(line) for line in raw.splitlines() if line.strip()]
            source_sha = digest
    measured = []
    for row in events:
        # Imports run in worker threads and report zero irrespective of live slots.
        # The shared preflight probe uses its own one-slot coordinator window.
        if row.get('event') not in SLOT_EVENTS or str(row.get('id', '')).startswith('preflight-'):
            continue
        stamp, active = row.get('time'), row.get('active_slots')
        if not _number(stamp) or type(active) is not int or active < 0:
            raise ValueError('Invalid slot sample')
        if concurrency is not None and active > concurrency:
            raise ValueError('Slot sample exceeds configured concurrency')
        if measured and stamp < measured[-1][0]:
            raise ValueError('Slot event time moved backwards')
        measured.append((stamp, active))
    result = {'window_started': None, 'window_finished': None, 'window_seconds': None,
        'occupied_slot_seconds': None, 'utilization_fraction': None, 'configured_slots': concurrency,
        'peak_active_slots': None, 'event_source_sha256': source_sha,
        'scope': 'trial slots within measured scheduler event window; preflight excluded'}
    if measured:
        result.update(window_started=measured[0][0], window_finished=measured[-1][0],
                      peak_active_slots=max(active for _, active in measured))
    if len(measured) >= 2:
        duration = measured[-1][0] - measured[0][0]
        occupied = sum((end[0] - start[0]) * start[1] for start, end in zip(measured, measured[1:]))
        result.update(window_seconds=duration, occupied_slot_seconds=occupied)
        if duration and concurrency:
            result['utilization_fraction'] = occupied / (duration * concurrency)
    return result


def summarize(report_path, *, metadata=None):
    """Return a deterministic summary; never modify or re-score any input."""
    report_path = Path(report_path)
    report, report_sha = _read(report_path); root = report_path.parent
    selected = report.get('selected_task_names', (report.get('prepared') or {}).get('task_names'))
    if not isinstance(selected, list) or len(set(selected)) != len(selected):
        raise ValueError('A fixed selected task list is required')
    selected = [_name(name) for name in selected]
    blocked = (report.get('material_gate') or {}).get('blocked_task_names', [])
    if not isinstance(blocked, list) or len(set(blocked)) != len(blocked) or not set(blocked) <= set(selected):
        raise ValueError('Material blocked tasks must belong to selected tasks')
    native = {}
    def add(row, source):
        identity = _name(row.get('id'))
        canonical = json.dumps(row, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()
        digest = hashlib.sha256(canonical).hexdigest()
        if identity in native:
            if native[identity]['digest'] != digest:
                raise ValueError('Conflicting native result identity')
            native[identity]['sources'].append(source)
        else:
            native[identity] = {'row': row, 'digest': digest, 'sources': [source]}
    for index, row in enumerate(report.get('results') or []):
        canonical = json.dumps(row, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()
        add(row, {'kind': 'report_embedded', 'report_sha256': report_sha, 'json_pointer': '/results/' + str(index),
                  'sha256': hashlib.sha256(canonical).hexdigest(), 'hash_encoding': 'canonical-json'})
    paths = sorted({*root.glob('jobs/*/*/result.json'), *root.glob('trials/*/jobs/*/*/result.json')})
    for path in paths:
        row, digest = _read(path)
        add(row, {'kind': 'native_file', 'path': path.relative_to(root).as_posix(),
                  'sha256': digest, 'hash_encoding': 'file-bytes'})
    trials = []
    for identity, item in sorted(native.items()):
        row = item['row']; task = _name(row.get('task_name', '').split('/')[-1])
        if task not in selected:
            raise ValueError('Native task is outside the fixed selected task list')
        usage = row.get('agent_result') or {}
        reward = ((row.get('verifier_result') or {}).get('rewards') or {}).get('reward')
        error = row.get('exception_info') or {}
        trials.append({'id': identity, 'task_name': task,
            'agent_name': _name((row.get('agent_info') or {}).get('name')),
            'is_model_evaluation': _model_agent({**((row.get('config') or {}).get('agent') or {}),
                                                'name': (row.get('agent_info') or {}).get('name')}),
            'reward': reward if _number(reward) else None,
            'exception_type': _name(error['exception_type']) if error.get('exception_type') else None,
            'wall_seconds': _duration(row), 'phase_seconds': {key: _duration(row.get(key)) for key in PHASES},
            'token_usage': {key: usage.get(key) if _number(usage.get(key)) else None for key in USAGE},
            'sources': item['sources']})
    model_scope = any(row['is_model_evaluation'] for row in trials) or any(
        _model_agent(agent) for agent in (report.get('effective_profile') or {}).get('agents', []))
    evaluated = [row for row in trials if row['is_model_evaluation']] if model_scope else trials
    controls = [row for row in trials if row['agent_name'] in
                ('oracle', 'nop', 'harness-control', 'computer-calibration-control')]
    executed = {row['task_name'] for row in evaluated}
    not_executed = [name for name in selected if name not in executed]
    phase_totals = {}; usage_totals = {}
    for fields, keys, output, total_key in (('phase_seconds', PHASES, phase_totals, 'known_seconds'),
                                           ('token_usage', USAGE, usage_totals, 'known_total')):
        for key in keys:
            known = [row[fields][key] for row in trials if row[fields][key] is not None]
            output[key] = {total_key: sum(known) if known else None,
                           'known_trials': len(known), 'missing_trials': len(trials) - len(known)}
    peaks = [report.get('max_live_vmm_count'), (report.get('resource_monitor') or {}).get('max_live_vmm_count')]
    monitor_sources = []
    for name in ('campaign-monitor.json', 'resource-monitor.json', 'resource_monitor.json'):
        path = root / name
        if path.is_file():
            monitor, digest = _read(path); peaks.append(monitor.get('max_live_vmm_count'))
            monitor_sources.append({'path': name, 'sha256': digest})
    measured_peaks = [value for value in peaks if type(value) is int and value >= 0]
    concurrency = (report.get('resource_plan') or {}).get('concurrency')
    if concurrency is not None and (type(concurrency) is not int or concurrency <= 0):
        raise ValueError('Invalid configured slot count')
    started, finished = report.get('started'), report.get('finished')
    wall = finished - started if _number(started) and _number(finished) and finished >= started else None
    return {'schema': 1, 'source_report_sha256': report_sha, 'binding': _binding(report, metadata),
        'selected_task_names': selected, 'material_blocked_task_names': blocked,
        'not_executed_task_names': not_executed,
        'coverage': {'selected_tasks': len(selected), 'executed_tasks': len(executed),
            'material_blocked_tasks': len(blocked), 'not_executed_tasks': len(not_executed),
            'other_not_executed_tasks': len(set(not_executed) - set(blocked)),
            'executed_task_fraction': len(executed) / len(selected) if selected else None},
        'coverage_scope': 'model_trials' if model_scope else 'all_native_trials',
        'trial_status_counts': _statuses(trials), 'evaluation_status_counts': _statuses(evaluated),
        'control_status_counts': _statuses(controls),
        'trials': trials, 'phase_totals': phase_totals, 'usage_totals': usage_totals,
        'input_tokens_include_cache': True, 'cost_usd_basis': 'native_framework_cost',
        'billing_cost_usd': None, 'report_wall_seconds': wall,
        'phase_totals_scope': 'sum of measured native trial phases; concurrent trials may overlap',
        'cache_evidence': _caches(report),
        'trial_slots': _slots(root, concurrency),
        'measured_vmm_peak': max(measured_peaks) if measured_peaks else None,
        'monitor_sources': monitor_sources,
        'unmeasured_phase_seconds': dict.fromkeys(('preparation', 'restore', 'install', 'cleanup'))}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.report.resolve() == args.output.resolve() or args.output.name in PROTECTED_SOURCE_NAMES:
            raise ValueError('Summary output must not overwrite native inputs')
        if args.output.is_symlink():
            raise FileExistsError('Summary output must not replace a symlink')
        if args.output.exists():
            existing, _ = _read(args.output)
            if (not isinstance(existing, dict) or existing.get('schema') != 1 or
                    not re.fullmatch(r'[0-9a-f]{64}', str(existing.get('source_report_sha256', ''))) or
                    not isinstance(existing.get('selected_task_names'), list) or
                    not isinstance(existing.get('trials'), list)):
                raise FileExistsError('Summary output already contains a different artifact')
        summary = summarize(args.report)
        with tempfile.NamedTemporaryFile('w', dir=args.output.parent, prefix='.summary-', delete=False) as stream:
            pending = Path(stream.name)
            try:
                json.dump(summary, stream, indent=2, allow_nan=False); stream.write('\n')
                stream.flush()
                pending.replace(args.output)
            finally:
                pending.unlink(missing_ok=True)
        return 0
    except (OSError, ValueError, TypeError, KeyError) as error:
        print('Benchmark summary failed (' + type(error).__name__ + ')', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
