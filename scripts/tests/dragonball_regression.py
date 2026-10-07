#!/usr/bin/env python3
"""Resume frozen Dragonball functional suites through the existing capability runner.

Build/provision separately. Select lifecycle, memory, slots, startup or compatibility suites explicitly.
Python 3.11+ is required for reading the active TOML config.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shutil
import signal
import subprocess
import sys
import time
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SUITES = {
    "lifecycle": ["dragonball_lifecycle::"],
    "memory": ["dragonball_memory::"],
    "slots": ["dragonball_slots::"],
    "startup": ["dragonball_startup::"],
    "compatibility": ["dragonball_compatibility::"],
}
DEFAULT_SUITES = ["lifecycle", "memory", "slots", "startup"]

REQUIRED = (
    "AENV_CONFIG_PATH", "AENV_TEST_STATE_DIR", "AENV_HOME_PATH", "AENV_RUNTIME_PATH",
    "AENV_DEPS_PATH", "AENV_DRAGONBALL_M0_ASSETS", "AENV_DRAGONBALL_RUNTIME_RUNNER",
    "AENV_DRAGONBALL_FAULT_RUNNER", "AENV_UBLK_DAEMON_BINARY_PATH",
)
# Make exports command-line variables; resume is controller state, not guest input.
# Stage selection and timeout/resource budgets are validated separately.
INTERNAL_ENV = {"AENV_DRAGONBALL_M0_PARENT_NETNS", "AENV_DRAGONBALL_M0_ISOLATED",
                "AENV_REGRESSION_ARGS"}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def file_digest(path):
    path = Path(path)
    if not path.is_file():
        raise ValueError(f"missing input file: {path}")
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def environment_digest(env):
    # Store only the digest: configuration/credentials must not enter receipts.
    return digest({k: v for k, v in env.items() if k not in INTERNAL_ENV and (
        k.startswith(("AENV_", "AGENTENV_", "RUST_", "LD_", "LC_"))
        or k in {"PATH", "HOME", "TMPDIR", "LANG", "HOST_RESOLV_CONF"}
    )})


def verify_environment(expected, env):
    if environment_digest(env) != expected:
        raise ValueError("test environment changed across capability/namespace boundary")


def global_configs(config, config_dir, env):
    def resolve(raw):
        expanded = raw.replace("$AENV_HOME", env["AENV_HOME_PATH"])
        return (config_dir / expanded).resolve()
    disk = resolve(config.get("ublk", {}).get("overlaybd", {}).get(
        "global_config_path", "$AENV_HOME/overlaybd/overlaybd-global.json"))
    memory = resolve(config.get("memory_snapshot", {}).get(
        "overlaybd_global_config_path", "$AENV_HOME/overlaybd/mem-overlaybd-global.json"))
    return [disk, memory, disk.parent / "resize-overlaybd-global.json",
            disk.parent / "convert-overlaybd-global.json"]


def collect_inputs(binary, env, root=ROOT):
    for name in REQUIRED:
        if not env.get(name):
            raise ValueError(f"explicit {name} is required")
    import tomllib
    config_path = Path(env["AENV_CONFIG_PATH"]).resolve(strict=True)
    config = tomllib.loads(config_path.read_text())
    files = {Path(binary).resolve(), config_path, Path(__file__).resolve()}
    for name in REQUIRED:
        path = Path(env[name])
        if not path.is_absolute():
            raise ValueError(f"{name} must be absolute")
        if name not in {"AENV_TEST_STATE_DIR", "AENV_HOME_PATH", "AENV_RUNTIME_PATH", "AENV_DEPS_PATH"}:
            files.add(path)
    globals_ = global_configs(config, config_path.parent, env)
    for path in globals_:
        file_digest(path)
        json.loads(path.read_text())
    files.update(globals_)
    if env.get("HOST_RESOLV_CONF"):
        files.add(Path(env["HOST_RESOLV_CONF"]))
    if Path("/etc/overlaybd/overlaybd.json").is_file():
        files.add(Path("/etc/overlaybd/overlaybd.json"))
    assets = json.loads(Path(env["AENV_DRAGONBALL_M0_ASSETS"]).read_text())
    for key in ("runner_path", "kernel_path", "tools_path", "image_path"):
        path = Path(assets[key])
        if not path.is_absolute():
            raise ValueError(f"asset {key} must be absolute")
        files.add(path)
    image = json.loads(Path(assets["image_path"]).read_text())
    if not image.get("lowers") or image.get("upper"):
        raise ValueError("guest image must have immutable local lowers and no upper")
    for lower in image["lowers"]:
        path = Path(lower["file"])
        if not path.is_absolute():
            raise ValueError("image lower must be an absolute local file")
        files.add(path)
    # Include installed tools/libraries, not only the dependency directory name.
    deps = Path(env["AENV_DEPS_PATH"])
    if not deps.is_dir():
        raise ValueError("missing AENV_DEPS_PATH directory")
    for path in deps.rglob("*"):
        if path.is_symlink() and path.is_dir():
            raise ValueError(f"directory symlink in dependencies cannot be fingerprinted: {path}")
        if path.is_file():
            files.add(path)
    for key, value in env.items():
        if key not in INTERNAL_ENV and not key.endswith("_NETNS") and key.startswith(("AENV_", "AGENTENV_")) and Path(value).is_absolute():
            if Path(value).is_file():
                files.add(Path(value))
    for tool in ("cc", "debugfs", "ip", "nsenter", "unshare", "bash", "python3"):
        path = shutil.which(tool)
        if not path:
            raise ValueError(f"missing tool: {tool}")
        files.add(Path(path))
    return {
        "schema": 1, **source_identity(root),
        "files": {str(p): file_digest(p) for p in sorted(files)},
        "environment": environment_digest(env),
        "host": [platform.node(), platform.release(), platform.machine(), os.getuid(), os.getgroups(),
                 Path("/proc/sys/kernel/random/boot_id").read_text().strip()],
    }


def source_identity(root):
    """Fingerprint tracked and non-ignored inputs, including modes and symlinks."""
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard",
         "--", ".", ":!:**/__pycache__/**"], cwd=root).split(b"\0")
    source = {}
    for name in sorted(set(tracked) - {b""}):
        path = root / os.fsdecode(name)
        source[os.fsdecode(name)] = (
            [file_digest(path), path.stat().st_mode & 0o777, os.readlink(path) if path.is_symlink() else None]
            if path.is_file() else None)
    return {"source": digest(source),
            "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()}


def check_capacity(path, minimum_bytes, minimum_inodes):
    stat = os.statvfs(path)
    if stat.f_bavail * stat.f_frsize < minimum_bytes:
        raise ValueError(f"insufficient free space: {path}")
    if stat.f_favail < minimum_inodes:
        raise ValueError(f"insufficient free inodes: {path}")


def check_socket_path(runtime):
    # db-runtime- + tempfile's six random characters + runner.sock.
    if len(os.fsencode(str(runtime / "db-runtime-XXXXXX/runner.sock"))) >= 108:
        raise ValueError(f"runtime socket path exceeds Linux sun_path: {runtime}")


def preflight(env, minimum_gib):
    if os.geteuid() == 0 or (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise ValueError("requires non-root x86_64 Linux")
    if os.sysconf("SC_PAGE_SIZE") != 4096:
        raise ValueError("requires 4 KiB pages")
    if env.get("AENV_DRAGONBALL_M0_ISOLATED") != "1":
        raise ValueError("use the existing isolated capability runner")
    if os.readlink("/proc/self/ns/net") == env.get("AENV_DRAGONBALL_M0_PARENT_NETNS"):
        raise ValueError("test network namespace is not isolated")
    status = Path("/proc/self/status").read_text()
    effective = int(re.search(r"CapEff:\s*(\w+)", status)[1], 16)
    if effective & ((1 << 12) | (1 << 21)) != ((1 << 12) | (1 << 21)):
        raise ValueError("missing CAP_NET_ADMIN/CAP_SYS_ADMIN")
    for name in ("AENV_DRAGONBALL_RUNTIME_RUNNER", "AENV_DRAGONBALL_FAULT_RUNNER", "AENV_UBLK_DAEMON_BINARY_PATH"):
        if not os.access(env[name], os.X_OK):
            raise ValueError(f"not executable: {name}")
    with open("/dev/kvm", "rb+", buffering=0) as kvm:
        if fcntl.ioctl(kvm, 0xAE00) != 12:
            raise ValueError("unsupported KVM API")
    with open("/dev/ublk-control", "rb+", buffering=0):
        pass
    state = Path(env["AENV_TEST_STATE_DIR"]).resolve(strict=True)
    for name in ("AENV_HOME_PATH", "AENV_RUNTIME_PATH"):
        path = Path(env[name]).resolve(strict=True)
        if state not in path.parents or not os.access(path, os.W_OK):
            raise ValueError(f"{name} must be writable and inside isolated AENV_TEST_STATE_DIR")
        check_capacity(path, minimum_gib * 1024**3, 10000)
    check_capacity(Path(tempfile.gettempdir()), minimum_gib * 1024**3, 10000)
    check_socket_path(Path(env["AENV_RUNTIME_PATH"]))
    # Conservative admission: unresolved leftovers need an audit, not a blind retry.
    if list(Path("/sys/class/block").glob("ublkb*")):
        raise ValueError("active ublk devices: audit test resources before running/resuming")
    binaries = {str(Path(env[k]).resolve()) for k in ("AENV_DRAGONBALL_RUNTIME_RUNNER", "AENV_DRAGONBALL_FAULT_RUNNER", "AENV_UBLK_DAEMON_BINARY_PATH")}
    assets = json.loads(Path(env["AENV_DRAGONBALL_M0_ASSETS"]).read_text())
    binaries.add(str(Path(assets["runner_path"]).resolve()))
    for proc in Path("/proc").glob("[0-9]*/cmdline"):
        try:
            args = proc.read_bytes().split(b"\0")
        except (OSError, ProcessLookupError):
            continue
        if args and args[0].startswith(b"/") and os.path.realpath(os.fsdecode(args[0])) in binaries:
            raise ValueError(f"active runtime process {proc.parent.name}: audit before resuming")


def save(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def execute(command, log, timeout, *, grace=30):
    with log.open("xb") as stream:
        process = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            return process.wait(timeout=timeout)
        except (KeyboardInterrupt, subprocess.TimeoutExpired):
            # Keep the stage non-passing; let libtest/runtime cleanup run first.
            for sig, wait_seconds in ((signal.SIGINT, grace), (signal.SIGTERM, 5), (signal.SIGKILL, 5)):
                try:
                    os.killpg(process.pid, sig)
                except ProcessLookupError:
                    break
                deadline = time.monotonic() + wait_seconds
                while time.monotonic() < deadline:
                    process.poll()  # Reap the leader without abandoning descendants.
                    try:
                        os.killpg(process.pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.05)
            process.wait(timeout=5)
            raise


def run_stages(binary, output, stages, identity, *, resume, check_inputs, timeout=1800):
    output.mkdir(parents=True, exist_ok=True)
    with (output / ".lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("another regression owns this output") from None
        path = output / "results.json"
        if path.exists():
            if not resume:
                raise ValueError("output exists; use --resume or a new output directory")
            receipt = json.loads(path.read_text())
            if receipt["identity"] != identity or receipt["stages"] != stages:
                raise ValueError("inputs changed; use a new output directory")
        else:
            if resume:
                raise ValueError("no receipt to resume")
            receipt = {"identity": identity, "stages": stages, "attempts": []}
        for stage in stages:
            listing = subprocess.check_output([str(binary), *SUITES[stage], "--ignored", "--list"], text=True, timeout=30)
            names = [line[:-6] for line in listing.splitlines() if line.endswith(": test")]
            if not names:
                raise ValueError(f"no tests matched {stage}")
            previous = [r for r in receipt["attempts"] if r["stage"] == stage]
            if previous and previous[-1]["status"] == "passed":
                last = previous[-1]
                if last["tests"] != names or file_digest(output / last["log"]) != last["log_sha256"]:
                    raise ValueError(f"passed evidence changed for {stage}")
                print(f"{stage}: reused {len(names)} passed tests (not newly executed)", flush=True)
                continue
            log = output / f"{stage}-{len(previous) + 1}.log"
            attempt = {"stage": stage, "tests": names, "log": log.name, "status": "running", "started": time.time()}
            receipt["attempts"].append(attempt)
            save(path, receipt)
            print(f"{stage}: running {len(names)} tests; log: {log}", flush=True)
            started = time.monotonic()
            try:
                code = execute([str(binary), *SUITES[stage], "--ignored", "--test-threads=1", "--nocapture"], log, timeout)
                summaries = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", log.read_text(errors="replace"))
                passed = code == 0 and summaries and summaries[-1] == (str(len(names)), "0", "0")
                attempt.update(exit_code=code, status="passed" if passed else "failed")
                if check_inputs() != identity:
                    attempt["status"] = "invalidated"
            except (KeyboardInterrupt, subprocess.TimeoutExpired):
                attempt["status"] = "interrupted"
            except Exception:
                attempt["status"] = "invalidated"
                raise
            finally:
                attempt.update(seconds=round(time.monotonic() - started, 3), ended=time.time())
                if log.exists():
                    attempt["log_sha256"] = file_digest(log)
                save(path, receipt)
            print(f"{stage}: {attempt['status']} ({attempt['seconds']}s)", flush=True)
            if attempt["status"] != "passed":
                return 1
        return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="prebuilt DB-only integration libtest harness")
    parser.add_argument("--output", required=True, type=Path, help="new evidence directory outside the source checkout")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--suite", action="append", choices=tuple(SUITES))
    parser.add_argument("--preflight-only", action="store_true")
    parser.add_argument("--min-free-gib", type=int, default=20)
    parser.add_argument("--timeout", type=int, default=1800, help="seconds per stage")
    parser.add_argument("--inside", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.version_info < (3, 11):
        parser.error("Python 3.11+ required")
    if args.min_free_gib < 1 or args.timeout < 1:
        parser.error("budgets must be positive")
    binary, output = args.binary.resolve(strict=True), args.output.resolve()
    if output == ROOT or ROOT in output.parents:
        parser.error("output must be outside the source checkout")
    os.environ.setdefault("AENV_RUN_USER", pwd.getpwuid(os.getuid()).pw_name)
    env = dict(os.environ)
    if not args.inside:
        # Fail on missing config/assets before invoking sudo or namespace setup.
        collect_inputs(binary, env)
        # sudo may reset these non-secret paths despite -E. Restore them
        # after capability delegation, then verify the complete relevant env.
        path_env = ["env"] + [f"{key}={env[key]}" for key in ("PATH", "HOME", "TMPDIR") if key in env]
        command = ["bash", str(ROOT / "scripts/tests/run-dragonball-tests.sh"), "--runtime",
                   *path_env, sys.executable, str(Path(__file__).resolve()),
                   *sys.argv[1:], "--inside", environment_digest(env)]
        os.execvpe(command[0], command, env)
    verify_environment(args.inside, env)
    identity = collect_inputs(binary, env)
    preflight(env, args.min_free_gib)
    identity["budgets"] = [args.min_free_gib, args.timeout]
    if args.preflight_only:
        print("preflight passed; no VM tests executed")
        return 0
    # Also serialize separate output directories sharing this writable state.
    with (Path(env["AENV_TEST_STATE_DIR"]) / ".regression.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        def current():
            result = collect_inputs(binary, env)
            result["budgets"] = identity["budgets"]
            return result
        return run_stages(binary, output, list(dict.fromkeys(args.suite or DEFAULT_SUITES)),
                          identity, resume=args.resume, check_inputs=current, timeout=args.timeout)


def interrupt(*_):
    raise KeyboardInterrupt()


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupt)
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"regression blocked: {error}", file=sys.stderr)
        sys.exit(2)
