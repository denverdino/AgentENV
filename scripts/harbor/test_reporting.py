"""Derived summaries cannot rewrite the authoritative result or hide failures."""
import json
from pathlib import Path
import tempfile
import unittest


class ReportingTests(unittest.TestCase):
    def test_report_creates_summary_without_changing_native_fields(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'write_report'), 'Report persistence does not produce an isolated summary')
        write_report = run_lab.write_report
        report = {'selected_task_names': ['task'], 'results': [],
                  'passed': False, 'error': 'original failure', 'cleanup': 'passed'}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            write_report(output, report)
            self.assertEqual(json.loads((output / 'receipt.json').read_text()), report)
            summary = json.loads((output / 'summary.json').read_text())
            self.assertEqual(summary['not_executed_task_names'], ['task'])
            self.assertFalse((output / 'summary-error.json').exists())

    def test_summary_failure_is_separate_and_does_not_change_receipt(self):
        import run_lab
        self.assertTrue(hasattr(run_lab, 'write_report'), 'Report persistence does not produce an isolated summary')
        write_report = run_lab.write_report
        report = {'results': [], 'passed': True, 'cleanup': 'passed'}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            write_report(output, report)
            self.assertEqual(json.loads((output / 'receipt.json').read_text()), report)
            self.assertEqual(json.loads((output / 'summary-error.json').read_text())['error_type'],
                             'ValueError')
            self.assertFalse((output / '.summary.json.tmp').exists())
