#!/usr/bin/env python3
"""Archive and reclaim an explicitly named successful M4 performance lab.

Root orchestration and the shared host lock are required. Only same-boot trusted
M4 manifests and completed, input-verified performance result.json are supported.
No scanning for candidates, recovery, device deletion, or generic cache GC occurs.
Node run directories, including UNIX sockets, are always retained; this tool does
not attempt to prove socket liveness across every network namespace.
Archive hashes must be retained by the caller, outside the lab. Read-only evidence
and its receipt are root-owned; this protects against workloads, not hostile root. SIGKILL cannot leave a receipt; ambiguous partial deletions
require manual review and are never automatically resumed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import stat
import time

from dragonball_m4_host import acquire_host_lock
import dragonball_m4_recovery as recovery

PAYLOADS = tuple(f'{node}/{name}' for node in ('a', 'b')
                 for name in ('home', 'deps', 'tmp')) + ('repo', 'private-tools.ext4')
NODE_FILES = {'config.toml', 'regctl.json', 'ready'}
RUNTIME_PATHS = ('a/run', 'b/run')


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def relative_path(value):
    path = PurePosixPath(value)
    if path.is_absolute() or not path.parts or '..' in path.parts or str(path) != value:
        raise ValueError('expected an exact relative evidence path: ' + value)
    return path


def inventory(path, device, *, retained_runtime=False):
    """Sockets are inventoried only inside runtime trees that are never deleted."""
    info = path.lstat()
    allowed = stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)
    if retained_runtime and stat.S_ISSOCK(info.st_mode):
        allowed = True
    if info.st_dev != device or not allowed:
        raise ValueError('symlink, mount or special file: ' + str(path))
    record = {'identity': [info.st_dev, info.st_ino], 'mode': info.st_mode,
              'size': info.st_size, 'mtime_ns': info.st_mtime_ns,
              'allocated_bytes': info.st_blocks * 512}
    if stat.S_ISDIR(info.st_mode):
        record['children'] = {child.name: inventory(child, device, retained_runtime=retained_runtime)
                              for child in sorted(path.iterdir())}
    return record


def allocated(record):
    return record['allocated_bytes'] + sum(allocated(child) for child in record.get('children', {}).values())


def successful(root):
    for name in ('PIN', '.pin', 'pinned'):
        if os.path.lexists(root / name):
            raise ValueError('lab is pinned: ' + name)
    recovery.trusted(root / 'result.json')
    result = json.loads((root / 'result.json').read_text())
    groups = result.get('measurements', {}).get('groups', {})
    if (result.get('root') != str(root) or result.get('cleanup') != 'passed'
            or result.get('inputs_verified') is not True or 'error' in result
            or not groups or any(row.get('count', 0) <= 0 or row.get('successes') != row.get('count')
                                 for row in groups.values())):
        raise ValueError('failed, unfinished or unsupported M4 performance result')


def layout(root):
    """Known Lab scaffolding; every retained regular file is evidence."""
    device = root.stat().st_dev
    payloads, evidence, scaffolding = {}, [], {}
    for item in sorted(root.iterdir()):
        info = item.lstat()
        if item.name in ('a', 'b'):
            if not stat.S_ISDIR(info.st_mode) or info.st_dev != device:
                raise ValueError('unknown node layout: ' + str(item))
            scaffolding[item.name] = recovery.file_identity(item)
            for child in sorted(item.iterdir()):
                relative = str(child.relative_to(root))
                if relative in RUNTIME_PATHS:
                    if not stat.S_ISDIR(child.lstat().st_mode):
                        raise ValueError('unknown runtime layout: ' + str(child))
                    scaffolding[relative] = inventory(child, device, retained_runtime=True)
                    evidence.extend(str(path.relative_to(root)) for path in sorted(child.rglob('*'))
                                    if path.is_file())
                elif relative in PAYLOADS:
                    payloads[relative] = inventory(child, device)
                elif child.name in NODE_FILES and stat.S_ISREG(child.lstat().st_mode):
                    evidence.append(relative)
                else:
                    raise ValueError('unknown node layout: ' + str(child))
        elif item.name in PAYLOADS:
            payloads[item.name] = inventory(item, device)
        elif item.name == 'evidence':
            recovery.trusted(item, directory=True)
        elif stat.S_ISREG(info.st_mode):
            if item.name != 'archive-receipt.json' and not item.name.startswith('retention-'):
                evidence.append(item.name)
        else:
            raise ValueError('unknown lab layout: ' + str(item))
    return payloads, evidence, scaffolding


def check_clean(root):
    manifest, _ = recovery.load_manifest(root)
    if manifest.get('pinned') or manifest.get('state') == 'pinned':
        raise ValueError('lab manifest is pinned')
    successful(root)
    report = recovery.audit(root)
    if report['status'] != 'clean' or report['errors']:
        raise ValueError('live or ambiguous lab resources: ' + json.dumps(report))
    return manifest


def archive(root, *, evidence=()):
    """Copy exact extra evidence plus all retained metadata; return receipt hash.

