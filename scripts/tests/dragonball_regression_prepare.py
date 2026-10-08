#!/usr/bin/env python3
"""Lock and admit explicit regression provisioning/builds before running Make.

This is the existing prepare target: it may install/download dependencies through
server --setup-only. Execution never silently invokes it. All output is retained.
"""
import argparse
import json
import os
from pathlib import Path
import pwd
import sys
import signal

from dragonball_m4_host import host_lock, lock_fds
from dragonball_m4_preflight import check
from dragonball_regression import execute, file_digest, fixture_state_parent, validate_fixtures
from dragonball_test_resources import RunRecord


def prepare(command, minimum_gib=20):
    with host_lock():
        env = dict(os.environ)
        paths = validate_fixtures(env, prepared=False)
        state = Path(env['AENV_TEST_STATE_DIR'])
        disk_path = state if state.exists() else fixture_state_parent(state)
        report = check({'mode': 'build', 'run_user': pwd.getpwuid(os.geteuid()).pw_name,
                        'disk_path': str(disk_path), 'reserve_bytes': minimum_gib * 1024**3,
                        'planned_bytes': 0, 'retained_bytes': 0,
                        'required_tools': ['cc', 'make'],
                        'required_env': ['AENV_CONFIG_PATH', 'AENV_TEST_STATE_DIR', 'AENV_HOME_PATH',
                                         'AENV_RUNTIME_PATH', 'AENV_DEPS_PATH'],
                        'socket_paths': [str(Path(env['AENV_RUNTIME_PATH']) / 'db-runtime-XXXXXX/runner.sock')]}, env)
        if not report['ok']:
            raise ValueError('preparation preflight refused: ' + '; '.join(report['errors']))
        if not state.exists():
            state.mkdir(parents=True, mode=0o700)
        # Never overwrite another run's preparation logs or evidence.
        import tempfile
        output = Path(tempfile.mkdtemp(prefix='prepare-', dir=state))
        with RunRecord(output, 'regression-preparation', owned=[state]) as record:
            (output / 'preflight.json').write_text(json.dumps(report, indent=2) + '\n')
            code = execute(command, output / 'build.log', timeout=7200)
            record.record['exit_code'] = code
            if code:
                raise RuntimeError('preparation failed; see ' + str(output / 'build.log'))
            validate_fixtures(env)
            record.record['inputs'] = {'generated_configs': {str(path): file_digest(path) for path in paths}}
            record.record['cleanup'] = 'build process exited; generated state retained'
        print(output / record.path.name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assert-locked', action='store_true')
    parser.add_argument('--min-free-gib', type=int, default=20)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.assert_locked:
        if not lock_fds():
            raise ValueError('private preparation target requires an inherited host lock')
        return
    if not args.command or args.min_free_gib < 1:
        parser.error('a command and positive disk budget are required')
    prepare(args.command, args.min_free_gib)


if __name__ == '__main__':
    from dragonball_regression import interrupt
    signal.signal(signal.SIGTERM, interrupt)
    main()
