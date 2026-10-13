#!/usr/bin/env python3
"""Await a trusted transfer, publish a verified archive, then check its frozen source."""
import argparse
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile

from check_tools import require_python
from freeze_driver import verify_candidate

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts/tests'))
import dragonball_m4_campaign as campaign


def check_archive(archive, candidate):
    candidate = Path(candidate)
    with tarfile.open(archive) as bundle:
        members = bundle.getmembers()
        for member in members:
            path = PurePosixPath(member.name)
            if (path.is_absolute() or '..' in path.parts or not path.parts
                    or path.parts[0] not in ('source', 'driver-sha256.json')
                    or (path.parts[0] == 'driver-sha256.json' and path.parts != ('driver-sha256.json',))
                    or not (member.isfile() or member.isdir())):
                raise ValueError('Candidate archive contains an unsafe member')
        candidate.mkdir()
        bundle.extractall(candidate, members=members, filter='data')
    verify_candidate(candidate)
    result = subprocess.run([sys.executable, '-B', str(candidate / 'source/scripts/harbor/check_tools.py')],
                            cwd=candidate / 'source', check=False,
                            env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
    verify_candidate(candidate)
    return result.returncode


def run_delivery(partial, archive, candidate, expected_sha256, expected_bytes,
                 transfer, receipts, timeout=600):
    require_python()
    if os.geteuid() == 0:
        raise ValueError('Delivery checks must run as the non-root test account')
    partial, archive, candidate = (Path(path).absolute() for path in (partial, archive, candidate))
    if len({partial, archive, candidate}) != 3:
        raise ValueError('Delivery paths must be distinct')
    if any(path.exists() or path.is_symlink() for path in (partial, archive, candidate)):
        raise FileExistsError('Use fresh delivery paths; existing evidence is retained')
    entry = Path(__file__).resolve()
    publisher = entry.with_name('artifact_delivery.py')
    plan = {'source': str(ROOT), 'cwd': str(Path.cwd()),
            'inputs': [str(path) for path in (entry, publisher, entry.with_name('freeze_driver.py'),
                       entry.with_name('check_tools.py'), ROOT / 'scripts/tests/dragonball_m4_campaign.py')],
            'environment': ['PATH', 'PYTHONDONTWRITEBYTECODE'], 'steps': [
        {'name': 'transfer', 'argv': transfer, 'timeout_seconds': timeout, 'artifacts': [str(partial)]},
        {'name': 'publish', 'argv': [sys.executable, '-B', str(publisher), '--partial', str(partial),
            '--output', str(archive), '--sha256', expected_sha256, '--bytes', str(expected_bytes)],
         'timeout_seconds': timeout, 'artifacts': [str(archive)]},
        {'name': 'check', 'argv': [sys.executable, '-B', str(entry), '--check-archive', str(archive),
            '--candidate', str(candidate)], 'timeout_seconds': timeout,
         'artifacts': [str(candidate / 'driver-sha256.json')]},
    ]}
    return campaign.run(plan, Path(receipts), env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--partial', type=Path)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--sha256')
    parser.add_argument('--bytes', type=int)
    parser.add_argument('--receipts', type=Path)
    parser.add_argument('--timeout', type=float, default=600)
    parser.add_argument('--check-archive', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--transfer', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    try:
        require_python()
        if args.check_archive:
            return check_archive(args.check_archive, args.candidate)
        if not all((args.partial, args.archive, args.sha256, args.bytes, args.receipts, args.transfer)):
            parser.error('partial, archive, sha256, bytes, receipts and transfer are required')
        return run_delivery(args.partial, args.archive, args.candidate, args.sha256, args.bytes,
                            args.transfer, args.receipts, timeout=args.timeout)
    except (OSError, ValueError, tarfile.TarError) as error:
        parser.exit(1, 'Delivery refused: ' + type(error).__name__ + '\n')


if __name__ == '__main__':
    raise SystemExit(main())
