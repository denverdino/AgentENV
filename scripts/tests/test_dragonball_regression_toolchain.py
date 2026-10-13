"""The repository declares a fixed Rust version for local and remote checks."""
from pathlib import Path
import tomllib
import unittest


class ToolchainTests(unittest.TestCase):
    def test_repository_declares_a_fixed_toolchain(self):
        repository = Path(__file__).resolve().parents[2]
        config = tomllib.loads((repository / 'rust-toolchain.toml').read_text())['toolchain']
        self.assertRegex(config['channel'], r'^\d+\.\d+\.\d+$')
        self.assertIn('rustfmt', config['components'])
        self.assertIn('clippy', config['components'])
