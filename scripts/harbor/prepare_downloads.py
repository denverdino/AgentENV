#!/usr/bin/env python3
"""Freeze CIFAR and uv public artifacts without changing task scripts."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
import urllib.request

from dependencies import file_digest, task_digest
from prepare_acr import verify_delivery

CIFAR_URL = 'http://www.cs.toronto.edu/~kriz/cifar-10-binary.tar.gz'
CIFAR_MD5 = 'c32a1d4ab5d03f1284b67883e8d87530'
UV_BASE = 'https://github.com/astral-sh/uv/releases/download/0.9.5'
UV_NAME = 'uv-x86_64-unknown-linux-gnu.tar.gz'


def fetch(url, cache, *, expected=None, algorithm='sha256', maximum=256 * 1024**2):
    cache = Path(cache)
    cache.mkdir(parents=True, exist_ok=True)
    index = cache / (hashlib.sha256(url.encode()).hexdigest() + '.json')
    if index.exists():
        record = json.loads(index.read_text())
        payload = cache / record['sha256']
        if (record.get('source') == url and payload.is_file() and not payload.is_symlink() and
                0 < record['bytes'] <= maximum and
                payload.stat().st_size == record['bytes'] and file_digest(payload) == record['sha256']):
            digest = hashlib.new(algorithm)
            with payload.open('rb') as source:
                for chunk in iter(lambda: source.read(1024**2), b''):
                    digest.update(chunk)
            if expected is None or digest.hexdigest() == expected:
                return {**record, 'path': str(payload.resolve()), 'cache_hit': True}
        raise ValueError('Existing download cache is corrupt or differs from its checksum')
    fd, name = tempfile.mkstemp(prefix='download-', dir=cache)
    pending = Path(name)
    started = time.monotonic()
    try:
        digest = hashlib.new(algorithm)
        size = 0
        with os.fdopen(fd, 'wb') as target, urllib.request.urlopen(url, timeout=30) as response:
            while chunk := response.read(1024**2):
                size += len(chunk)
                if size > maximum or time.monotonic() - started > 1200:
                    raise ValueError('Public download exceeded its preparation budget')
                digest.update(chunk)
                target.write(chunk)
        if not size or (expected is not None and digest.hexdigest() != expected):
            raise ValueError('Public download checksum mismatch')
        sha = file_digest(pending)
        payload = cache / sha
        try:
            os.link(pending, payload)
        except FileExistsError:
            if payload.is_symlink() or file_digest(payload) != sha:
                raise ValueError('Existing content-addressed payload is invalid')
        payload.chmod(0o444)
        record = {'sha256': sha, 'bytes': size, 'source': url}
        index.write_text(json.dumps(record, indent=2))
        return {**record, 'path': str(payload.resolve()), 'cache_hit': False}
    finally:
        pending.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--delivery', type=Path, required=True)
    parser.add_argument('--cache', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--task', action='append', required=True, choices=['caffe-cifar-10', 'path-tracing'])
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error('Download preparation must use the non-root test account')
    _, images = verify_delivery(args.delivery)
    for task in set(args.task):
        if 'https://astral.sh/uv/0.9.5/install.sh' not in (args.delivery / 'tasks' / task / 'tests/test.sh').read_text():
            raise ValueError('Task no longer uses the reviewed uv installer version')
    args.cache.mkdir(parents=True, exist_ok=True)
    with (args.cache / 'download.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        uv_index = args.cache / (hashlib.sha256((UV_BASE + '/' + UV_NAME).encode()).hexdigest() + '.json')
        if uv_index.exists():
            uv_sha = json.loads(uv_index.read_text())['sha256']
        else:
            with urllib.request.urlopen(UV_BASE + '/' + UV_NAME + '.sha256', timeout=30) as response:
                uv_sha = response.read(512).decode().split()[0]
        if len(uv_sha) != 64 or any(c not in '0123456789abcdef' for c in uv_sha):
            raise ValueError('Invalid published uv checksum')
        uv = fetch(UV_BASE + '/' + UV_NAME, args.cache, expected=uv_sha, maximum=64 * 1024**2)
        downloads = {}
        for task in sorted(set(args.task)):
            base = {'task_name': task, 'task_sha256': task_digest(args.delivery / 'tasks' / task),
                    'image_digest': images[task]['source_platform_digest']}
            mirror = 'http://198.18.0.1:3128/harbor-cache/' + task + '/uv-0.9.5'
            downloads[mirror + '/' + UV_NAME] = {**base, **uv, 'kind': 'uv', 'mirror_base': mirror}
            if task == 'caffe-cifar-10':
                cifar = fetch(CIFAR_URL.replace('http:', 'https:', 1), args.cache,
                              expected=CIFAR_MD5, algorithm='md5')
                downloads[CIFAR_URL] = {**base, **cifar, 'kind': 'dataset',
                                       'published_md5': CIFAR_MD5}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        if args.output.exists():
            raise ValueError('Download binding exists; choose a new output to preserve previous inputs')
        args.output.write_text(json.dumps({'schema': 1, 'downloads': downloads}, indent=2))
    print(json.dumps({'artifacts': len(downloads), 'bytes': sum(row['bytes'] for row in downloads.values())}))


if __name__ == '__main__':
    main()
