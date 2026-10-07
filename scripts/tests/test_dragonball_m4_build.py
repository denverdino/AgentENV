#!/usr/bin/env python3
"""Build provenance contracts, using a subprocess fixture at the Cargo boundary."""
import importlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


class BuildReceiptTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.source = self.root / 'source'
        self.source.mkdir()
        self.output = self.root / 'candidate'
        subprocess.run(['git', 'init', '-q', str(self.source)], check=True)
        (self.source / 'input').write_text('original')
        subprocess.run(['git', 'add', '.'], cwd=self.source, check=True)
        subprocess.run(['git', '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                        'commit', '-qm', 'fixture'], cwd=self.source, check=True)
        self.cargo = self.root / 'cargo'
        self.cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
if '-vV' in sys.argv:
    print('fixture toolchain v1')
    sys.exit(0)
root = pathlib.Path.cwd()
if os.getenv('FAIL_BUILD'):
    sys.exit(9)
backend = sys.argv[sys.argv.index('--features') + 1].removeprefix('hypervisor-')
revision = subprocess.check_output(['git', 'rev-parse', '--short', 'HEAD'], text=True).strip()
identity = {'backend': backend, 'firecracker': backend == 'firecracker',
            'dragonball': backend == 'dragonball', 'git_commit': revision}
if os.getenv('WRONG_HEAD'):
    identity['git_commit'] = 'unknown'
binary = pathlib.Path(os.environ['FIXTURE_BINARY'])
binary.write_text('#!' + sys.executable + '\\nprint(' + repr(json.dumps(identity)) + ')\\n')
binary.chmod(0o755)
print(json.dumps({'reason': 'compiler-artifact', 'target': {'name': 'server', 'kind': ['bin']},
                  'executable': str(binary), 'features': ['hypervisor-' + backend]}))
if os.getenv('MUTATE_SOURCE'):
    (root / 'input').write_text('changed while building')
''')
        self.cargo.chmod(0o755)
        self.env = patch.dict(os.environ, {'FIXTURE_BINARY': str(self.root / 'compiled-server'),
                                         'RUSTC': str(self.cargo)})
        self.env.start()
        self.addCleanup(self.env.stop)

    def module(self):
        self.assertTrue(Path(__file__).with_name('dragonball_m4_build.py').exists(),
                        'build-time receipt entrypoint missing')
        return importlib.import_module('dragonball_m4_build')

    def build(self, backend='dragonball'):
        return self.module().build(self.source, backend, self.output, cargo=str(self.cargo))

    def test_dirty_build_records_real_head_and_rejects_replaced_binary_or_source(self):
        (self.source / 'input').write_text('dirty candidate')
        receipt = self.build()
        m = self.module()
        path = self.output / 'receipt.json'
        frozen = self.output / 'server'
        self.assertEqual(m.verify(path, frozen, 'dragonball'), receipt)
        self.assertTrue(receipt['source']['dirty'])
        self.assertTrue(receipt['source']['revision'].startswith(receipt['identity']['git_commit']))
        self.assertEqual(receipt['build']['profile'], 'release')
        self.assertIn('--locked', receipt['build']['command'])
        original = frozen.read_bytes()
        frozen.chmod(0o755)
        frozen.write_bytes(original + b'\n')
        with self.assertRaisesRegex(ValueError, 'binary'):
            m.verify(path, frozen, 'dragonball')
        frozen.write_bytes(original)
        (self.source / 'input').write_text('next candidate')
        with self.assertRaisesRegex(ValueError, 'source'):
            m.verify(path, frozen, 'dragonball')

    def test_failed_or_mutating_build_never_publishes_receipt(self):
        for flag in ('FAIL_BUILD', 'MUTATE_SOURCE', 'WRONG_HEAD'):
            with self.subTest(flag=flag):
                self.output = self.root / flag
                with patch.dict(os.environ, {flag: '1'}):
                    with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                        self.build()
                self.assertFalse((self.output / 'receipt.json').exists())

    def test_backend_mismatch_and_output_reuse_are_rejected(self):
        self.build('firecracker')
        with self.assertRaisesRegex(ValueError, 'backend'):
            self.module().verify(self.output / 'receipt.json', self.output / 'server', 'dragonball')
        with self.assertRaises(FileExistsError):
            self.build('firecracker')

    def test_untracked_and_deleted_inputs_invalidate_receipt(self):
        self.build()
        for path, action in ((self.source / 'new-input', 'create'), (self.source / 'input', 'delete')):
            if action == 'create':
                path.write_text('new')
            else:
                path.unlink()
            with self.assertRaisesRegex(ValueError, 'source'):
                self.module().verify(self.output / 'receipt.json', self.output / 'server', 'dragonball')
            if action == 'create':
                path.unlink()

    def test_external_cargo_configuration_change_invalidates_receipt(self):
        home = self.root / "cargo-home"
        home.mkdir()
        config = home / "config.toml"
        config.write_text('[build]\njobs = 2\n')
        with patch.dict(os.environ, {"CARGO_HOME": str(home)}):
            self.build()
            config.write_text('[build]\njobs = 3\n')
            with self.assertRaisesRegex(ValueError, 'configuration'):
                self.module().verify(self.output / 'receipt.json', self.output / 'server', 'dragonball')

    def test_shared_preflight_accepts_frozen_pair_and_detects_replacement(self):
        m = self.module()
        db = self.build()
        db_output = self.output
        self.output = self.root / 'fc'
        fc = self.build('firecracker')
        performance = importlib.import_module('dragonball_m4_performance')
        asset = self.root / 'asset'
        asset.write_text('fixture')
        assets = self.root / 'assets.json'
        assets.write_text(json.dumps({'kernel_path': str(asset)}))
        inputs = {key: str(asset) for key in
                  ('daemon', 's3', 'registry', 'ublk_cli', 'workload', 'firecracker', 'kernel_fc')}
        inputs.update(server=str(db_output / 'server'), server_receipt=str(db_output / 'receipt.json'),
                      server_fc=str(self.output / 'server'), server_fc_receipt=str(self.output / 'receipt.json'),
                      assets=str(assets), deps=str(self.root))
        validate = performance.validate_inputs
        with patch.object(performance, 'validate_inputs', side_effect=lambda inputs, scenario, uid: validate(inputs, scenario, 0)):
            checked = performance.preflight(inputs)
            self.assertEqual(checked['builds'], {'dragonball': db, 'firecracker': fc})
            frozen = self.output / 'server'
            frozen.chmod(0o755)
            frozen.write_bytes(frozen.read_bytes() + b'\n')
            with self.assertRaisesRegex(ValueError, 'binary'):
                performance.preflight(inputs)

    def test_root_verification_delegates_to_nonroot_build_user(self):
        receipt = self.build()
        m = self.module()
        def command(argv, **kwargs):
            self.assertEqual(argv[:2], ['runuser', '-u'], 'root must not inspect user Git configuration')
            self.assertIn('--verify', argv)
            return json.dumps(receipt)
        with patch.object(m.os, 'geteuid', return_value=0), patch.object(m.subprocess, 'check_output', side_effect=command):
            self.assertEqual(m.verify(self.output / 'receipt.json', self.output / 'server', 'dragonball'), receipt)

    def test_dirty_candidates_with_same_head_are_not_comparable(self):
        import copy
        self.build()
        receipt = json.loads((self.output / 'receipt.json').read_text())
        other = copy.deepcopy(receipt)
        other['source']['source'] = 'different-tree'
        performance = importlib.import_module('dragonball_m4_performance')
        rows = [{'backend': b, 'identity': {'git_commit': receipt['identity']['git_commit']},
                 'build_receipt': r} for b, r in [('dragonball', receipt), ('firecracker', other)]]
        self.assertFalse(performance.comparable_builds(rows))
        rows[1]['build_receipt'] = receipt
        self.assertTrue(performance.comparable_builds(rows))
        changed = copy.deepcopy(receipt)
        changed['build']['configurations']['/external/config.toml'] = 'different'
        rows[1]['build_receipt'] = changed
        self.assertFalse(performance.comparable_builds(rows))


if __name__ == '__main__':
    unittest.main(verbosity=2)
