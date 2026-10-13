#!/usr/bin/env python3
"""Run explicitly selected trusted local commands with input-bound stage receipts.

JSON plan (paths relative to cwd except source/cwd, which must be absolute):
  {"source": "/checkout", "cwd": "/checkout", "inputs": ["tool.py", "config"],
   "environment": ["PATH", "AENV_CONFIG_PATH"],
   "steps": [{"name": "check", "argv": ["python3", "tool.py"],
              "timeout_seconds": 60, "artifacts": ["result.json"]}]}

`plan --plan FILE --output DIR` validates and freezes the binding without running.
`run --plan FILE --output DIR [--resume]` runs sequentially, stopping on failure.
A prepared directory may be run once without --resume. Otherwise use a fresh
output directory or --resume. Changed settings, selected environment values, or
input contents/modes require a fresh output directory; no incompatible overwrite.
Failed/interrupted/corrupt/missing stage evidence reruns. Changed/missing output
artifacts rerun that stage and all later stages. Only completed, unchanged evidence
is reused. The controller does not infer inputs: explicitly list binaries, source,
config and dependency files, and every environment variable affecting commands.
`source` identifies the source root; it does not recursively fingerprint that tree.

Commands inherit the environment, use argv without a shell and run in a new
process group. Only selected environment values are bound (stored as a hash).
Arguments and environment values are never copied to JSON receipts; command
stdout/stderr are saved verbatim in private logs and may contain sensitive data.
These plans must come from a trusted operator; validation is not authorization.
No VM/cleanup commands are inferred. Timeouts terminate the owned process group,
but detached processes and VM/device/network cleanup remain UNVERIFIED: invoked
VM entrypoints own recovery. Audit interrupted resources before operator resume.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

SCHEMA = 1


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                     allow_nan=False).encode()).hexdigest()


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(path, value):
    """Atomic replacement; incomplete writes never look like a valid receipt."""
    fd, temporary = tempfile.mkstemp(prefix='.receipt-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, indent=2, allow_nan=False)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def file_hash(path):
    """Reject directories and symlink files; hash an opened regular file."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            before = os.fstat(stream.fileno())
            if not stat.S_ISREG(before.st_mode):
                raise ValueError('input/artifact must be a regular non-symlink file')
            h = hashlib.sha256()
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                h.update(block)
            after = os.fstat(stream.fileno())
            identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
            if identity(before) != identity(after) or identity(after) != identity(os.lstat(path)):
                raise ValueError('file changed while hashing')
            return {'sha256': h.hexdigest(), 'mode': stat.S_IMODE(after.st_mode)}
    except OSError as error:
        raise ValueError('input/artifact unavailable or not a regular non-symlink file') from error


def strings(value):
    return isinstance(value, list) and all(isinstance(v, str) and v and '\0' not in v for v in value)


def validate(plan):
    if not isinstance(plan, dict) or set(plan) != {'source', 'cwd', 'inputs', 'environment', 'steps'}:
        raise ValueError('plan requires source, cwd, inputs, environment and steps only')
    for name in ('source', 'cwd'):
        value = plan[name]
        if not isinstance(value, str) or not Path(value).is_absolute() or not Path(value).is_dir():
            raise ValueError(f'{name} must be an existing absolute directory')
    for name in ('inputs', 'environment'):
        if not strings(plan[name]) or len(set(plan[name])) != len(plan[name]):
            raise ValueError(f'{name} must be a list of unique strings')
    if any(not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', n) for n in plan['environment']):
        raise ValueError('invalid environment variable name')
    steps = plan['steps']
    if not isinstance(steps, list) or not steps:
        raise ValueError('steps must be a nonempty list')
    names = set()
    for step in steps:
        if not isinstance(step, dict) or set(step) != {'name', 'argv', 'timeout_seconds', 'artifacts'}:
            raise ValueError('step requires name, argv, timeout_seconds and artifacts only')
        name = step['name']
        if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}', name) or name in names:
            raise ValueError('step names must be unique safe identifiers')
        names.add(name)
        argv = step['argv']
        if (not isinstance(argv, list) or not argv or not argv[0]
                or any(not isinstance(arg, str) or '\0' in arg for arg in argv)):
            raise ValueError('argv must be a nonempty string list')
        timeout = step['timeout_seconds']
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError('timeout_seconds must be positive and finite')
        if not strings(step['artifacts']) or len(set(step['artifacts'])) != len(step['artifacts']):
            raise ValueError('artifacts must be a list of unique file paths')
    return plan


def files(plan, paths):
    return {str(Path(os.path.abspath(Path(plan['cwd']) / p))): file_hash(Path(plan['cwd']) / p)
            for p in paths}


