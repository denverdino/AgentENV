#!/usr/bin/env python3
"""Read-only M4 host admission; emit all prerequisite failures before allocating VMs.

Call check(profile) under the entrypoint's host lock immediately before Lab setup.
Required environment names, concrete longest socket paths, and disk budgets come
from the calling suite. Docker is opt-in. This check installs or changes nothing.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import pwd
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
from dragonball_m4_host import require_clean_host
import dragonball_m4_retention as retention

CGROUP_ROOT = Path('/sys/fs/cgroup')
DEVICE_ROOT = Path('/dev')
M4_TOOLS = ('ip', 'nsenter', 'unshare', 'setpriv', 'nft', 'numactl')


def check(profile, env=None):
    """Return a JSON-safe report; ok=False means the caller must refuse execution.

Required profile fields: disk_path, reserve_bytes, planned_bytes, retained_bytes.
Optional fields: mode (m4, build or functional), run_user (aenv), required_tools, required_env,
socket_paths (exact paths, including generated suffixes), docker (False),
input_files (dragonball_m4_inputs profile checked as the workload account).
M4 checks root orchestration, controllers and runuser transfer; build/functional
check the current non-root account and subprocess transfer. Functional also
checks KVM. This does not qualify a later capability wrapper boundary.
"""
    env = os.environ if env is None else env
    report = {'schema': 1, 'read_only': True, 'ok': False, 'checks': {}, 'errors': []}

    def probe(name, action):
        try:
            details = action()
            report['checks'][name] = {'ok': True, 'details': details}
            return details
        except (OSError, ValueError, RuntimeError, KeyError, subprocess.SubprocessError) as error:
            report['checks'][name] = {'ok': False, 'error': str(error)}
            report['errors'].append(name + ': ' + str(error))
            return None

    def require(condition, message):
        if not condition:
            raise ValueError(message)
        return True

    def validate_profile():
        required = {'disk_path', 'reserve_bytes', 'planned_bytes', 'retained_bytes'}
        optional = {'run_user', 'required_tools', 'required_env', 'socket_paths', 'docker', 'mode', 'input_files'}
        require(isinstance(profile, dict), 'profile must be an object')
        require(not set(profile) - required - optional, 'unknown profile fields: ' +
                ', '.join(sorted(str(key) for key in set(profile) - required - optional)))
        require(required <= profile.keys(), 'missing profile fields: ' +
                ', '.join(sorted(required - profile.keys())))
        for name in ('required_tools', 'required_env', 'socket_paths'):
            values = profile.get(name, ())
            require(isinstance(values, (list, tuple)) and
                    all(isinstance(value, str) and value and '\0' not in value for value in values),
                    name + ' must be a list of nonempty strings')
        require(profile.get('mode', 'm4') in ('m4', 'build', 'functional'), 'unsupported mode')
        require(type(profile.get('docker', False)) is bool, 'docker must be boolean')
        require(isinstance(profile.get('run_user', 'aenv'), str) and profile.get('run_user', 'aenv'),
                'run_user must be a nonempty account name')
        require(isinstance(profile['disk_path'], (str, Path)), 'disk_path must be a path')
        if 'input_files' in profile:
            from dragonball_m4_inputs import validate
            validate(profile['input_files'])
        return True

    if not probe('profile', validate_profile):
        return report

    mode = profile.get('mode', 'm4')
    linux = probe('platform', lambda: require(platform.system() == 'Linux', 'Linux required'))
    python = probe('python', lambda: require(sys.version_info >= (3, 11), 'Python 3.11+ required'))
    root = probe('orchestrator', lambda: require(
        os.geteuid() == 0 if mode == 'm4' else os.geteuid() != 0,
        'root orchestration required' if mode == 'm4' else 'non-root orchestration required'))
    run_user = profile.get('run_user', 'aenv')

    def account():
        entry = pwd.getpwnam(run_user)
        require(entry.pw_uid != 0, 'workload account must be non-root')
        if mode != 'm4':
            require(entry.pw_uid == os.geteuid(), 'workload account differs from current account')
        return {'name': run_user, 'uid': entry.pw_uid}

    target = probe('account', account)
    if 'input_files' in profile:
        def input_files():
            from dragonball_m4_inputs import check as check_inputs
            require(bool(target and root and linux), 'requires valid platform and workload account')
            result = check_inputs(profile['input_files'], run_user=run_user)
            require(result['ok'], '; '.join(result['errors']))
            return result
        probe('input_files', input_files)
    tools = {}
    names = list(profile.get('required_tools', M4_TOOLS if mode == 'm4' else ()))
    if mode == 'm4':
        names.append('runuser')
    if profile.get('docker', False):
        names.append('docker')
    for name in dict.fromkeys(names):
        def tool(name=name):
            path = shutil.which(name)
            require(path is not None, 'missing executable: ' + name)
            return path
        tools[name] = probe('tool:' + name, tool)
    for name in profile.get('required_env', ()):
        probe('env:' + name, lambda name=name: require(
            isinstance(env.get(name), str) and bool(env[name].strip()),
                                                       'missing explicit environment: ' + name))

    def socket_path(path):
        encoded = os.fsencode(path)
        require(bool(encoded) and b'\0' not in encoded and len(encoded) < 108,
                'Unix socket path must fit 107 bytes plus terminator: ' + os.fsdecode(path))
        return {'path': os.fsdecode(path), 'encoded_bytes': len(encoded)}

    for index, path in enumerate(profile.get('socket_paths', ())):
        probe('socket:' + str(index), lambda path=path: socket_path(path))

    def disk():
        budgets = {name: profile[name] for name in ('reserve_bytes', 'planned_bytes', 'retained_bytes')}
        require(all(type(value) is int and value >= 0 for value in budgets.values()),
                'disk budgets must be nonnegative integers')
        return retention.admit(profile['disk_path'], **budgets)

    probe('disk', disk)

    def controllers():
        available = set((CGROUP_ROOT / 'cgroup.subtree_control').read_text().split())
        missing = {'cpu', 'memory', 'pids'} - available
        require(not missing, 'missing cgroup controllers: ' + ', '.join(sorted(missing)))
        return sorted(available)

    if mode == 'm4':
        probe('controllers', controllers)
    probe('clean_host', lambda: require_clean_host(CGROUP_ROOT, DEVICE_ROOT))

    def delegated(argv, *, capture=False):
        require(bool(linux and python and root and target and (mode != 'm4' or tools['runuser'])),
                'requires Linux, supported Python and valid orchestration/workload accounts')
        command = [tools['runuser'], '-u', run_user, '--', *argv] if mode == 'm4' else argv
        result = subprocess.run(command,
                                text=True, capture_output=True, timeout=10)
        require(result.returncode == 0,
                f'delegated probe failed (exit {result.returncode})')
        return result.stdout if capture else {'run_user': run_user}

    # Opening and closing KVM checks target-user access without a VM-create ioctl.
    uid = target['uid'] if target else -1
    kvm = ('import os; '
           f'assert os.geteuid() == {uid} and os.geteuid() != 0; '
           "fd = os.open('/dev/kvm', os.O_RDWR); os.close(fd)")
    if mode != 'build':
        probe('kvm', lambda: delegated([sys.executable, '-c', kvm]))
    if profile.get('required_env'):
        def delegated_environment():
            names = profile['required_env']
            require(all(report['checks']['env:' + name]['ok'] for name in names),
                    'requires nonempty explicit source environment')
            # Only names cross argv. Values stay in the respective environments;
            # neither subprocess output nor the report contains their contents.
            script = (
                'import hashlib,json,os,sys; names=json.loads(sys.argv[1]); '
                'values={name:os.environ.get(name) for name in names}; '
                'missing=[name for name,value in values.items() if not value or not value.strip()]; '
                "digest=hashlib.sha256(json.dumps(values,sort_keys=True,separators=(',',':')).encode()).hexdigest(); "
                "print(json.dumps({'missing':missing,'sha256':digest}))")
            output = delegated([sys.executable, '-c', script, json.dumps(names)], capture=True)
            observed = json.loads(output)
            require(isinstance(observed, dict) and set(observed) == {'missing', 'sha256'},
                    'invalid runuser environment response')
            require(observed['missing'] == [], 'required environment missing or empty after runuser')
            expected = hashlib.sha256(json.dumps({name: env[name] for name in names},
                sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            require(observed['sha256'] == expected, 'required environment changed after runuser')
            return {'boundary': 'runuser' if mode == 'm4' else 'subprocess',
                    'run_user': run_user, 'names': names}
        probe('runuser_environment', delegated_environment)
    if profile.get('docker', False):
        def docker():
            require(bool(tools['docker']), 'missing executable: docker')
            return delegated([tools['docker'], 'info', '--format', '{{.ServerVersion}}'])
        probe('docker', docker)
    report['ok'] = not report['errors']
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-user', default='aenv')
    parser.add_argument('--tool', action='append', dest='required_tools',
                        help='explicit tool profile; defaults to M4 tools')
    parser.add_argument('--required-env', action='append', default=[],
                        help='require a nonempty value unchanged across runuser')
    parser.add_argument('--socket-path', action='append', default=[], dest='socket_paths',
                        help='concrete longest socket path including generated suffixes')
    parser.add_argument('--disk-path', type=Path, required=True)
    for name in ('reserve', 'planned', 'retained'):
        parser.add_argument('--' + name + '-bytes', type=int, required=True)
    parser.add_argument('--docker', action='store_true', help='also require target-user Docker access')
    parser.add_argument('--input-files', type=json.loads, help='JSON files/json/toml/modules/assets profile')
    profile = vars(parser.parse_args())
    if profile['input_files'] is None:
        del profile['input_files']
    if profile['required_tools'] is None:
        profile['required_tools'] = M4_TOOLS
    report = check(profile)
    print(json.dumps(report, indent=2))
    return 0 if report['ok'] else 2


if __name__ == '__main__':
    raise SystemExit(main())
