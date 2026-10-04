#!/usr/bin/env python3
"""Make entrypoints refuse missing explicit lab inputs before allocating state."""
from pathlib import Path
import subprocess
import tempfile
import unittest


class EntryTests(unittest.TestCase):
    def invoke(self, target, *settings):
        return subprocess.run(
            ["make", target, *settings], cwd=Path(__file__).resolve().parents[2],
            text=True, capture_output=True, timeout=10,
        )

    def test_physical_requires_explicit_host_inputs(self):
        result = self.invoke("test-dragonball-m4-physical", "AENV_M4_HOSTS=", "AENV_M4_OUTPUT=")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("AENV_M4_HOSTS must name", result.stderr)

    def test_physical_requires_explicit_evidence_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            hosts = Path(directory) / "hosts.json"
            hosts.write_text("[]")
            result = self.invoke("test-dragonball-m4-physical", f"AENV_M4_HOSTS={hosts}", "AENV_M4_OUTPUT=")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("AENV_M4_OUTPUT must name", result.stderr)
            self.assertEqual(sorted(p.name for p in Path(directory).iterdir()), ["hosts.json"])

    def test_performance_requires_explicit_asset_inputs(self):
        result = self.invoke("test-dragonball-m4-performance", "AENV_M4_INPUTS=")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("AENV_M4_INPUTS must name", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