def binding(plan, env):
    validate(plan)
    value = {'schema': SCHEMA, 'plan_sha256': digest(plan),
             'source': str(Path(plan['source']).resolve()),
             'cwd': str(Path(plan['cwd']).resolve()),
             'inputs': files(plan, plan['inputs']),
             'environment_sha256': digest({name: env.get(name) for name in plan['environment']})}
    return {**value, 'fingerprint': digest(value)}


def disk(path):
    usage = shutil.disk_usage(path)
    fs = os.statvfs(path)
    return {'total_bytes': usage.total, 'used_bytes': usage.used, 'free_bytes': usage.free,
            'available_inodes': fs.f_favail}


def load_json(path):
    try:
        value = json.loads(path.read_text())
        if not isinstance(value, dict):
            raise ValueError('receipt is not an object')
        return value
    except (OSError, ValueError) as error:
        raise ValueError('missing or corrupt campaign receipt; use a fresh output directory') from error


def campaign_receipt(plan, output, current, resume, preparing=False):
    path = output / 'campaign.json'
    if path.exists():
        receipt = load_json(path)
        if (receipt.get('schema') != SCHEMA or receipt.get('binding') != current
                or receipt.get('steps') != [s['name'] for s in plan['steps']]
                or receipt.get('status') not in {'planned', 'running', 'finished'}):
            raise ValueError('campaign binding changed or corrupt; use a fresh output directory')
        if preparing or (not resume and receipt['status'] != 'planned'):
            raise ValueError('output exists; use --resume or a fresh output directory')
        return receipt
    if resume:
        raise ValueError('no campaign receipt to resume')
    if any(p.name != '.lock' for p in output.iterdir()):
        raise ValueError('output is not empty; use a fresh output directory')
    receipt = {'schema': SCHEMA, 'binding': current,
               'steps': [s['name'] for s in plan['steps']], 'created_utc': utc(), 'status': 'planned'}
    save(path, receipt)
    return receipt


def stop_group(process):
    errors = []
    for sig, seconds in ((signal.SIGTERM, 0.5), (signal.SIGKILL, 1)):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        except OSError as error:
            errors.append(type(error).__name__)
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            process.poll()
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.02)
        else:
            continue
        break
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        errors.append('leader_reap_timeout')
    return errors


def reuse_reason(plan, step, previous, current):
    if not previous:
        return 'no_receipt'
    try:
        old = load_json(previous[-1])
        if old.get('schema') != SCHEMA or old.get('name') != step['name'] or old.get('fingerprint') != current['fingerprint']:
            return 'corrupt_receipt'
        if old.get('status') not in {'attempted', 'completed', 'failed', 'timed_out'}:
            return 'corrupt_receipt'
        if old['status'] != 'completed':
            return 'previous_' + old['status']
        elapsed = old.get('elapsed_seconds')
        if (old.get('exit_code') != 0 or old.get('validation_errors') != []
                or old.get('category') is not None
                or not isinstance(old.get('started_utc'), str)
                or not isinstance(old.get('ended_utc'), str)
                or not isinstance(elapsed, (float, int)) or isinstance(elapsed, bool)
                or not math.isfinite(elapsed) or elapsed < 0
                or not isinstance(old.get('disk_before'), dict)
                or not isinstance(old.get('disk_after'), dict)):
            return 'corrupt_receipt'
        if files(plan, step['artifacts']) != old.get('artifacts'):
            return 'artifact_changed'
        if file_hash(previous[-1].with_suffix('.log')) != old.get('log'):
            return 'log_changed'
        return 'reused'
    except ValueError:
        try:
            load_json(previous[-1])
        except ValueError:
            return 'corrupt_receipt'
        return 'artifact_changed'


