"""Reject private dotenv files in the Git index, including forced additions."""
from pathlib import PurePosixPath
import subprocess


def forbidden(paths):
    return [path for path in paths if (PurePosixPath(path).name == '.env' or
            PurePosixPath(path).name.startswith('.env.')) and
            PurePosixPath(path).name != '.env.example']


def main():
    paths = subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')
    if forbidden(paths):
        raise SystemExit('Private .env files are tracked; remove them from the Git index')


if __name__ == '__main__':
    main()
