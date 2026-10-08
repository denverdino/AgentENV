#!/usr/bin/env python3
"""Build and freeze a single-backend server with a local provenance receipt.

No receipt can be manufactured from an existing executable. Keep the source
checkout available and unchanged until tests complete. This is an accidental
staleness guard, not a signed or hermetic supply-chain attestation.
"""
import argparse
import hashlib
import json
import os
import pwd
import sys
import signal
from pathlib import Path
import shutil
import subprocess

from dragonball_m4_preflight import check
from dragonball_test_resources import RunRecord
from dragonball_m4_host import host_lock, lock_fds, require_clean_host
from dragonball_regression import digest, file_digest, source_identity


def source_inputs(root):
    identity = source_identity(root)
    # Includes staged changes, binary diffs, and file modes; untracked content is
    # covered by source_identity even though it is absent from git diff.
    patch = subprocess.check_output(['git', 'diff', '--binary', 'HEAD', '--'], cwd=root)
    status = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=normal',
                                      '--', '.', ':!:**/__pycache__/**'], cwd=root)
    return {**identity, 'patch_sha256': hashlib.sha256(patch).hexdigest(), 'dirty': bool(status)}


def build_environment():
    # Only a digest is persisted: build environment may contain credentials.
    return digest({k: v for k, v in os.environ.items()
                   if k.startswith(('CARGO_', 'RUST', 'CC_', 'CXX_', 'CFLAGS', 'CXXFLAGS', 'LDFLAGS'))
                   or k in ('PATH', 'CC', 'CXX', 'AR', 'PKG_CONFIG_PATH', 'PROTOC')})


def cargo_configurations(root):
    directories = [root, *root.parents]
    paths = {directory / '.cargo' / name for directory in directories
             for name in ('config', 'config.toml')}
    home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')).resolve()
    paths.update(home / name for name in ('config', 'config.toml'))
    return {str(path): file_digest(path) if path.exists() else None for path in sorted(paths)}


def toolchains(cargo, root):
    return {name: subprocess.check_output([program, '-vV'], cwd=root, text=True).strip()
            for name, program in (('cargo', cargo), ('rustc', os.environ.get('RUSTC', 'rustc')))}


def check_identity(identity, backend, revision):
    commit = identity.get('git_commit', '')
    if (identity.get('backend') != backend or identity.get(backend) is not True or
            identity.get('firecracker' if backend == 'dragonball' else 'dragonball') is not False):
        raise ValueError('backend identity does not match requested build')
    if not isinstance(commit, str) or len(commit) < 7 or not revision.startswith(commit):
        raise ValueError('binary embedded revision differs from source HEAD')


def build(root, backend, output, profile='release', cargo='cargo', minimum_gib=20):
    with host_lock():
        require_clean_host()
        return _build(root, backend, output, profile, cargo, minimum_gib)


