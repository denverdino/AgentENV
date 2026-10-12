"""Private literal dotenv credentials, used only by Harbor integration labs."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import stat
import sys
from urllib.parse import urlsplit

FIELDS = ('ENDPOINT', 'REGION', 'BUCKET', 'PREFIX', 'ACCESS_KEY_ID', 'ACCESS_KEY_SECRET',
          'SECURITY_TOKEN', 'EXPIRATION')


def read_env(path, uid=None):
    path = Path(path)
    info = path.lstat()
    if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or
            info.st_uid != (os.geteuid() if uid is None else uid)):
        raise ValueError('Harbor OSS env must be a private regular file owned by the runtime user')
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        key, separator, value = line.partition('=')
        key, value = key.strip(), value.strip()
        if (not separator or key not in {'HARBOR_OSS_' + name for name in FIELDS} or
                key.removeprefix('HARBOR_OSS_') in values):
            raise ValueError('Unknown or duplicate Harbor OSS env key')
        if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
            value = value[1:-1]
        if any(char in value for char in ('$','`','\x00','\n','\r')):
            raise ValueError('Harbor OSS env uses literal values; expansion is unsupported')
        values[key.removeprefix('HARBOR_OSS_')] = value
    for name in ('ENDPOINT', 'REGION', 'BUCKET', 'PREFIX', 'ACCESS_KEY_ID', 'ACCESS_KEY_SECRET'):
        if not values.get(name):
            raise ValueError('Missing Harbor OSS env key: HARBOR_OSS_' + name)
    endpoint = urlsplit(values['ENDPOINT'])
    if (endpoint.scheme != 'https' or not endpoint.hostname or endpoint.username or
            endpoint.password or endpoint.path not in ('', '/') or endpoint.query or endpoint.fragment):
        raise ValueError('Harbor OSS endpoint must be credential-free HTTPS')
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]{2,62}', values['BUCKET']):
        raise ValueError('Invalid Harbor OSS bucket')
    if not re.fullmatch(r'[a-z0-9-]+', values['REGION']):
        raise ValueError('Invalid Harbor OSS region')
    if (values['PREFIX'].startswith('/') or '..' in values['PREFIX'].split('/') or
            not re.fullmatch(r'[A-Za-z0-9_./-]+', values['PREFIX'])):
        raise ValueError('Invalid Harbor OSS test prefix')
    return values


def snapshot_config(path, run_id, *, uid):
    values = read_env(path, uid)
    if not re.fullmatch(r'[A-Za-z0-9_.-]+', run_id):
        raise ValueError('Invalid Harbor OSS run ID')
    command = shlex.join([sys.executable, str(Path(__file__).resolve()),
                          'credentials', str(Path(path).absolute())])
    return {'endpoint': values['ENDPOINT'], 'region': values['REGION'],
            'bucket': values['BUCKET'], 'prefix': values['PREFIX'].rstrip('/') + '/' + run_id,
            'addressing_style': 'virtual', 'credential_process': command,
            'cache_max_size_gb': 2, 'timeout_secs': 30}


def credentials(path):
    values = read_env(path)
    record = {'AccessKeyId': values['ACCESS_KEY_ID'], 'AccessKeySecret': values['ACCESS_KEY_SECRET']}
    if values.get('SECURITY_TOKEN'):
        record['SecurityToken'] = values['SECURITY_TOKEN']
    if values.get('EXPIRATION'):
        record['Expiration'] = values['EXPIRATION']
    return record


def server_proxy_env():
    from egress_proxy import benchmark_proxy_env
    env = benchmark_proxy_env()
    # Registry and default guest control addresses live in Lab namespaces,
    # outside the proxy's outbound namespace (including envd readiness HTTP).
    bypass = env['no_proxy'] + ',10.241.0.1,10.11.0.0/16'
    env.update(no_proxy=bypass, NO_PROXY=bypass)
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['credentials'])
    parser.add_argument('env', type=Path)
    args = parser.parse_args()
    try:
        record = credentials(args.env)
    except (OSError, ValueError):
        parser.exit(1, 'Harbor OSS credentials unavailable; check private env input\n')
    print(json.dumps(record))


if __name__ == '__main__':
    main()
