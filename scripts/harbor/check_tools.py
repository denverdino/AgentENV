#!/usr/bin/env python3
"""Run Harbor checks with one selected Python; preserve the failing exit code."""
import os
from pathlib import Path
import subprocess
import sys


def require_python(version=None):
    if (sys.version_info if version is None else version) < (3, 12):
        raise ValueError('Harbor checks require Python 3.12+; set REGRESSION_PYTHON to the frozen environment')


def run_checks(root, *, suite=None, output=None):
    require_python()
    if os.geteuid() == 0:raise ValueError('Harbor checks must run as the non-root test account')
    root = Path(root);suite = Path(suite) if suite is not None else root / 'scripts/harbor'
    if (root / '.git').exists():
        guard = [sys.executable, '-B', str(root / 'scripts/harbor/check_env_files.py')]
    elif (root.parent / 'driver-sha256.json').is_file():
        guard = [sys.executable, '-B', str(root / 'scripts/harbor/freeze_driver.py'), '--verify', str(root.parent)]
    else:
        raise ValueError('Checks require a Git checkout or a frozen candidate manifest')
    commands = [guard,
                [sys.executable, '-B', '-m', 'unittest', 'discover', '-s', str(suite), '-p', 'test_*.py', '-v'],
                guard]
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
    for command in commands:
        result = subprocess.run(command, cwd=root, env=env, stdout=output,
                                stderr=subprocess.STDOUT if output is not None else None, timeout=600)
        if result.returncode:return result.returncode
    return 0


def main():
    try:
        require_python()
        print('Harbor checks: ' + sys.executable + ' / ' + sys.version.split()[0], flush=True)
        return run_checks(Path(__file__).resolve().parents[2])
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
