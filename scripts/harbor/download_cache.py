"""Read-only HTTP delivery of explicitly frozen public download artifacts."""
import json
from pathlib import Path
import re
from urllib.parse import urlsplit

from dependencies import file_digest


def load_cache(path):
    record = json.loads(Path(path).read_text())
    if record.get('schema') != 1 or not isinstance(record.get('downloads'), dict):
        raise ValueError('Invalid download cache schema')
    for url, row in record['downloads'].items():
        target = urlsplit(url)
        payload = Path(row['path'])
        if (target.scheme != 'http' or not target.hostname or target.username or target.password or
                target.query or target.fragment or payload.is_symlink() or not payload.is_file() or
                type(row.get('bytes')) is not int or not 0 < row['bytes'] <= 256 * 1024**2 or
                not re.fullmatch(r'[0-9a-f]{64}', row.get('sha256', '')) or
                not re.fullmatch(r'[0-9a-f]{64}', row.get('task_sha256', '')) or
                not re.fullmatch(r'sha256:[0-9a-f]{64}', row.get('image_digest', '')) or
                payload.stat().st_size != row['bytes'] or file_digest(payload) != row['sha256']):
            raise ValueError('Download cache payload or binding is invalid')
    return record['downloads']


def validate_bindings(entries, tasks):
    selected = {}
    for url, row in entries.items():
        binding = tasks.get(row['task_name'])
        if binding is None:
            continue
        if any(row[name] != binding[name] for name in ('task_sha256', 'image_digest')):
            raise ValueError('Download cache task source or image changed')
        selected[url] = row
    return selected


def lookup(entries, url, headers):
    if any(name.lower() in ('authorization', 'cookie') for name in headers):
        return None
    return entries.get(url)
