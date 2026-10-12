"""Explicit, source-bound external task material admission; never caches answers."""
import asyncio
import argparse
import copy
import json
from pathlib import Path
import re
import shlex
import sys
from urllib.parse import urlsplit

from dependencies import task_digest
from download_cache import lookup
from prepare_tasks import HARBOR_REVISION

TASK_REVISION = '69671fbaac6d67a7ef0dfec016cc38a64ef7a77c'
REQUIRED_SOURCES = {
    'extract-moves-from-video': 'youtube:ZCbvyPbhRfA',
    'mteb-leaderboard': 'github:embeddings-benchmark/results@c0fd3271442f8c0c839084a109fede85c8514dab',
}
RESULTS_COMMIT = 'c0fd3271442f8c0c839084a109fede85c8514dab'
SOURCE_URLS = {
    'extract-moves-from-video': {'https://www.youtube.com/watch?v=ZCbvyPbhRfA'},
    'mteb-leaderboard': {
        'https://github.com/embeddings-benchmark/results/archive/' + RESULTS_COMMIT + '.tar.gz',
        'https://codeload.github.com/embeddings-benchmark/results/tar.gz/' + RESULTS_COMMIT},
}


def load_manifest(path, tasks, *, harbor_revision=HARBOR_REVISION, task_revision=TASK_REVISION):
    """Validate explicit sources against selected task trees; absent required sources stay blocked."""
    record = copy.deepcopy(path) if isinstance(path, dict) else json.loads(Path(path).read_text()) if path else {
        'schema': 1, 'harbor_revision': harbor_revision, 'task_revision': task_revision, 'tasks': {}}
    if (record.get('schema') != 1 or record.get('harbor_revision') != harbor_revision or
            record.get('task_revision') != task_revision or not isinstance(record.get('tasks'), dict)):
        raise ValueError('Material manifest schema or revision changed')
    selected = {}
    for name, task in tasks.items():
        row = record['tasks'].get(name)
        if row is None:
            continue
        if row.get('task_sha256') != task_digest(task):
            raise ValueError('Material manifest task source changed: ' + name)
        inputs = row.get('inputs')
        if not isinstance(inputs, list) or not inputs:
            raise ValueError('Material manifest inputs must be explicit and nonempty')
        seen = set()
        for index, item in enumerate(inputs):
            identifier = item.get('id', '')
            source = urlsplit(item.get('url', ''))
            if (not re.fullmatch(r'[a-z0-9][a-z0-9_-]*', identifier) or identifier in seen or
                    not item.get('source_id') or item.get('kind') not in ('unchecked', 'http', 'cache') or
                    source.scheme not in ('http', 'https') or not source.hostname or
                    source.username or source.password or source.fragment):
                raise ValueError('Invalid explicit material input')
            seen.add(identifier)
            if name in REQUIRED_SOURCES and (item['source_id'] != REQUIRED_SOURCES[name] or
                    item['url'] not in SOURCE_URLS[name]):
                raise ValueError('Material source identity differs from the real task source')
            if name == 'extract-moves-from-video' and item['kind'] == 'http':
                raise ValueError('YouTube media requires a verified media cache; page HTTP is insufficient')
            if item['kind'] == 'unchecked':
                if not isinstance(item.get('reason'), str) or not item['reason']:
                    raise ValueError('Unchecked material must state its reason')
            else:
                if (not re.fullmatch(r'[0-9a-f]{64}', item.get('sha256', '')) or
                        type(item.get('bytes')) is not int or not 0 < item['bytes'] <= 256 * 1024**2 or
                        type(item.get('max_bytes')) is not int or
                        not item['bytes'] <= item['max_bytes'] <= 256 * 1024**2 or
                        type(item.get('timeout_sec')) not in (int, float) or
                        not 0 < item['timeout_sec'] <= 60):
                    raise ValueError('Material input requires bounded bytes, timeout and content digest')
                if item['kind'] == 'cache':
                    if not isinstance(item.get('cache_path'), str) or not item['cache_path']:
                        raise ValueError('Cached material requires an explicit cache path')
                    base = Path('.') if isinstance(path, dict) else Path(path).parent
                    item = {**item, 'cache_path': str((base / item['cache_path']).absolute())}
                if name == 'extract-moves-from-video' and item.get('format') not in ('mp4', 'webm'):
                    raise ValueError('YouTube cache must declare a media format')
            inputs[index] = {key: item[key] for key in ('id', 'source_id', 'kind', 'url', 'reason',
                'sha256', 'bytes', 'max_bytes', 'timeout_sec', 'cache_path', 'format') if key in item}
        selected[name] = {'task_sha256': row['task_sha256'], 'inputs': inputs}
    return {key: record[key] for key in ('schema', 'harbor_revision', 'task_revision')} | {
        'tasks': selected, 'selected_tasks': list(tasks)}


