#!/usr/bin/env python3
"""Check shared-template identities, scores and exact local-cache boundaries."""
import argparse
from collections import Counter
import json
import os
from pathlib import Path

from prepare_templates import launch_key


def validate_shared_runs(cells, *, runtime_uid):
    identities = set();bindings = {};runs = set();trial_count = 0;cold_restores = 0;expected_keys = None
    def require(condition, field):
        if not condition:raise ValueError('Shared qualification rejected: ' + field)
    def sandbox(row, candidates):
        binding = candidates.get(row.get('template_id'))
        require(binding is not None, 'snapshot ID')
        require(row.get('start_mode') == 'template' and row.get('state') == 'deleted', 'sandbox lifecycle')
        identity = row.get('sandbox_id')
        require(bool(identity) and identity not in identities and
                identity not in {value['source_sandbox_id'] for value in candidates.values()}, 'sandbox identity')
        identities.add(identity)
        require(row.get('template_key') == binding['key'] and
                row.get('initial_workspace_sha256') == binding['workspace_sha256'], 'template/workspace binding')
        require(row.get('cpu_count') == binding['resources']['cpus'] and
                row.get('memory_mb') == binding['resources']['memory_mb'], 'sandbox resources')
    require(cells and runtime_uid > 0, 'non-root workload and nonempty run set')
    for receipt, probe in cells:
        require(receipt.get('passed') is True and receipt.get('cleanup') == 'passed', 'Lab completion')
        require(receipt['root'] not in runs, 'duplicate Lab');runs.add(receipt['root'])
        store = receipt['snapshot_store'];mode = receipt['shared_templates']['mode']
        require(store.get('backend') == 'aliyun-oss' and mode in ('prepare', 'reuse'), 'shared OSS mode')
        preparation = receipt['template_preparation'];current = preparation['tasks']
        require(preparation.get('passed') is True and bool(current), 'template preparation')
        require(all(row.get('cleanup') == 'passed' for row in preparation['builders']), 'builder cleanup')
        current_keys = {binding['key'] for binding in current.values()}
        if expected_keys is None:expected_keys = current_keys
        require(current_keys == expected_keys, 'cross-run task/resource/asset alignment')
        candidates = {}
        for binding in current.values():
            require(binding.get('scope') == 'shared' and binding.get('key') == launch_key(binding), 'template key')
            require(all(store.get(name) == value for name, value in binding['repository'].items()), 'repository')
            require(binding['assets'] == receipt['runtime_sha256'], 'runtime assets')
            key = binding['key']
            if key in bindings:require(bindings[key] == binding, 'cross-run snapshot binding')
            bindings[key] = binding;candidates[binding['snapshot_id']] = binding
        expected = Counter((binding['task_name'], agent) for binding in current.values() for agent in ('oracle', 'nop'))
        require(len(candidates) == len(current), 'distinct task snapshots')
        require(Counter(row.get('template_id') for row in receipt['sandbox_receipts']) ==
                Counter(binding['snapshot_id'] for binding in current.values() for _ in range(2)),
                'per-task snapshot coverage')
        results = receipt['results']
        require(Counter((row['task_name'].split('/')[-1], row['agent_info']['name']) for row in results) == expected,
                'exact Oracle/nop trial set')
        for row in results:
            require(not row.get('exception_info') and
                    row['verifier_result']['rewards']['reward'] == (1 if row['agent_info']['name'] == 'oracle' else 0),
                    'original score or framework exception')
        require(len(receipt['sandbox_receipts']) == len(results), 'sandbox receipt count')
        for row in receipt['sandbox_receipts']:sandbox(row, candidates)
        require(receipt['vmm_observed'] and all(row['uid'] == runtime_uid for row in receipt['vmm_observed']), 'VMM UID')
        transport = receipt['transport_probe']
        require(transport.get('passed') is True and transport.get('cleanup') == 'passed', 'transport preflight')
        if mode == 'reuse':
            require(preparation['builders'] == [] and
                    set(preparation['cache_hits']) == {binding['task_name'] for binding in current.values()}, 'cache-only consumption')
            require(receipt.get('transport_probe_start_mode') == 'template', 'first restore mode')
            cache = receipt['cache_before_first_restore']
            require(isinstance(cache, dict) and not any(
                path.startswith('snapshot-local-cache/') or
                (path.startswith('image-cache/') and not path.startswith('image-cache/metadata/'))
                for path in cache), 'first restore local cache')
            sandbox(probe, candidates);cold_restores += 1
        trial_count += len(results)
    return {'passed': True, 'run_count': len(runs), 'trial_count': trial_count,
            'cold_first_restores': cold_restores, 'snapshot_ids': sorted({value['snapshot_id'] for value in bindings.values()}),
            'unique_restored_ids': len(identities), 'cache_scope': 'Lab snapshot/image content cache; OS page cache unchanged'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-dir', type=Path, action='append', required=True, help='Directory containing receipt.json and transport-probe/')
    parser.add_argument('--runtime-uid', type=int, default=os.getuid())
    args = parser.parse_args()
    try:
        cells = [(json.loads((path / 'receipt.json').read_text()),
                  json.loads((path / 'transport-probe/agentenv.json').read_text())) for path in args.run_dir]
        report = validate_shared_runs(cells, runtime_uid=args.runtime_uid)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, 'Shared qualification failed: ' + type(error).__name__ + '\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