def attempt_step(plan, step, directory, current, env, reason):
    directory.mkdir(mode=0o700, exist_ok=True)
    # Preserve orphaned logs and malformed receipts, including interrupted writes.
    numbers = [int(p.stem) for p in directory.iterdir() if p.stem.isdigit()]
    path = directory / f'{max(numbers, default=0) + 1:06d}.json'
    started = time.monotonic()
    attempt = {'schema': SCHEMA, 'name': step['name'], 'fingerprint': current['fingerprint'],
               'reason': reason, 'status': 'attempted', 'started_utc': utc(),
               'started_monotonic': started, 'disk_before': disk(plan['cwd']),
               'resource_cleanup': 'unverified', 'validation_errors': []}
    save(path, attempt)
    process = None
    try:
        fd = os.open(path.with_suffix('.log'), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            process = subprocess.Popen(step['argv'], cwd=plan['cwd'], env=env,
                                       stdin=subprocess.DEVNULL, stdout=stream,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            attempt['pid'] = process.pid
            save(path, attempt)
            code = process.wait(timeout=step['timeout_seconds'])
        attempt.update(exit_code=code, status='completed' if code == 0 else 'failed',
                       category=None if code == 0 else 'exit_nonzero')
    except subprocess.TimeoutExpired:
        attempt.update(status='timed_out', category='timeout')
        attempt['termination_errors'] = stop_group(process)
    except (KeyboardInterrupt, InterruptedError):
        attempt.update(status='failed', category='interrupted')
        if process is not None:
            attempt['termination_errors'] = stop_group(process)
    except Exception as error:
        attempt.update(status='failed', category='execution_error', error_type=type(error).__name__)
        if process is not None:
            attempt['termination_errors'] = stop_group(process)
    finally:
        try:
            if binding(plan, env) != current:
                attempt['validation_errors'].append('input_changed')
        except (OSError, ValueError):
            attempt['validation_errors'].append('input_changed')
        try:
            attempt['artifacts'] = files(plan, step['artifacts'])
        except ValueError:
            attempt['validation_errors'].append('artifact_missing_or_invalid')
        try:
            attempt['log'] = file_hash(path.with_suffix('.log'))
        except ValueError:
            attempt['validation_errors'].append('log_missing_or_invalid')
        if attempt['status'] == 'completed' and attempt['validation_errors']:
            attempt.update(status='failed', category=attempt['validation_errors'][0])
        attempt.update(ended_utc=utc(), elapsed_seconds=time.monotonic() - started)
        try:
            attempt['disk_after'] = disk(plan['cwd'])
        except OSError as error:
            attempt['disk_after_error'] = type(error).__name__
        save(path, attempt)
    return attempt


def operate(plan, output, *, resume=False, env=None, preparing=False):
    env = dict(os.environ if env is None else env)
    current = binding(plan, env)
    output = Path(output)
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (output / '.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError('another campaign owns this output') from None
        receipt = campaign_receipt(plan, output, current, resume, preparing)
        if preparing:
            return 0
        receipt['status'] = 'running'
        save(output / 'campaign.json', receipt)
        summary = {'schema': SCHEMA, 'fingerprint': current['fingerprint'], 'started_utc': utc(),
                   'resource_cleanup': 'unverified', 'steps': [],
                   'unattempted': [step['name'] for step in plan['steps']]}
        start = time.monotonic()
        reran = False
        result = 0
        for step in plan['steps']:
            if binding(plan, env) != current:
                raise ValueError('campaign input binding changed before step')
            directory = output / step['name']
            previous = sorted(directory.glob('[0-9]*.json'))
            reason = reuse_reason(plan, step, previous, current)
            if reran and reason == 'reused':
                reason = 'upstream_rerun'
            if reason == 'reused':
                summary['steps'].append({'name': step['name'], 'decision': 'reused',
                                         'status': 'completed', 'elapsed_seconds': 0})
            else:
                reran = True
                attempt = attempt_step(plan, step, directory, current, env, reason)
                summary['steps'].append({k: attempt[k] for k in ('name', 'status', 'elapsed_seconds', 'category', 'reason')})
                summary['steps'][-1]['decision'] = 'executed'
                if attempt['status'] != 'completed':
                    result = 1
            summary.update(ended_utc=utc(), elapsed_seconds=time.monotonic() - start,
                           exit_code=result, unattempted=[s['name'] for s in plan['steps'][len(summary['steps']):]])
            save(output / 'summary.json', summary)
            if result:
                break
        receipt.update(status='finished', last_ended_utc=utc(), last_exit_code=result)
        save(output / 'campaign.json', receipt)
        return result


def prepare(plan, output, *, env=None):
    return operate(plan, output, env=env, preparing=True)


def run(plan, output, *, resume=False, env=None):
    return operate(plan, output, resume=resume, env=env)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('action', choices=['plan', 'run'])
    parser.add_argument('--plan', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--resume', action='store_true')
    args = parser.parse_args()
    if args.action == 'plan' and args.resume:
        parser.error('--resume is only supported by run')
    def interrupted(signum, frame):
        raise InterruptedError('campaign interrupted')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        plan = load_json(args.plan)
        return operate(plan, args.output, resume=args.resume, preparing=args.action == 'plan')
    except (ValueError, OSError):
        # Do not print exception strings: they may contain supplied argv or env.
        print('Campaign rejected: invalid plan, unavailable input, or incompatible output binding. Use a fresh output directory for changed inputs.', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