FETCH_SCRIPT = '''import hashlib,json,time,urllib.request,urllib.error
from pathlib import Path
row = ROW
started = time.monotonic()
try:
    digest = hashlib.sha256()
    size = 0
    if row['kind'] == 'cache':
        path = Path(row['cache_path'])
        if path.is_symlink() or not path.is_file(): raise ValueError('material cache missing or symlink')
        if path.stat().st_size > row['max_bytes']: raise ValueError('material cache exceeds byte bound')
        stream = path.open('rb')
    else:
        stream = urllib.request.urlopen(row['url'], timeout=row['timeout_sec'])
    with stream as response:
        if row['kind'] == 'http' and response.status != 200:
            raise ValueError('HTTP ' + str(response.status))
        prefix = b''
        while True:
            if time.monotonic() - started > row['timeout_sec']:
                raise TimeoutError('material fetch deadline exceeded')
            block = response.read(min(65536, row['max_bytes'] - size + 1))
            if not block: break
            size += len(block)
            if size > row['max_bytes']: raise ValueError('material exceeds byte bound')
            digest.update(block)
            if len(prefix) < 16: prefix = (prefix + block)[:16]
        if (row.get('format') == 'mp4' and prefix[4:8] != b'ftyp') or (
                row.get('format') == 'webm' and prefix[:4] != bytes.fromhex('1a45dfa3')):
            raise ValueError('cached material has no declared media signature')
    print(json.dumps({'status':'passed','source_id':row['source_id'],
                      'bytes':size,'sha256':digest.hexdigest()}))
except urllib.error.HTTPError as error:
    print(json.dumps({'status':'failed','reason':'HTTP ' + str(error.code)}))
except Exception as error:
    reason = str(error) if isinstance(error, (ValueError, TimeoutError)) else type(error).__name__
    print(json.dumps({'status':'failed','reason':reason}))
'''