Node home/logs and repo/registry.json are included automatically. Caller must
    list any additional diagnostic files within disposable payloads.
No payload is deleted. Existing archives are never overwritten.
"""
    root = Path(root)
    lock = acquire_host_lock()
    try:
        manifest = check_clean(root)
        payloads, retained, scaffolding = layout(root)
        diagnostic = ['repo/registry.json'] if (root / 'repo/registry.json').is_file() else []
        for node in ('a', 'b'):
            logs = root / node / 'home/logs'
            if logs.is_dir():
                diagnostic.extend(str(path.relative_to(root)) for path in logs.rglob('*') if path.is_file())
        selected = sorted(set(retained) | set(evidence) | set(diagnostic))
        for name in selected:
            path = root / relative_path(name)
            # Check each ancestor, even if the selected file itself is regular.
            for parent in list(path.relative_to(root).parents)[:-1]:
                if (root / parent).is_symlink():
                    raise ValueError('symlink evidence ancestor: ' + str(parent))
            if not stat.S_ISREG(path.lstat().st_mode):
                raise ValueError('evidence must be a regular file: ' + name)
        destination = root / 'evidence'
        if destination.exists() or os.path.lexists(root / 'archive-receipt.json'):
            raise ValueError('archive already exists; never overwrite evidence')
        destination.mkdir(mode=0o700)
        hashes = {}
        for name in selected:
            target = destination / name
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            with (root / name).open('rb') as source, target.open('xb') as output:
                shutil.copyfileobj(source, output)
                output.flush()
                os.fsync(output.fileno())
            target.chmod(0o400)
            hashes[name] = digest(target)
            if hashes[name] != digest(root / name):
                raise ValueError('evidence changed during archival: ' + name)
        for directory in sorted((p for p in destination.rglob('*') if p.is_dir()),
                                key=lambda p: len(p.parts), reverse=True) + [destination]:
            directory.chmod(0o500)
            fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        # Revalidate the source and all payload identities after copying evidence.
        check_clean(root)
        if layout(root) != (payloads, retained, scaffolding):
            raise ValueError('lab changed during archival')
        receipt = {'schema': 1, 'root': str(root), 'root_identity': manifest['root_identity'],
                   'payloads': payloads, 'evidence': hashes, 'retained': retained,
                   'scaffolding': scaffolding}
        path = root / 'archive-receipt.json'
        recovery.atomic_json(path, receipt)
        path.chmod(0o400)
        return digest(path)
    finally:
        os.close(lock)


def verified_archive(root, expected):
    recovery.trusted(root / 'archive-receipt.json')
    path = root / 'archive-receipt.json'
    if path.stat().st_mode & 0o222 or digest(path) != expected:
        raise ValueError('archive receipt is writable or hash mismatch')
    record = json.loads(path.read_text())
    if (record.get('schema') != 1 or record.get('root') != str(root)
            or record.get('root_identity') != recovery.file_identity(root)
            or not {'lab-manifest.json', 'result.json'} <= record.get('evidence', {}).keys()
            or not set(record.get('payloads', {})) <= set(PAYLOADS)):
        raise ValueError('archive identity or layout mismatch')
    recovery.trusted(root / 'evidence', directory=True)
    for name, checksum in record['evidence'].items():
        path = root / 'evidence' / relative_path(name)
        for parent in path.parents:
            if parent == root:
                break
            recovery.trusted(parent, directory=True)
            if parent.stat().st_mode & 0o222:
                raise ValueError('writable archived evidence directory')
        recovery.trusted(path)
        if path.stat().st_mode & 0o222 or digest(path) != checksum:
            raise ValueError('archived evidence is writable or hash mismatch: ' + name)
        original = root / name
        if name in record['retained'] and digest(original) != checksum:
            raise ValueError('retained evidence changed: ' + name)
    return record


def prior_deleted(root, expected):
    deleted = set()
    for path in root.glob('retention-*.json'):
        recovery.trusted(path)
        record = json.loads(path.read_text())
        if (record.get('archive_sha256') == expected and record.get('root') == str(root)
                and record.get('root_identity') == recovery.file_identity(root)):
            deleted.update(record.get('deleted_paths', []))
    return deleted


def retain(root, *, archive_sha256, apply=False):
    """Audit by default; explicit apply reclaims only verified archived payloads.

