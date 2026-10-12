#!/usr/bin/env python3
"""Publish an isolated, verified copy; partial producer files remain evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile


def publish_verified(partial, output, expected_sha256, expected_bytes):
    partial, output = Path(partial), Path(output)
    if (not re.fullmatch(r'[0-9a-f]{64}', expected_sha256) or
            type(expected_bytes) is not int or expected_bytes <= 0):
        raise ValueError('Expected SHA256 and positive byte count are required')
    if output.exists() or output.is_symlink():
        raise FileExistsError('Final artifact already exists')
    fd = os.open(partial, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    temporary = None
    try:
        with os.fdopen(fd, 'rb') as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                raise ValueError('Transport input must be a regular file')
            with tempfile.NamedTemporaryFile(dir=output.parent, prefix='.delivery-', delete=False) as target:
                temporary = Path(target.name)
                digest = hashlib.sha256();size = 0
                # Copy to another inode: an unfinished producer can never mutate
                # the published artifact, even through an already-open descriptor.
                while block := source.read(min(1024**2, expected_bytes - size + 1)):
                    size += len(block)
                    if size > expected_bytes:
                        raise ValueError('Transport byte count differs from frozen input')
                    digest.update(block);target.write(block)
                if size != expected_bytes or digest.hexdigest() != expected_sha256:
                    raise ValueError('Transport is incomplete or differs from frozen input')
                target.flush();os.fsync(target.fileno())
        # Same-directory hard link publishes atomically without replacing any
        # existing file. The closed staging inode is no longer writable here.
        os.link(temporary, output)
        return {'schema': 1, 'path': str(output.absolute()), 'sha256': digest.hexdigest(), 'bytes': size}
    finally:
        if temporary is not None:temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--partial', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--bytes', type=int, required=True)
    args = parser.parse_args()
    try:
        receipt = publish_verified(args.partial, args.output, args.sha256, args.bytes)
    except (OSError, ValueError) as error:
        parser.exit(1, 'Artifact publication refused: ' + type(error).__name__ + '\n')
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
