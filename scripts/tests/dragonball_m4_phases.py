#!/usr/bin/env python3
"""Analyze explicit operation windows, never infer lifecycle from log order.

Timing versions describe frozen instrumentation, not snapshot formats. DB runner
v1 overlap reports spawn + remaining wait; do not sum overlapping preparation
parts. FC total includes release; DB total ends before readiness's release.
"""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import re
import statistics

VERSIONS = {'dragonball': {'db-serial-v1', 'db-overlap-v1'}, 'firecracker': {'fc-v1'}}
PREPARATION = ('network_us', 'disks_us', 'runner_us', 'memory_us', 'load_us', 'post_load_us', 'total_us')


def validate_version(backend, version):
    if version not in VERSIONS.get(backend, ()):
        raise ValueError('unknown backend/timing version')


def phase_parts(backend, operation, readiness, preparation):
    def integers(fields, required):
        if not isinstance(fields, dict) or any(k not in fields for k in required):
            raise ValueError('missing timing fields')
        if any(type(v) is not int or v < 0 for v in fields.values()):
            raise ValueError('timing fields must be nonnegative integers')

    required = ['envd_us', 'initialized_us', 'ready_us']
    if backend == 'dragonball':
        required += ['resumed_us', 'daemon_notified_us', 'identity_us', 'attempts']
    integers(readiness, required)
    if backend == 'firecracker' and any(k in readiness for k in ('resumed_us', 'identity_us', 'daemon_notified_us', 'attempts')):
        raise ValueError('Dragonball readiness fields in Firecracker timing')
    release = readiness.get('resumed_us', 0)
    init = readiness['initialized_us']
    identity = readiness.get('identity_us', init)
    sequence = [release, readiness['envd_us'], readiness.get('daemon_notified_us', readiness['envd_us']),
                init, identity, readiness['ready_us']]
    if sequence != sorted(sequence):
        raise ValueError('non-monotonic readiness boundaries')
    parts = {'health_us': readiness['envd_us'] - release,
             'init_us': init - readiness['envd_us'], 'identity_us': identity - init,
             'finish_us': readiness['ready_us'] - identity}
    if 'daemon_notified_us' in readiness:
        parts.update(daemon_notify_us=readiness['daemon_notified_us'] - readiness['envd_us'],
                     init_rpc_us=init - readiness['daemon_notified_us'])
    if operation == 'cold':
        if preparation is not None:
            raise ValueError('unexpected restore preparation for cold operation')
    else:
        integers(preparation, PREPARATION + (('release_us',) if backend == 'firecracker' else ()))
        if backend == 'dragonball' and 'release_us' in preparation:
            raise ValueError('Firecracker release field in Dragonball preparation')
        if any(preparation[k] > preparation['total_us'] for k in preparation if k != 'total_us'):
            raise ValueError('preparation component exceeds total')
        parts.update({k: preparation[k] for k in PREPARATION if k != 'total_us'})
        parts['preparation_through_release_us'] = preparation['total_us'] + release
        parts['measured_internal_us'] = preparation['total_us'] + readiness['ready_us']
    return parts


def validate_row(row):
    if row.get('schema') != 1 or row.get('operation') not in ('cold', 'restore', 'resume'):
        raise ValueError('unknown phase schema or operation')
    if any(not isinstance(row.get(k), str) or not row[k] for k in ('operation_id', 'sandbox_id')):
        raise ValueError('explicit operation and sandbox IDs required')
    validate_version(row.get('backend'), row.get('timing_version'))
    return phase_parts(row['backend'], row['operation'], row.get('readiness'), row.get('preparation'))