Every accepted root gets a durable receipt, including skips and errors. An
untrusted/replaced root cannot safely receive a receipt; its report is returned.
"""
    root = Path(root)
    lock = acquire_host_lock()
    report = {'schema': 1, 'root': str(root), 'archive_sha256': archive_sha256,
              'apply': apply, 'status': 'refused', 'bytes_before': None, 'bytes_after': None,
              'free_bytes_before': None, 'free_bytes_after': None,
              'deleted_paths': [], 'skipped_paths': [], 'retained_paths': [],
              'retained_bytes': None, 'errors': []}
    identity = None
    try:
        try:
            manifest, _ = recovery.load_manifest(root)
            identity = manifest['root_identity']
            report['root_identity'] = identity
            report['free_bytes_before'] = shutil.disk_usage(root)[2]
            check_clean(root)
            current, retained, scaffolding = layout(root)
            report['retained_paths'] = [name for name in RUNTIME_PATHS if name in scaffolding]
            report['retained_bytes'] = sum(allocated(scaffolding[name]) for name in report['retained_paths'])
            report['bytes_before'] = sum(allocated(item) for item in current.values())
            report['bytes_after'] = report['bytes_before']
            record = verified_archive(root, archive_sha256)
            if retained != record['retained'] or scaffolding != record['scaffolding']:
                raise ValueError('retained lab layout or identity changed')
            deleted = prior_deleted(root, archive_sha256)
            if any(name not in record['payloads'] or record['payloads'][name] != item
                   for name, item in current.items()):
                raise ValueError('payload identity or contents changed')
            missing = set(record['payloads']) - current.keys()
            if not missing <= deleted:
                raise ValueError('payload missing without a verified deletion receipt')
            report['status'] = 'eligible' if current else 'already_reclaimed'
            report['skipped_paths'] = sorted(missing)
            if apply and current:
                if not shutil.rmtree.avoids_symlink_attacks:
                    raise ValueError('platform lacks safe directory deletion')
                for name, item in current.items():
                    # Each mutation requires fresh ownership, references and archive.
                    check_clean(root)
                    verified_archive(root, archive_sha256)
                    if inventory(root / name, root.stat().st_dev) != item:
                        raise ValueError('payload identity changed before deletion: ' + name)
                    if 'children' in item:
                        shutil.rmtree(root / name)
                    else:
                        (root / name).unlink()
                    report['deleted_paths'].append(name)
                    report['bytes_after'] -= allocated(item)
                report['status'] = 'reclaimed'
        except BaseException as error:
            report['status'] = 'refused'
            report['errors'].append(type(error).__name__ + ': ' + str(error))
            # rmtree can remove children before raising, including on signals.
            # Never retain a pre-operation estimate as the post-operation total.
            report['bytes_after'] = None
            if not isinstance(error, Exception):
                raise
        finally:
            if identity is not None:
                # Do not write into a substituted root even when reporting failure.
                try:
                    manifest, _ = recovery.load_manifest(root)
                    if manifest['root_identity'] != identity:
                        raise ValueError('root identity changed before receipt')
                    if report['bytes_before'] is not None and report['bytes_after'] is None:
                        try:
                            remaining, _, _ = layout(root)
                            report['bytes_after'] = sum(allocated(item) for item in remaining.values())
                        except Exception as error:
                            report['errors'].append('remaining bytes unknown: ' + str(error))
                    report['free_bytes_after'] = shutil.disk_usage(root)[2]
                    recovery.atomic_json(root / f'retention-{time.time_ns()}.json', report)
                except Exception as error:
                    report['status'] = 'refused'
                    report['errors'].append('receipt not persisted: ' + str(error))
        return report
    finally:
        os.close(lock)


def admit(path, *, reserve_bytes, planned_bytes, retained_bytes):
    """Admission only: available space must cover all three explicit budgets."""
    if min(reserve_bytes, planned_bytes, retained_bytes) < 0:
        raise ValueError('disk budgets must be nonnegative')
    free = shutil.disk_usage(path)[2]
    required = reserve_bytes + planned_bytes + retained_bytes
    if free < required:
        raise ValueError(f'insufficient disk space: free={free}, required={required}')
    return {'admitted': True, 'free_bytes': free, 'required_bytes': required,
            'reserve_bytes': reserve_bytes, 'planned_bytes': planned_bytes, 'retained_bytes': retained_bytes}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('archive', 'audit', 'apply'):
        command = commands.add_parser(name)
        command.add_argument('--root', type=Path, required=True)
        if name == 'archive':
            command.add_argument('--evidence', action='append', default=[], help='exact extra relative evidence file')
        else:
            command.add_argument('--archive-sha256', required=True, help='externally retained archive receipt SHA256')
    command = commands.add_parser('admit')
    command.add_argument('--path', type=Path, required=True)
    for name in ('reserve', 'planned', 'retained'):
        command.add_argument('--' + name + '-bytes', type=int, required=True)
    args = parser.parse_args()
    if args.command != 'admit' and os.geteuid() != 0:
        parser.error('root orchestration required for complete host audit')
    if args.command == 'archive':
        report = {'archive_sha256': archive(args.root, evidence=args.evidence)}
    elif args.command == 'admit':
        report = admit(args.path, reserve_bytes=args.reserve_bytes,
                       planned_bytes=args.planned_bytes, retained_bytes=args.retained_bytes)
    else:
        def terminate(signum, _frame):
            raise SystemExit(128 + signum)
        previous = signal.signal(signal.SIGTERM, terminate)
        try:
            report = retain(args.root, archive_sha256=args.archive_sha256, apply=args.command == 'apply')
        finally:
            signal.signal(signal.SIGTERM, previous)
    print(json.dumps(report, indent=2))
    return int(report.get('status') == 'refused')


if __name__ == '__main__':
    raise SystemExit(main())
