#!/usr/bin/env python3
"""Prepare three unmodified Harbor official examples before allocating VMs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

TASKS = ("hello-workdir", "hello-user", "hello-multi-step-simple")
HARBOR_REVISION = "4b94505a91c5ddcb70b5740ac95718ddee13e5a0"
BENCHMARK_TASKS = ("openssl-selfsigned-cert", "log-summary-date-ranges")
BENCHMARK_REVISION = "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
COMPUTER_TASKS = ('computer-1-click-calibration',)


def task_environment_name(task):
    from harbor.models.task.config import TaskConfig
    config = TaskConfig.model_validate_toml((task / 'task.toml').read_text())
    return config.task.short_name if config.task is not None else task.name


def verify_tree(root: Path, files):
    actual = {path.relative_to(root).as_posix() for path in root.rglob("*")
              if path.is_file() and "__pycache__" not in path.parts}
    if actual != set(files):
        raise ValueError("Source file set changed: " + str(root))
    for name, expected in files.items():
        path = root / name
        if path.is_symlink():
            raise ValueError("Source file replaced by symlink: " + name)
        with path.open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != expected:
                raise ValueError("Source changed: " + name)


def verify_source(root: Path, manifest_path: Path):
    manifest = json.loads(manifest_path.read_text())
    if manifest["revision"] != HARBOR_REVISION or not manifest["files"]:
        raise ValueError("Unexpected Harbor source revision or empty source manifest")
    for prefix in ("src", *("examples/tasks/" + name for name in TASKS)):
        selected = {name[len(prefix) + 1:]: digest for name, digest in manifest["files"].items()
                    if name.startswith(prefix + "/")}
        if not selected:
            raise ValueError("Missing source manifest subtree: " + prefix)
        verify_tree(root / prefix, selected)
    with (root / "pyproject.toml").open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != manifest["files"]["pyproject.toml"]:
            raise ValueError("Harbor package definition changed")
    return manifest


def verify_benchmark(root: Path, manifest_path: Path):
    manifest = json.loads(manifest_path.read_text())
    if manifest['revision'] != BENCHMARK_REVISION:
        raise ValueError('Unexpected Terminal-Bench source revision')
    for task in BENCHMARK_TASKS:
        selected = {name[len(task) + 1:]: digest for name, digest in manifest['files'].items()
                    if name.startswith(task + '/')}
        if not selected:
            raise ValueError('Missing benchmark task: ' + task)
        verify_tree(root / task, selected)
    return manifest


def main():
    from harbor.environments.definition import environment_content_hash
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harbor-source", type=Path, required=True)
    parser.add_argument("--source-manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--terminal-bench-source", type=Path)
    parser.add_argument("--terminal-bench-manifest", type=Path)
    parser.add_argument("--computer-use", action="store_true")
    args = parser.parse_args()
    args.output = args.output.resolve()
    if os.geteuid() == 0:
        parser.error("Build task images as a non-root Docker-authorized user")
    if shutil.disk_usage(args.output.parent).free < 40 * 1024**3:
        parser.error("At least 40 GiB free space required before preparing images")
    manifest = verify_source(args.harbor_source, args.source_manifest)
    if bool(args.terminal_bench_source) != bool(args.terminal_bench_manifest):
        parser.error('Both Terminal-Bench source and manifest are required')
    benchmark = args.terminal_bench_source is not None
    if benchmark:
        verify_benchmark(args.terminal_bench_source, args.terminal_bench_manifest)
    if benchmark and args.computer_use:
        parser.error('Choose either Terminal-Bench or Computer Use tasks')
    task_names = COMPUTER_TASKS if args.computer_use else BENCHMARK_TASKS if benchmark else TASKS
    if args.computer_use:
        prefix = 'examples/tasks/' + COMPUTER_TASKS[0] + '/'
        selected = {name[len(prefix):]: digest for name, digest in manifest['files'].items()
                    if name.startswith(prefix)}
        if not selected:
            parser.error('Computer Use task missing from source manifest')
        verify_tree(args.harbor_source / prefix, selected)
    args.output.mkdir()
    (args.output / "tasks").mkdir()
    images = {}
    for name in task_names:
        task = args.output / "tasks" / name
        source = args.terminal_bench_source / name if benchmark else args.harbor_source / "examples/tasks" / name
        shutil.copytree(source, task)
        tag = f"agentenv-harbor/{name}:{args.output.name}"
        if benchmark:
            import tomllib
            tag = tomllib.loads((task / 'task.toml').read_text())['environment']['docker_image']
            subprocess.run(['docker', 'pull', '--platform', 'linux/amd64', tag], check=True)
        else:
            subprocess.run(["docker", "build", "--platform", "linux/amd64", "--label",
                f"agentenv.harbor.run={args.output.name}", "-t", tag, str(task / "environment")], check=True)
        config = json.loads(subprocess.check_output(["docker", "inspect", tag]))[0]
        archive = args.output / f"{name}.tar"
        subprocess.run(["docker", "save", "--output", str(archive), tag], check=True)
        with archive.open("rb") as stream:
            sha = hashlib.file_digest(stream, "sha256").hexdigest()
        images[name] = {"archive": str(archive), "archive_sha256": sha,
            "environment_name": task_environment_name(task),
            "docker_tag": tag, "docker_id": config["Id"],
            "workdir": config["Config"].get("WorkingDir", ""),
            "environment_id": environment_content_hash(task / "environment")}
    (args.output / "prepared.json").write_text(json.dumps({
        "harbor_revision": HARBOR_REVISION, "task_names": list(task_names),
        "computer_use": args.computer_use,
        "benchmark": {"revision": BENCHMARK_REVISION,
            "source_root": str(args.terminal_bench_source.resolve()),
            "source_manifest": str(args.terminal_bench_manifest.resolve())} if benchmark else None,
        "source_root": str(args.harbor_source.resolve()),
        "source_manifest": str(args.source_manifest.resolve()),
        "uid": os.geteuid(), "images": images}, indent=2))


if __name__ == "__main__":
    main()