def _build(root, backend, output, profile='release', cargo='cargo', minimum_gib=20):
    root, output = Path(root).resolve(), Path(output).resolve()
    if os.geteuid() == 0:
        raise ValueError('build must run as a non-root user')
    if backend not in ('firecracker', 'dragonball') or profile not in ('debug', 'release'):
        raise ValueError('unsupported backend or profile')
    if output == root or root in output.parents:
        raise ValueError('candidate output must be outside the source checkout')
    if minimum_gib < 1:
        raise ValueError('disk budget must be positive')
    admission = check({'mode': 'build', 'run_user': pwd.getpwuid(os.geteuid()).pw_name,
                       'disk_path': str(output.parent), 'reserve_bytes': minimum_gib * 1024**3,
                       'planned_bytes': 0, 'retained_bytes': 0,
                       'required_tools': [cargo, os.environ.get('RUSTC', 'rustc')]})
    if not admission['ok']:
        raise ValueError('build preflight refused: ' + '; '.join(admission['errors']))
    before = source_inputs(root)
    versions, environment = toolchains(cargo, root), build_environment()
    configurations = cargo_configurations(root)
    output.mkdir(parents=True, exist_ok=False)
    with RunRecord(output, 'm4-build', owned=[output], inputs=before) as record:
        (output / 'preflight.json').write_text(json.dumps(admission, indent=2) + '\n')
        command = [cargo, 'build', '--locked', '-p', 'agentenv', '--bin', 'server',
                   '--no-default-features', '--features', 'hypervisor-' + backend, '--message-format=json']
        if profile == 'release':
            command.append('--release')
        log = output / 'cargo.jsonl'
        with log.open('w') as stdout, (output / 'cargo.stderr.log').open('w') as stderr:
            subprocess.run(command, cwd=root, stdout=stdout, stderr=stderr, check=True, pass_fds=lock_fds())
        artifacts = []
        for line in log.read_text().splitlines():
            row = json.loads(line)
            if (row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'server'
                    and row.get('target', {}).get('kind') == ['bin'] and row.get('executable')):
                artifacts.append(row)
        if len(artifacts) != 1:
            raise ValueError('expected exactly one Cargo server artifact')
        artifact = artifacts[0]
        binary = Path(artifact['executable'])
        original = file_digest(binary)
        frozen = output / 'server'
        shutil.copy2(binary, frozen)
        identity = json.loads(subprocess.check_output([str(frozen), '--component-info-json'], text=True, timeout=30))
        check_identity(identity, backend, before['revision'])
        if (source_inputs(root) != before or toolchains(cargo, root) != versions
                or build_environment() != environment or cargo_configurations(root) != configurations):
            raise ValueError('source or build inputs changed during build')
        if file_digest(binary) != original or file_digest(frozen) != original:
            raise ValueError('binary changed while freezing candidate')
        frozen.chmod(0o555)
        receipt = {'schema': 1, 'build_uid': os.geteuid(), 'backend': backend, 'source_root': str(root), 'source': before,
                   'binary_sha256': original, 'identity': identity,
                   'build': {'command': command, 'profile': profile, 'features': artifact.get('features', []),
                             'toolchains': versions, 'environment_sha256': environment,
                             'configurations': configurations},
                   'cargo_log_sha256': file_digest(log)}
        temporary = output / 'receipt.tmp'
        temporary.write_text(json.dumps(receipt, indent=2) + '\n')
        temporary.replace(output / 'receipt.json')
        return receipt


def verify(path, binary, backend):
    path, binary = Path(path), Path(binary)
    if not path.is_absolute() or not path.is_file():
        raise ValueError('missing absolute build receipt: ' + str(path))
    receipt = json.loads(path.read_text())
    if receipt.get('schema') != 1 or receipt.get('backend') != backend:
        raise ValueError('receipt schema or backend mismatch')
    if receipt.get('binary_sha256') != file_digest(binary):
        raise ValueError('binary differs from build receipt')
    if os.geteuid() == 0:
        uid = receipt.get('build_uid')
        if type(uid) is not int or uid <= 0:
            raise ValueError('receipt requires a non-root build identity')
        user = pwd.getpwuid(uid).pw_name
        # Keep Git ownership checks and repository configuration under the
        # build account. Never add a global safe.directory exception for root.
        return json.loads(subprocess.check_output(
            ['runuser', '-u', user, '--', sys.executable, str(Path(__file__).resolve()),
             '--verify', str(path), '--binary', str(binary), '--backend', backend],
            text=True, timeout=60))
    root = Path(receipt['source_root'])
    if not root.is_absolute() or source_inputs(root) != receipt['source']:
        raise ValueError('source differs from build receipt')
    for config, expected in receipt['build']['configurations'].items():
        actual = file_digest(config) if Path(config).exists() else None
        if actual != expected:
            raise ValueError('Cargo configuration differs from build receipt: ' + config)
    check_identity(receipt['identity'], backend, receipt['source']['revision'])
    if file_digest(path.parent / 'cargo.jsonl') != receipt['cargo_log_sha256']:
        raise ValueError('Cargo evidence differs from build receipt')
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument('--backend', choices=('firecracker', 'dragonball'), required=True)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--verify', type=Path, help='verify an existing receipt; never build')
    parser.add_argument('--binary', type=Path, help='candidate executable to verify')
    parser.add_argument('--profile', choices=('debug', 'release'), default='release')
    parser.add_argument('--cargo', default='cargo')
    parser.add_argument('--min-free-gib', type=int, default=20)
    args = parser.parse_args()
    if args.verify:
        if not args.binary:
            parser.error('--verify requires --binary')
        print(json.dumps(verify(args.verify, args.binary, args.backend)))
        return
    if not args.output:
        parser.error('building requires --output')
    build(args.source, args.backend, args.output, args.profile, args.cargo, args.min_free_gib)
    print(args.output.resolve() / 'receipt.json')


if __name__ == '__main__':
    from dragonball_regression import interrupt
    signal.signal(signal.SIGTERM, interrupt)
    main()
