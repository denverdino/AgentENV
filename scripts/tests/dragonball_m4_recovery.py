#!/usr/bin/env python3
"""Audit a recorded M4 lab; explicitly recover only its proven cgroup.

Preserve all data and diagnostics. Historical ublk IDs cannot prove ownership
after a crash, so remaining devices require separate manual inspection.
"""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import time

from dragonball_m4_host import acquire_host_lock

RUNS = Path("/var/tmp")
CGROUPS = Path("/sys/fs/cgroup")
PROC = Path("/proc")
DEV = Path("/dev")
OWNER_UID = 0


def file_identity(path):
    info = path.stat()
    return [info.st_dev, info.st_ino]


def atomic_json(path, value):
    fd, temporary = tempfile.mkstemp(prefix=".lab-record-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
    finally:
        Path(temporary).unlink(missing_ok=True)


def save_manifest(root, manifest):
    atomic_json(root / "lab-manifest.json", manifest)


def process_start(pid):
    try:
        # comm is parenthesized and may itself contain spaces or parentheses.
        return (PROC / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[19]
    except (FileNotFoundError, ProcessLookupError):
        return None


def record_lab(lab, state="running"):
    namespaces = set()
    for pid in [os.getpid()] + [node.pid for node in lab.nodes if node.pid]:
        for name in ("net", "mnt"):
            namespaces.add(os.readlink(PROC / str(pid) / "ns" / name))
    manifest = {
        "schema": 1, "root": str(lab.root), "root_identity": file_identity(lab.root),
        "boot_id": (PROC / "sys/kernel/random/boot_id").read_text().strip(),
        "cgroup_identity": file_identity(lab.cgroup), "run_user": lab.run_user,
        "controller": {"pid": os.getpid(), "start_time": process_start(os.getpid())},
        "namespaces": sorted(namespaces), "state": state,
    }
    save_manifest(lab.root, manifest)


def trusted(path, directory=False):
    info = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(info.st_mode) or info.st_uid != OWNER_UID or info.st_mode & 0o022:
        raise ValueError("untrusted lab path: " + str(path))


def load_manifest(root):
    if not root.is_absolute() or root.parent != RUNS or not root.name.startswith("db-m4."):
        raise ValueError("expected an exact /var/tmp/db-m4.* root")
    trusted(root, directory=True)
    path = root / "lab-manifest.json"
    trusted(path)
    manifest = json.loads(path.read_text())
    if (manifest.get("schema") != 1 or manifest.get("root") != str(root)
            or manifest.get("root_identity") != file_identity(root)
            or manifest.get("boot_id") != (PROC / "sys/kernel/random/boot_id").read_text().strip()):
        raise ValueError("lab root or boot identity mismatch")
    group = CGROUPS / root.name
    if group.exists() and (group.is_symlink() or manifest.get("cgroup_identity") != file_identity(group)):
        raise ValueError("lab cgroup identity mismatch")
    return manifest, group


def references(root, namespaces):
    found = []
    prefix = str(root) + "/"
    root_pattern = re.compile(r"(?<!\S)" + re.escape(str(root)) + r"(?=/|\s|$)")
    def owned_path(target):
        target = target.removesuffix(" (deleted)")
        return target == str(root) or target.startswith(prefix)
    for proc in PROC.iterdir():
        if not proc.name.isdigit():
            continue
        try:
            kinds = []
            for name in ("maps", "mountinfo"):
                path = proc / name
                if path.exists():
                    content = path.read_text()
                    # mountinfo escapes whitespace and backslashes in paths.
                    content = re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), content)
                    if root_pattern.search(content) or (name == "mountinfo" and any(ns in content for ns in namespaces)):
                        kinds.append(name)
            for name in ("cwd", "root"):
                path = proc / name
                if path.is_symlink() and owned_path(os.readlink(path)):
                    kinds.append(name)
            for name in ("net", "mnt"):
                path = proc / "ns" / name
                if path.is_symlink() and os.readlink(path) in namespaces:
                    kinds.append("namespace:" + name)
            descriptors = proc / "fd"
            if descriptors.exists():
                for fd in descriptors.iterdir():
                    try:
                        target = os.readlink(fd)
                        if owned_path(target) or target in namespaces:
                            kinds.append("fd:" + fd.name)
                    except (FileNotFoundError, ProcessLookupError):
                        pass
            if kinds:
                found.append({"pid": int(proc.name), "kinds": kinds})
        except (FileNotFoundError, ProcessLookupError):
            pass
    return found


def audit(root):
    manifest, group = load_manifest(root)
    controller = manifest["controller"]
    alive = process_start(controller["pid"]) == controller["start_time"]
    devices = sorted(str(p) for pattern in ("ublkc[0-9]*", "ublkb[0-9]*") for p in DEV.glob(pattern))
    refs = references(root, manifest["namespaces"])
    status = "active" if alive else "orphaned" if group.exists() else "manual_review" if devices or refs else "clean"
    return {"schema": 1, "root": str(root), "status": status, "controller_alive": alive,
            "cgroup_present": group.exists(), "devices": devices, "references": refs,
            "errors": []}


def remove_cgroup_tree(group):
    from dragonball_m4_lab import empty_cgroup
    empty_cgroup(group)
    errors = []
    children = sorted((p for p in group.rglob("*") if p.is_dir()),
                      key=lambda p: len(p.parts), reverse=True)
    for directory in children + [group]:
        try:
            directory.rmdir()
        except OSError as error:
            errors.append(str(error))
    if errors:
        raise RuntimeError("; ".join(errors))


def recover(root, *, apply=False):
    """Caller holds the host lock. Audit is read-only; recovery retains receipts."""
    report = audit(root)
    if not apply or report["controller_alive"]:
        return report
    errors = []
    try:
        # Recheck identities immediately before mutating a verified cgroup.
        _, group = load_manifest(root)
        if group.exists():
            remove_cgroup_tree(group)
    except Exception as error:
        errors.append(str(error))
    try:
        report = audit(root)
    except Exception as error:
        errors.append(str(error))
    report["errors"] = errors
    if errors or report["cgroup_present"]:
        report["status"] = "manual_review"
    atomic_json(root / f"recovery-{time.time_ns()}.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--apply", action="store_true", help="recover proven cgroup; preserve files and devices")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("root orchestration required for complete host audit")
    lock = acquire_host_lock()
    try:
        report = recover(args.root, apply=args.apply)
        print(json.dumps(report, indent=2))
        return 0 if report["status"] == "clean" else 1
    finally:
        os.close(lock)


if __name__ == "__main__":
    raise SystemExit(main())
