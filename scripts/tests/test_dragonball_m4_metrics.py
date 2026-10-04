#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest


def summarize(samples, conditions):
    path = Path(__file__).with_name("dragonball_m4_metrics.py")
    if not path.exists():
        raise AssertionError("backend-neutral metrics missing")
    spec = importlib.util.spec_from_file_location("metrics", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.summarize(samples, conditions)


class MetricsTests(unittest.TestCase):
    def test_failed_samples_count_and_small_samples_do_not_qualify(self):
        rows = [{"backend": "dragonball", "operation": "restore", "ok": ok, "seconds": seconds}
                for ok, seconds in [(True, 1), (False, 9), (True, 3)]]
        report = summarize(rows, {})
        group = report["groups"]["dragonball/restore"]
        self.assertEqual(group["failure_rate"], 1 / 3)
        self.assertEqual(group["p50_seconds"], 1)
        self.assertIsNone(group["p95_seconds"])
        self.assertIsNone(group["p99_seconds"])
        self.assertFalse(report["performance_qualified"])
        self.assertIn("unfrozen_capture_and_peak_slo", report["limitations"])

    def test_kernel_mismatch_prevents_comparison(self):
        report = summarize([], {"kernel_sha256": {"firecracker": "a", "dragonball": "b"}})
        self.assertFalse(report["conditions_comparable"])
        self.assertFalse(report["performance_qualified"])

    def test_percentile_uses_successes_and_preserves_raw_samples(self):
        rows = [{"backend": "firecracker", "operation": "create", "ok": True, "seconds": x}
                for x in range(1, 101)]
        report = summarize(rows, {"kernel_sha256": {"firecracker": "same", "dragonball": "same"}})
        self.assertEqual(report["groups"]["firecracker/create"]["p99_seconds"], 99)
        self.assertEqual(report["samples"], rows)
        self.assertFalse(report["performance_qualified"], "one operation cannot qualify M4")


if __name__ == "__main__":
    unittest.main(verbosity=2)
