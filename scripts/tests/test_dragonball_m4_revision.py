"""Offline pin checks must fail before any Cargo/network work."""
import importlib
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
FILES = ['crates/dragonball-runner/Cargo.toml', 'Cargo.lock',
         'crates/vmm-protocol/src/runtime.rs', 'src/sandbox/dragonball/client.rs',
         'crates/dragonball-runner/tests/identity_cli.rs', 'crates/dragonball-runner/tests/runner_cli.rs',
         'crates/dragonball-runner/tests/runtime_cli.rs', 'scripts/tests/verify-package-dragonball.sh']


class RevisionTests(unittest.TestCase):
    def test_current_tree_is_consistent(self):
        result = importlib.import_module('dragonball_revision').check(ROOT)
        self.assertEqual(result['files_checked'], len(FILES))

    def test_each_stale_positive_reference_is_rejected(self):
        m = importlib.import_module('dragonball_revision')
        revision = m.check(ROOT)['revision']
        for name in FILES:
            with self.subTest(file=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for path in FILES:
                    target = root / path
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(ROOT / path, target)
                target = root / name
                target.write_text(target.read_text().replace(revision, '0' * 40, 1))
                with self.assertRaises(ValueError):
                    m.check(root)

    def test_negative_fixture_and_comments_are_not_current_pins(self):
        m = importlib.import_module('dragonball_revision')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in FILES:
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / path, target)
            with (root / FILES[2]).open('a') as out:
                out.write('\n// previous revision: ' + '0' * 40 + '\nconst BAD_REV: &str = "' + '1' * 40 + '";\n')
            self.assertEqual(m.check(root), m.check(ROOT))

    def test_make_refuses_stale_pin_before_invoking_cargo(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in [*FILES, 'Makefile', 'scripts/tests/dragonball_revision.py']:
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / path, target)
            cargo = root / 'fake-cargo'
            cargo.write_text('#!/bin/sh\nprintf called >> cargo-invoked\n')
            cargo.chmod(0o755)
            pin = root / 'crates/vmm-protocol/src/runtime.rs'
            original = pin.read_text()
            revision = importlib.import_module('dragonball_revision').check(ROOT)['revision']
            pin.write_text(original.replace(revision, '0' * 40))
            argv = ['make', 'build-server-dragonball', f'CARGO={cargo}', f'REGRESSION_PYTHON={sys.executable}']
            rejected = subprocess.run(argv, cwd=root, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn('revision check failed', rejected.stderr)
            self.assertFalse((root / 'cargo-invoked').exists())
            pin.write_text(original)
            subprocess.run(argv, cwd=root, capture_output=True, text=True, check=True)
            self.assertTrue((root / 'cargo-invoked').exists())


if __name__ == '__main__':
    unittest.main()
