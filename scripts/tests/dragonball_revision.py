#!/usr/bin/env python3
"""Offline consistency check for current Dragonball pins; never invokes Cargo.

Only named production constants and positive identity fixtures are checked.
Historical rejection fixtures and documentation may intentionally use old pins.
"""
import argparse
import json
from pathlib import Path
import re
import tomllib


PATTERNS = {
    'crates/vmm-protocol/src/runtime.rs': [(r'const QUALIFIED_DRAGONBALL_REVISION: &str = "([^"]+)"', 1)],
    'src/sandbox/dragonball/client.rs': [(r'const M0_REVISION: &str = "([^"]+)"', 1)],
    'crates/dragonball-runner/tests/identity_cli.rs': [(r'identity\["dragonball_revision"\]\s*,\s*"([^"]+)"', 2)],
    'crates/dragonball-runner/tests/runner_cli.rs': [(r'identity\.dragonball_revision\s*,\s*"([^"]+)"', 1)],
    'crates/dragonball-runner/tests/runtime_cli.rs': [(r'hello\.dragonball_revision\s*,\s*"([^"]+)"', 1)],
    'scripts/tests/verify-package-dragonball.sh': [(r'"dragonball_revision":"([^"]+)"', 1),
                                                 (r'kata_revision:"([^"]+)"', 1)],
}


def check(root):
    root = Path(root)
    manifest = tomllib.loads((root / 'crates/dragonball-runner/Cargo.toml').read_text())
    dependency = manifest['target']['cfg(target_os = "linux")']['dependencies']['dragonball']
    revision = dependency.get('rev', '')
    if not re.fullmatch(r'[0-9a-f]{40}', revision):
        raise ValueError('runner Cargo.toml requires a full pinned revision')
    locked = tomllib.loads((root / 'Cargo.lock').read_text())['package']
    packages = [p for p in locked if p['name'] == 'dragonball']
    expected_source = f"git+{dependency['git']}?rev={revision}#{revision}"
    if len(packages) != 1 or packages[0].get('source') != expected_source:
        raise ValueError('Cargo.lock Dragonball source differs from runner dependency')
    if any(p.get('source', '').startswith(f"git+{dependency['git']}?")
           and p['source'] != expected_source for p in locked):
        raise ValueError('Cargo.lock contains inconsistent Kata dependency revisions')
    for name, patterns in PATTERNS.items():
        text = (root / name).read_text()
        for pattern, count in patterns:
            found = re.findall(pattern, text)
            if len(found) != count or any(value != revision for value in found):
                raise ValueError(f'{name}: current revision reference missing or inconsistent')
    return {'revision': revision, 'files_checked': len(PATTERNS) + 2}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    try:
        print(json.dumps(check(args.root)))
    except (ValueError, KeyError, OSError) as error:
        parser.exit(2, f'Dragonball revision check failed: {error}\n')
