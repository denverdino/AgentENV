#!/usr/bin/env python3
"""Pack explicitly selected evidence; fetch summaries before optional full archives.

No VM payload discovery/deletion. Use existing retention.archive/apply first.
SSH uses the caller's existing authentication. Failed transfers retain .partial
files for inspection; use a new destination for a new run or an interrupted retry.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import subprocess
import tarfile


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def relative(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or '..' in path.parts or str(path) != name:
        raise ValueError('expected exact relative evidence path')
    return path


def validate_receipt(receipt):
    if receipt.get('schema') != 1 or not isinstance(receipt.get('files'), dict) or 'summary.json' not in receipt['files']:
        raise ValueError('invalid evidence receipt')
    for name, value in receipt['files'].items():
        relative(name)
        if not isinstance(value, str) or not re.fullmatch('[0-9a-f]{64}', value):
            raise ValueError('invalid member digest')
    if not re.fullmatch('[0-9a-f]{64}', receipt.get('archive_sha256', '')):
        raise ValueError('invalid archive digest')


def verify(path, receipt):
    validate_receipt(receipt)
    if digest(path) != receipt['archive_sha256']:
        raise ValueError('archive checksum mismatch')
    seen = set()
    with tarfile.open(path, 'r:gz') as archive:
        for member in archive:
            relative(member.name)
            if not member.isfile() or member.name in seen or member.name not in receipt['files']:
                raise ValueError('unexpected, duplicate or non-file archive member')
            with archive.extractfile(member) as stream:
                observed = hashlib.file_digest(stream, 'sha256').hexdigest()
            if observed != receipt['files'][member.name]:
                raise ValueError('member checksum mismatch: ' + member.name)
            seen.add(member.name)
    if seen != set(receipt['files']):
        raise ValueError('missing archive members')
    return len(seen)


def pack(root, files, summary, destination):
    root, destination = Path(root).resolve(), Path(destination)
    if not files or len(set(files)) != len(files):
        raise ValueError('nonempty unique evidence paths required')
    selected = []
    for name in files:
        path = root / relative(name)
        if path.resolve() != path or not path.is_file():
            raise ValueError('evidence must be a regular file without symlink ancestors: ' + name)
        selected.append((name, path))
    destination.mkdir(parents=True, exist_ok=False)
    summary_bytes = (json.dumps(summary, indent=2, allow_nan=False) + '\n').encode()
    (destination / 'summary.json').write_bytes(summary_bytes)
    hashes = {'summary.json': hashlib.sha256(summary_bytes).hexdigest()}
    partial = destination / 'evidence.tar.gz.partial'
    with tarfile.open(partial, 'w:gz', dereference=True) as archive:
        info = tarfile.TarInfo('summary.json')
        info.size, info.mode = len(summary_bytes), 0o644
        archive.addfile(info, io.BytesIO(summary_bytes))
        for name, path in selected:
            target = 'evidence/' + name
            hashes[target] = digest(path)
            archive.add(path, arcname=target, recursive=False)
    receipt = {'schema': 1, 'archive_sha256': digest(partial), 'files': hashes}
    verify(partial, receipt)
    partial.rename(destination / 'evidence.tar.gz')
    # The receipt is published last; an incomplete pack cannot be fetched as complete.
    (destination / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    return receipt


def ssh_copy(host, source, target):
    with Path(target).open('xb') as output:
        subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '--', host,
                        'cat -- ' + shlex.quote(source)], stdout=output,
                       stderr=subprocess.PIPE, check=True, timeout=600)


def fetch(host, remote, destination, *, full=False, copy=ssh_copy):
    if (not isinstance(host, str) or host.startswith('-')
            or not re.fullmatch(r'[A-Za-z0-9_.@:-]+', host)
            or not isinstance(remote, str) or not PurePosixPath(remote).is_absolute() or '\0' in remote):
        raise ValueError('explicit SSH host and absolute remote directory required')
    destination = Path(destination)
    if destination.is_symlink():
        raise ValueError('destination must not be a symlink')
    destination.mkdir(parents=True, exist_ok=True)

    def obtain(name, expected=None, validate=None):
        final = destination / name
        partial = destination / (name + '.partial')
        if os.path.lexists(final):
            if final.is_symlink() or not final.is_file() or (expected and digest(final) != expected):
                raise ValueError('existing local evidence differs; use a new destination')
            if validate:
                validate(final)
            return final
        if os.path.lexists(partial):
            raise FileExistsError('incomplete transfer retained; use a new destination: ' + str(partial))
        copy(host, str(PurePosixPath(remote) / name), partial)
        if expected and digest(partial) != expected:
            raise ValueError('download checksum mismatch: ' + name)
        if validate:
            validate(partial)
        partial.rename(final)
        return final

    receipt_path = obtain('receipt.json', validate=lambda p: validate_receipt(json.loads(p.read_text())))
    receipt = json.loads(receipt_path.read_text())
    obtain('summary.json', receipt['files']['summary.json'], validate=lambda p: json.loads(p.read_text()))
    count = 0
    if full:
        obtain('evidence.tar.gz', receipt['archive_sha256'], validate=lambda p: verify(p, receipt))
        count = len(receipt['files'])
    return {'archive_downloaded': full, 'files_verified': count,
            'archive_sha256': receipt['archive_sha256'], 'destination': str(destination)}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    pack_args = commands.add_parser('pack')
    pack_args.add_argument('--root', type=Path, required=True)
    pack_args.add_argument('--files', type=Path, required=True, help='JSON list of exact relative paths')
    pack_args.add_argument('--summary', type=Path, required=True, help='generated current result JSON')
    pack_args.add_argument('--output', type=Path, required=True)
    fetch_args = commands.add_parser('fetch')
    fetch_args.add_argument('--host', required=True)
    fetch_args.add_argument('--remote', required=True)
    fetch_args.add_argument('--output', type=Path, required=True)
    fetch_args.add_argument('--full', action='store_true')
    args = parser.parse_args()
    if args.command == 'pack':
        result = pack(args.root, json.loads(args.files.read_text()), json.loads(args.summary.read_text()), args.output)
    else:
        result = fetch(args.host, args.remote, args.output, full=args.full)
    print(json.dumps(result))
