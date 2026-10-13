#!/usr/bin/env python3
"""Freeze a reviewed Harbor candidate without private runtime inputs."""
import argparse
import ast
import hashlib
import json
from pathlib import Path
import shutil
import tarfile

from check_env_files import forbidden
from artifact_delivery import publish_verified


def freeze(baseline, repository, output, *, controls=()):
    baseline, repository, output = map(Path, (baseline, repository, output))
    controls = [Path(path) for path in controls]
    if (forbidden([path.name for path in controls]) or
            len({path.name for path in controls}) != len(controls) or
            any(path.is_symlink() or not path.is_file() or path.suffix not in ('.py', '.sh') for path in controls)):
        raise ValueError('Control inputs must be unique regular Python or shell scripts')
    for path in controls:
        if path.suffix == '.py':ast.parse(path.read_bytes(), filename=path.name)
    paths = [p for p in baseline.rglob('*') if p.is_file() or p.is_symlink()]
    if any(p.is_symlink() for p in paths) or forbidden([p.name for p in paths]):
        raise ValueError('Candidate baseline contains private env files or symlinks')
    current = {path.name for path in (repository / 'scripts/harbor').glob('*.py')}
    obsolete = sorted(path.name for path in (baseline / 'scripts/harbor').glob('*.py')
                      if path.name not in current)
    if obsolete:
        raise ValueError('Candidate baseline contains deleted Harbor modules: ' + ', '.join(obsolete))
    if output.with_suffix('.tar.gz').exists():
        raise FileExistsError('Candidate archive already exists')
    output.mkdir()
    source = output / 'source'
    shutil.copytree(baseline, source)
    for path in (repository / 'scripts/harbor').glob('*.py'):
        if path.is_symlink():
            raise ValueError('Candidate source must not use symlinks')
        target = source / 'scripts/harbor' / path.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
    names = ['scripts/tests/dragonball_m4_scenarios.py', 'scripts/tests/dragonball_m4_lab.py',
             'scripts/run-with-capabilities.sh', 'scripts/harbor/.env.example',
             'scripts/harbor/OSS.md', 'scripts/harbor/README.md',
             'scripts/harbor/examples/cached-archives.example.json', 'scripts/harbor/examples/host-inputs.example.json', 'scripts/harbor/requirements-test.txt', '.gitignore', 'Makefile',
             '.github/workflows/regression-tools.yml']
    names += ['tools-image/init', 'tools-image/pivot-init', 'tools-image/Dockerfile',
              'tools-image/Makefile', 'tools-image/README.md',
              'tools-image/patches/envd-immediate-mmds.patch',
              'scripts/harbor/material-inputs.example.json']
    names += ['scripts/tests/dragonball_m4_campaign.py', 'rust-toolchain.toml',
              'scripts/tests/test_dragonball_m4_campaign.py',
              'scripts/tests/test_dragonball_regression_environment.py',
              'scripts/tests/test_dragonball_regression_toolchain.py']
    for name in names:
        if (repository / name).is_symlink():
            raise ValueError('Candidate source must not use symlinks')
        target = source / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(repository / name, target)
    for path in controls:
        target = source / 'harbor-control' / path.name
        target.parent.mkdir(exist_ok=True)
        shutil.copy2(path, target)
    manifest = {p.relative_to(source).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in sorted(source.rglob('*')) if p.is_file()}
    (output / 'driver-sha256.json').write_text(json.dumps(manifest, indent=2))
    partial = output.with_suffix('.tar.gz.part')
    with tarfile.open(partial, 'x:gz') as archive:
        archive.add(source, arcname='source')
        archive.add(output / 'driver-sha256.json', arcname='driver-sha256.json')
    publish_verified(partial, output.with_suffix('.tar.gz'),
                     hashlib.sha256(partial.read_bytes()).hexdigest(), partial.stat().st_size)
    partial.unlink()
    return manifest


def verify_candidate(candidate):
    candidate = Path(candidate)
    manifest = json.loads((candidate / 'driver-sha256.json').read_text())
    source = candidate / 'source'
    paths = [path for path in source.rglob('*') if path.is_file() or path.is_symlink()]
    if (any(path.is_symlink() for path in paths) or
            forbidden([path.name for path in paths]) or
            {path.relative_to(source).as_posix() for path in paths} != set(manifest)):
        raise ValueError('Frozen candidate file set changed')
    for path in paths:
        if hashlib.sha256(path.read_bytes()).hexdigest() != manifest[path.relative_to(source).as_posix()]:
            raise ValueError('Frozen candidate file digest changed')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, help='Previously frozen source directory')
    parser.add_argument('--output', type=Path, help='New candidate directory; existing names are refused')
    parser.add_argument('--control-input', type=Path, action='append', default=[])
    parser.add_argument('--verify', type=Path, help='Verify an unpacked candidate including control scripts')
    args = parser.parse_args()
    if args.verify:
        if args.baseline or args.output or args.control_input:parser.error('--verify cannot be combined with freeze inputs')
        manifest = verify_candidate(args.verify)
        print(json.dumps({'verified': True, 'files': len(manifest)}))
        return
    if not args.baseline or not args.output:parser.error('--baseline and --output are required for freezing')
    manifest = freeze(args.baseline, Path(__file__).resolve().parents[2], args.output, controls=args.control_input)
    print(json.dumps({'files': len(manifest), 'manifest': str(args.output / 'driver-sha256.json')}))


if __name__ == '__main__':
    main()