async def fetch_input(row, exec_guest=None):
    script = FETCH_SCRIPT.replace('ROW', repr({name: row[name] for name in
        ('kind', 'url', 'source_id', 'timeout_sec', 'max_bytes', 'cache_path', 'format')
        if name in row}), 1)
    process = None
    try:
        async with asyncio.timeout(row['timeout_sec'] + 1):
            if exec_guest is not None:
                result = await exec_guest('python3 -c ' + shlex.quote(script), user='root',
                                          timeout_sec=row['timeout_sec'] + 1)
                code, stdout = result.return_code, result.stdout
            else:
                process = await asyncio.create_subprocess_exec(sys.executable, '-B', '-c', script,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
                stdout, _ = await process.communicate()
                code = process.returncode
        try:
            measured = json.loads(stdout)
        except (ValueError, TypeError):
            raise ValueError('material fetch returned no valid content receipt') from None
        if code != 0:
            raise ValueError('material fetch process failed: ' + str(code))
        if not isinstance(measured, dict):
            raise ValueError('material fetch returned no valid content receipt')
        if measured.get('status') != 'passed':
            raise ValueError(measured.get('reason', 'material fetch failed'))
        if any(measured.get(name) != row[name] for name in ('source_id', 'bytes', 'sha256')):
            raise ValueError('material content digest, size or source differs from binding')
        return {name: measured[name] for name in ('bytes', 'sha256')}
    finally:
        if process is not None and process.returncode is None:
            process.kill()
            await process.wait()


async def probe_material_inputs(manifest, task_name, *, exec_guest=None, cache=None):
    """Retrieve frozen bytes on host or via environment.exec; cache preparation is not admission."""
    if task_name not in manifest['selected_tasks']:
        raise ValueError('Material probe task was not selected')
    if task_name in REQUIRED_SOURCES and task_name not in manifest['tasks']:
        return {'task_name': task_name, 'passed': False, 'inputs': [
            {'source_id': REQUIRED_SOURCES[task_name], 'status': 'missing',
             'reason': 'required material has no explicit preflight binding'}]}
    inputs = []
    for row in manifest['tasks'].get(task_name, {}).get('inputs', []):
        receipt = {'id': row['id'], 'source_id': row['source_id'],
                   'scope': 'guest' if exec_guest else 'host'}
        if row['kind'] == 'unchecked':
            receipt.update(status='unchecked', reason=row['reason'])
        else:
            try:
                effective = row
                if row['kind'] == 'http' and cache and exec_guest is None:
                    cached = lookup(cache, row['url'], {})
                    if cached:
                        if (cached['task_name'] != task_name or
                                cached['task_sha256'] != manifest['tasks'][task_name]['task_sha256'] or
                                any(cached.get(name) != row[name] for name in ('sha256', 'bytes'))):
                            raise ValueError('download cache material source binding changed')
                        effective = {**row, 'kind': 'cache', 'cache_path': cached['path']}
                if effective['kind'] == 'cache':
                    receipt['scope'] = 'host-cache'
                measured = await fetch_input(effective,
                    exec_guest if effective['kind'] == 'http' else None)
                if effective['kind'] == 'cache':
                    receipt.update(status='cache_verified', availability='unchecked',
                        reason='Cache content verified; original task URL delivery has not been checked', **measured)
                else:
                    receipt.update(status='passed', availability='available', **measured)
            except Exception as error:
                reason = str(error) if isinstance(error, (ValueError, TimeoutError)) else type(error).__name__
                receipt.update(status='failed', reason=reason or type(error).__name__)
        inputs.append(receipt)
    return {'task_name': task_name, 'passed': all(row['status'] == 'passed' for row in inputs),
            'inputs': inputs}


async def select_material_tasks(manifest, *, model_required, cache=None):
    checks = {}
    for name in manifest['selected_tasks']:
        checks[name] = await probe_material_inputs(manifest, name, cache=cache) if model_required else {
            'task_name': name, 'passed': False, 'status': 'not_checked',
            'reason': 'Material admission is required for model evaluation only'}
    blocked = [name for name, row in checks.items() if model_required and not row['passed']]
    eligible = [name for name in manifest['selected_tasks'] if name not in blocked]
    return {'phase': 'host', 'model_required': model_required,
            'selected_task_names': manifest['selected_tasks'], 'selected_count': len(checks),
            'blocked_task_names': blocked, 'blocked_count': len(blocked),
            'eligible_task_names': eligible, 'eligible_count': len(eligible), 'checks': checks}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--job', type=Path, required=True)
    args = parser.parse_args(argv)
    job = json.loads(args.job.read_text())
    manifest = load_manifest(job.get('material_manifest'), job['tasks'])
    gate = asyncio.run(select_material_tasks(manifest, model_required=job['model_required'],
                                            cache=job.get('downloads')))
    print(json.dumps({'manifest': manifest, 'material_gate': gate}))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
