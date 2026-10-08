#!/usr/bin/env python3
"""Read/parse/import frozen inputs as the workload account, without starting VMs.

Module probes execute trusted test drivers' imports; pass library files, not
scripts with top-level workloads. File contents and import output are not logged.
"""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import pwd
import subprocess
import sys
import tomllib

KINDS = {'files', 'executables', 'json', 'toml', 'modules', 'assets'}


def validate(profile):
    if not isinstance(profile, dict) or set(profile) - KINDS:
        raise ValueError('unknown input profile fields')
    for kind, paths in profile.items():
        if not isinstance(paths, list) or any(not isinstance(p, str) or '\0' in p
                                             or not Path(p).is_absolute() for p in paths):
            raise ValueError(kind + ' must contain absolute file paths')


def probe(profile, uid):
    validate(profile)
    report = {'ok': False, 'uid': os.geteuid(), 'checks': [], 'errors': []}
    if os.geteuid() != uid or uid == 0:
        report['errors'].append('expected non-root workload account')
        return report
    sys.dont_write_bytecode = True

    def read(path, kind):
        path = Path(path)
        if not path.is_file():
            raise ValueError('expected regular file')
        if kind == 'executables' and not os.access(path, os.X_OK):
            raise PermissionError('expected executable file')
        with path.open('rb') as stream:
            if kind == 'json':
                return json.load(stream)
            if kind == 'toml':
                return tomllib.load(stream)
            stream.read(1)

    for kind, paths in profile.items():
        for path in paths:
            try:
                if kind == 'assets':
                    assets = read(path, 'json')
                    for key in ('runner_path', 'kernel_path', 'tools_path', 'image_path'):
                        validate({'files': [assets[key]]})
                        read(assets[key], 'executables' if key == 'runner_path' else 'files')
                    image = read(assets['image_path'], 'json')
                    if not image.get('lowers') or image.get('upper'):
                        raise ValueError('expected immutable image lowers')
                    for lower in image['lowers']:
                        validate({'files': [lower['file']]})
                        read(lower['file'], 'files')
                elif kind == 'modules':
                    read(path, 'files')
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        sys.path.insert(0, str(Path(path).parent))
                        name = Path(path).stem
                        previous = sys.modules.get(name)
                        try:
                            spec = importlib.util.spec_from_file_location(name, path)
                            module = importlib.util.module_from_spec(spec)
                            sys.modules[spec.name] = module
                            spec.loader.exec_module(module)
                        finally:
                            sys.path.pop(0)
                            if previous is None:
                                sys.modules.pop(name, None)
                            else:
                                sys.modules[name] = previous
                else:
                    read(path, kind)
                report['checks'].append({'kind': kind, 'path': path})
            except (Exception, SystemExit) as error:
                report['errors'].append(f'{kind}: {path}: {type(error).__name__}')
    report['ok'] = not report['errors']
    return report


def check(profile, run_user=None):
    """Use a fresh interpreter so imports cannot reuse cached driver modules."""
    validate(profile)
    uid = pwd.getpwnam(run_user).pw_uid if run_user else os.geteuid()
    argv = [sys.executable, '-B', str(Path(__file__).resolve()), '--probe', json.dumps(profile), '--uid', str(uid)]
    if run_user and uid != os.geteuid():
        argv = ['runuser', '-u', run_user, '--', *argv]
    result = subprocess.run(argv, text=True, capture_output=True, timeout=30)
    try:
        report = json.loads(result.stdout)
        if not isinstance(report, dict) or type(report.get('ok')) is not bool or report.get('uid') != uid:
            raise ValueError('invalid input probe response')
    except (ValueError, TypeError):
        raise RuntimeError('input probe could not run as workload account') from None
    if result.returncode and report['ok']:
        raise RuntimeError('input probe failed after reporting success')
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', required=True, help='JSON input profile')
    parser.add_argument('--uid', required=True, type=int)
    args = parser.parse_args()
    result = probe(json.loads(args.probe), args.uid)
    print(json.dumps(result))
    raise SystemExit(0 if result['ok'] else 2)
