"""Serialize test entrypoints on one host, across checkouts and state roots."""
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import stat
import sys

LOCK_PATH = Path("/var/tmp/agentenv-dragonball-test-host.lock")


def acquire_host_lock(path=LOCK_PATH):
    # Open existing files read-only so root and the delegated test account can
    # share flock without truncation or Linux protected_regular restrictions.
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
    try:
        fd = os.open(path, flags)
    except FileNotFoundError:
        try:
            fd = os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o644)
            os.fchmod(fd, 0o644)
        except FileExistsError:
            fd = os.open(path, flags)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("test host lock must be a regular file")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError("Dragonball test host is busy: " + str(path)) from error
        return fd
    except BaseException:
        os.close(fd)
        raise


def require_clean_host(cgroups=Path("/sys/fs/cgroup"), devices=Path("/dev")):
    residuals = sorted(str(p) for p in cgroups.glob("db-m4.*"))
    residuals.extend(sorted(str(p) for pattern in ("ublkc[0-9]*", "ublkb[0-9]*")
                            for p in devices.glob(pattern)))
    if residuals:
        raise RuntimeError("test host has residual resources; audit before starting: " + ", ".join(residuals))


LOCK_FD_ENV = "AENV_TEST_HOST_LOCK_FD"


def lock_fds(path=None):
    """Validate a real inherited lease, never an environment-only bypass."""
    path = LOCK_PATH if path is None else path
    raw = os.environ.get(LOCK_FD_ENV)
    if raw is None:
        return ()
    try:
        fd = int(raw)
        actual, expected = os.fstat(fd), os.stat(path, follow_symlinks=False)
        if (fd < 3 or not stat.S_ISREG(actual.st_mode)
                or not stat.S_ISREG(expected.st_mode)
                or (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino)):
            raise ValueError("descriptor does not identify the host lock")
        # Reaffirm exclusion on this open file description. An unrelated opener
        # of the same inode fails if another description owns the lease.
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except (OSError, ValueError) as error:
        raise ValueError("invalid inherited host lock descriptor") from error
    return (fd,)


@contextmanager
def host_lock(path=None):
    path = LOCK_PATH if path is None else path
    inherited = lock_fds(path)
    fd = os.dup(inherited[0]) if inherited else acquire_host_lock(path)
    old = os.environ.get(LOCK_FD_ENV)
    os.set_inheritable(fd, True)
    os.environ[LOCK_FD_ENV] = str(fd)
    try:
        yield fd
    finally:
        if old is None:
            os.environ.pop(LOCK_FD_ENV, None)
        else:
            os.environ[LOCK_FD_ENV] = old
        # Closing our duplicate must not explicitly unlock surviving children.
        os.close(fd)


def exec_locked(argv, path=LOCK_PATH):
    with host_lock(path):
        require_clean_host()
        os.execvp(argv[0], argv)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit("usage: dragonball_m4_host.py <command> [arguments...]")
    exec_locked(sys.argv[1:])
