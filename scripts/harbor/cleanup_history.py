"""Delete explicitly authorized, expired Lab roots after ownership/reference checks."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat


def _identity(path):
    s = path.lstat()
    if (path != path.resolve() or not stat.S_ISDIR(s.st_mode)
            or s.st_uid != os.geteuid() or s.st_mode & 0o022):
        raise ValueError('Not an exact owned directory')
    return {'identity': [s.st_dev, s.st_ino], 'uid': s.st_uid, 'mtime_ns': s.st_mtime_ns}


def _inspect(path, cutoff, protected, inspect_references):
    if not path.is_absolute() or not path.name.startswith('db-m4.'):
        raise ValueError('Unknown history root')
    identity = _identity(path)
    if path.stat().st_mtime >= cutoff:
        raise ValueError('Current evidence')
    if any(path == p or path in p.parents or p in path.parents for p in protected):
        raise ValueError('Protected input or evidence')
    if any(os.path.lexists(path / marker) for marker in ('PIN', '.pin', 'pinned', 'KEEP')):
        raise ValueError('Pinned evidence')
    manifest = path / 'lab-manifest.json'
    if manifest.exists():
        if manifest.is_symlink(): raise ValueError('Untrusted runtime manifest')
        state = json.loads(manifest.read_text())
        if state.get('pinned') or state.get('state') == 'pinned': raise ValueError('Pinned runtime manifest')
    records = list(path.glob('resources-*.json'))
    if len(records) != 1 or records[0].is_symlink():
        raise ValueError('Missing or ambiguous ownership record')
    record = json.loads(records[0].read_text())
    owner = record.get('evidence_root', {})
    if (record.get('family') != 'm4-lab' or record.get('schema') != 1
            or owner.get('path') != str(path) or owner.get('identity') != identity['identity']
            or owner.get('uid') != identity['uid']
            or record.get('state') not in ('completed', 'failed', 'interrupted')):
        raise ValueError('Unknown or active ownership')
    if inspect_references(path):
        raise ValueError('Live reference')
    return {'path': str(path), **identity, 'record': records[0].name,
            'record_sha256': hashlib.sha256(records[0].read_bytes()).hexdigest()}


def plan_cleanup(roots, *, cutoff, protected, inspect_references):
    protected = {Path(p).resolve() for p in protected}
    plan = {'schema': 1, 'cutoff': cutoff, 'protected': sorted(map(str, protected)),
            'targets': [], 'skipped': []}
    for path in dict.fromkeys(map(Path, roots)):
        try:
            plan['targets'].append(_inspect(path, cutoff, protected, inspect_references))
        except (OSError, ValueError) as error:
            plan['skipped'].append({'path': str(path), 'reason': str(error)})
    return plan


def apply_cleanup(plan, *, inspect_references, record=lambda receipt: None):
    protected = {Path(p) for p in plan['protected']}
    def verify(row):
        actual = _inspect(Path(row['path']), plan['cutoff'], protected, inspect_references)
        if actual != row:
            raise ValueError('Historical directory changed after planning')
    # Refuse the whole plan before any mutation if any identity/reference changed.
    for row in plan['targets']:
        verify(row)
    parents = {str(Path(row['path']).parent) for row in plan['targets']}
    before = {p: shutil.disk_usage(p).free for p in parents}
    receipt = {'removed': [], 'free_before': before, 'skipped': plan['skipped'], 'in_progress': None}
    record(dict(receipt))
    try:
        for row in plan['targets']:
            verify(row)
            receipt['in_progress'] = row['path']; record(dict(receipt))
            shutil.rmtree(row['path'])
            receipt['removed'].append(row['path']); receipt['in_progress'] = None
            record(dict(receipt))
    except BaseException as error:
        receipt['error_type'] = type(error).__name__
        raise
    finally:
        receipt['free_after'] = {p: shutil.disk_usage(p).free for p in parents}
        record(dict(receipt))
    return receipt


def inspect_host_references(root):
    """Reuse the M4 audit, including mountinfo, namespaces and live device checks."""
    import sys
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
    from dragonball_m4_recovery import audit
    try:
        report = audit(root)
    except (OSError, ValueError) as error:
        return ['unverified M4 resources: ' + str(error)]
    return [] if report['status'] == 'clean' and not report['errors'] else [report]