def parse_operation(text, *, operation_id, backend, operation, sandbox_id, timing_version):
    """text must contain only this request batch's log window; match by sandbox ID."""
    found = {'readiness': [], 'preparation': []}
    for line in re.sub(r'\x1b\[[0-9;]*m', '', text).splitlines():
        kind = ('preparation' if 'restore preparation phase timing' in line else
                'readiness' if 'readiness cumulative timing' in line else None)
        match = re.search(r'\bsandbox_id=("[^"\n]+"|\S+)', line)
        if kind is None or match is None or match[1].strip('"') != sandbox_id:
            continue
        declared = re.search(r'\bbackend=("[^"\n]+"|\S+)', line)
        if declared and declared[1].strip('"') != backend:
            raise ValueError('timing event backend differs from operation')
        pairs = re.findall(r'\b([a-z_]+_us|attempts)=([^\s,]+)', line)
        fields = {}
        for key, value in pairs:
            if key in fields:
                raise ValueError('duplicate timing field')
            try:
                fields[key] = int(value)
            except ValueError:
                raise ValueError('invalid timing field: ' + key) from None
        found[kind].append(fields)
    if len(found['readiness']) != 1 or len(found['preparation']) != (0 if operation == 'cold' else 1):
        raise ValueError('missing or duplicate operation timing events')
    row = dict(schema=1, operation_id=operation_id, backend=backend, operation=operation,
               sandbox_id=sandbox_id, timing_version=timing_version,
               readiness=found['readiness'][0], preparation=next(iter(found['preparation']), None))
    row['parts'] = validate_row(row)
    return row


def analyze(rows, expected_operations):
    if (not expected_operations or any(not isinstance(k, str) or not k for k in expected_operations)
            or len(set(expected_operations)) != len(expected_operations)):
        raise ValueError('nonempty unique expected operations required')
    if collections.Counter(r.get('operation_id') for r in rows) != collections.Counter(expected_operations):
        raise ValueError('missing, duplicate or unexpected operation')
    groups, versions = collections.defaultdict(list), collections.defaultdict(set)
    for row in rows:
        parts = validate_row(row)
        if row.get('parts') != parts:
            raise ValueError('derived phase values differ from raw boundaries')
        versions[row['backend']].add(row['timing_version'])
        groups[(row['backend'], row['operation'], row['timing_version'])].append(parts)
    if any(len(v) != 1 for v in versions.values()):
        raise ValueError('mixed timing versions for one backend; analyze separately')
    summary = []
    for (backend, operation, version), samples in sorted(groups.items()):
        keys = set(samples[0])
        if any(set(s) != keys for s in samples):
            raise ValueError('mixed phase fields')
        summary.append(dict(backend=backend, operation=operation, timing_version=version, n=len(samples),
                            parts={k: {'mean': statistics.mean(s[k] for s in samples),
                                       'p50': statistics.median(s[k] for s in samples)} for k in sorted(keys)}))
    return {'schema': 1, 'unit': 'microseconds', 'operations': len(rows), 'summary': summary,
            'qualification': False}


class Window:
    """One explicitly named request batch, including concurrent distinct sandboxes."""
    def __init__(self, path):
        self.path = Path(path)
        self.before = self.path.stat()

    def finish(self, requests, *, backend, operation, operation_id, timing_version):
        with self.path.open('rb') as stream:
            import os
            after = os.fstat(stream.fileno())
            if ((after.st_dev, after.st_ino) != (self.before.st_dev, self.before.st_ino)
                    or after.st_size < self.before.st_size):
                raise ValueError('timing log rotated or truncated during operation')
            stream.seek(self.before.st_size)
            data = stream.read(after.st_size - self.before.st_size)
        rows = []
        for index, request in enumerate(requests):
            if not request['ok']:
                continue
            guest = request['value']
            row = parse_operation(data.decode(), operation_id=f'{operation_id}/{index}',
                                  backend=backend, operation=operation, sandbox_id=guest['sandboxID'],
                                  timing_version=timing_version)
            row['log_window'] = {'path': str(self.path), 'start': self.before.st_size,
                                 'end': after.st_size, 'sha256': hashlib.sha256(data).hexdigest()}
            rows.append(row)
        return rows


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path, help='JSON with rows and expected_operations')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    document = json.loads(args.input.read_text())
    result = analyze(document['rows'], document['expected_operations'])
    args.output.write_text(json.dumps(result, indent=2) + '\n')
