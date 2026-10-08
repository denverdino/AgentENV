"""Exercise real child reads/imports without allocating VM resources."""
import importlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class InputTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def probe(self, profile):
        return importlib.import_module('dragonball_m4_inputs').check(profile)

    def test_reads_parses_and_imports_without_bytecode(self):
        module = self.root / 'fixture_driver.py'
        module.write_text('from __future__ import annotations\nfrom dataclasses import dataclass\n@dataclass\nclass Driver:\n    name: str\n')
        config = self.root / 'config.toml'
        config.write_text('enabled = true\n')
        report = self.probe({'modules': [str(module)], 'toml': [str(config)]})
        self.assertTrue(report['ok'], report)
        self.assertFalse((self.root / '__pycache__').exists())

    def test_missing_unreadable_and_invalid_inputs_fail_without_contents(self):
        path = self.root / 'config.json'
        for content, mode in [('secret-invalid-json', 0o600), ('{}', 0)]:
            path.write_text(content)
            path.chmod(mode)
            try:
                report = self.probe({'json': [str(path)]})
                self.assertFalse(report['ok'], report)
                self.assertNotIn('secret-invalid-json', json.dumps(report))
            finally:
                path.chmod(0o600)
        self.assertFalse(self.probe({'files': [str(self.root / 'missing')]})['ok'])

    def test_asset_manifest_checks_referenced_files_and_lower_layers(self):
        asset = self.root / 'asset'
        asset.write_bytes(b'x')
        asset.chmod(0o755)
        image = self.root / 'image.json'
        image.write_text(json.dumps({'lowers': [{'file': str(asset)}]}))
        manifest = self.root / 'assets.json'
        manifest.write_text(json.dumps({**{k: str(asset) for k in ('runner_path', 'kernel_path', 'tools_path')},
                                        'image_path': str(image)}))
        profile = {'assets': [str(manifest)]}
        self.assertTrue(self.probe(profile)['ok'])
        asset.unlink()
        self.assertFalse(self.probe(profile)['ok'])

    def test_readable_executable_requires_execute_permission(self):
        binary = self.root / 'binary'
        binary.write_bytes(b'fixture')
        binary.chmod(0o644)
        self.assertFalse(self.probe({'executables': [str(binary)]})['ok'])
        binary.chmod(0o755)
        self.assertTrue(self.probe({'executables': [str(binary)]})['ok'])

    def test_import_failure_is_reported_without_module_output_or_secret(self):
        module = self.root / 'broken_driver.py'
        module.write_text('print("secret-print")\nraise RuntimeError("secret-error")\n')
        report = self.probe({'modules': [str(module)]})
        self.assertFalse(report['ok'])
        self.assertNotIn('secret-', json.dumps(report))

    def test_schema_refuses_unknown_relative_and_wrong_types(self):
        for profile in ({'typo': []}, {'files': 'abc'}, {'files': ['relative']}, {'files': [42]}):
            with self.subTest(profile=profile), self.assertRaises(ValueError):
                self.probe(profile)

    def test_cli_checks_expected_identity(self):
        m = importlib.import_module('dragonball_m4_inputs')
        result = subprocess.run([sys.executable, '-B', m.__file__, '--probe', '{}',
                                 '--uid', str(os.geteuid() + 1)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(json.loads(result.stdout)['ok'])


if __name__ == '__main__':
    unittest.main()
