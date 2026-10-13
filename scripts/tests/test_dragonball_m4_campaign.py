"""Local subprocess tests; no VM, capabilities, or external services."""
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import time
import unittest

import dragonball_m4_campaign as campaign


class CampaignTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.input = self.root / 'input'
        self.input.write_text('frozen')
        self.output = self.root / 'receipts'
        self.env = {**os.environ, 'CAMPAIGN_SECRET': 'never-print-this'}
        self.plan = {
            'source': str(self.root), 'cwd': str(self.root),
            'inputs': [str(self.input)], 'environment': ['PATH', 'CAMPAIGN_SECRET'],
            'steps': [self.step('one', "from pathlib import Path; p=Path('count'); "
                                "p.write_text(str(int(p.read_text())+1) if p.exists() else '1')")],
        }

    def step(self, name, code, timeout=5, artifacts=None):
        return {'name': name, 'argv': [sys.executable, '-c', code],
                'timeout_seconds': timeout,
                'artifacts': ['count'] if artifacts is None else artifacts}

    def run_plan(self, resume=False):
        return campaign.run(self.plan, self.output, resume=resume, env=self.env)

    def attempts(self, name='one'):
        return [json.loads(p.read_text()) for p in sorted((self.output / name).glob('*.json'))]

    def test_success_resume_and_secret_binding(self):
        self.assertEqual(self.run_plan(), 0)
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual((self.root / 'count').read_text(), '1')
        self.assertEqual(len(self.attempts()), 1)
        summary = json.loads((self.output / 'summary.json').read_text())
        self.assertEqual(summary['steps'][0]['decision'], 'reused')
        receipt = self.attempts()[0]
        self.assertEqual(receipt['status'], 'completed')
        self.assertGreaterEqual(receipt['elapsed_seconds'], 0)
        self.assertIn('disk_before', receipt)
        self.assertEqual(receipt['resource_cleanup'], 'unverified')
        for p in self.output.rglob('*.json'):
            self.assertNotIn(self.env['CAMPAIGN_SECRET'], p.read_text())
        with self.assertRaises(ValueError):
            self.run_plan()

    def test_child_does_not_consume_controller_stdin(self):
        self.plan['steps'][0] = self.step('one',
            'from pathlib import Path; import sys; Path("count").write_text(sys.stdin.read())')
        path = self.root / 'plan.json';path.write_text(json.dumps(self.plan))
        result = subprocess.run([sys.executable, campaign.__file__, 'run', '--plan', str(path),
            '--output', str(self.output)], input=b'controller input', capture_output=True, env=self.env, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'count').read_text(), '')

    def test_changed_input_plan_and_environment_rejected(self):
        self.run_plan()
        self.input.write_text('changed')
        with self.assertRaisesRegex(ValueError, 'binding'):
            self.run_plan(True)
        self.input.write_text('frozen')
        self.env['CAMPAIGN_SECRET'] = 'changed'
        with self.assertRaisesRegex(ValueError, 'binding'):
            self.run_plan(True)
        self.env['CAMPAIGN_SECRET'] = 'never-print-this'
        self.plan['steps'][0]['timeout_seconds'] = 6
        with self.assertRaisesRegex(ValueError, 'binding'):
            self.run_plan(True)
        self.assertEqual((self.root / 'count').read_text(), '1')

    def test_changed_or_missing_artifact_reruns(self):
        self.run_plan()
        (self.root / 'count').write_text('7')
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual((self.root / 'count').read_text(), '8')
        self.assertEqual(self.attempts()[-1]['reason'], 'artifact_changed')
        (self.root / 'count').unlink()
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual(self.attempts()[-1]['reason'], 'artifact_changed')

    def test_failure_reruns_and_preserves_exit(self):
        self.plan['steps'][0]['argv'][-1] += "; import sys; sys.exit(7 if p.read_text() == '1' else 0)"
        self.assertEqual(self.run_plan(), 1)
        self.assertEqual(self.attempts()[0]['exit_code'], 7)
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual(self.attempts()[-1]['reason'], 'previous_failed')

    def test_input_changes_during_failed_step_preserve_both_failures(self):
        self.plan['steps'][0] = self.step('one', "from pathlib import Path; Path('input').write_text('changed'); raise SystemExit(9)", artifacts=[])
        self.assertEqual(self.run_plan(), 1)
        attempt = self.attempts()[0]
        self.assertEqual(attempt['category'], 'exit_nonzero')
        self.assertEqual(attempt['exit_code'], 9)
        self.assertIn('input_changed', attempt['validation_errors'])

    def test_input_changes_during_success_are_failed(self):
        self.plan['steps'][0] = self.step('one', "from pathlib import Path; Path('input').write_text('changed')", artifacts=[])
        self.assertEqual(self.run_plan(), 1)
        self.assertEqual(self.attempts()[0]['category'], 'input_changed')

    def test_corrupt_step_reruns_but_corrupt_binding_rejects(self):
        self.run_plan()
        next((self.output / 'one').glob('*.json')).write_text('{broken')
        self.assertEqual(self.run_plan(True), 0)
        latest = sorted((self.output / 'one').glob('*.json'))[-1]
        self.assertEqual(json.loads(latest.read_text())['reason'], 'corrupt_receipt')
        (self.output / 'campaign.json').write_text('{broken')
        with self.assertRaises(ValueError):
            self.run_plan(True)

    def test_interrupted_attempt_and_missing_receipt_rerun(self):
        self.run_plan()
        p = next((self.output / 'one').glob('*.json'))
        value = json.loads(p.read_text()); value['status'] = 'attempted'
        p.write_text(json.dumps(value))
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual(self.attempts()[-1]['reason'], 'previous_attempted')
        for p in (self.output / 'one').glob('*.json'):
            p.unlink()
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual((self.root / 'count').read_text(), '3')

    def test_timeout_kills_descendant_even_when_parent_exits(self):
        child = "import signal,time; from pathlib import Path; signal.signal(signal.SIGTERM, signal.SIG_IGN); Path('child-ready').touch(); time.sleep(2); Path('escaped').touch()"
        parent = ("import subprocess,time; from pathlib import Path; "
                  f"p=subprocess.Popen([{sys.executable!r}, '-c', {child!r}]); "
                  "Path('child-pid').write_text(str(p.pid)); time.sleep(30)")
        self.plan['steps'][0] = self.step('one', parent, timeout=0.5, artifacts=[])
        start = time.monotonic()
        self.assertEqual(self.run_plan(), 1)
        self.assertLess(time.monotonic() - start, 4)
        self.assertEqual(self.attempts()[0]['status'], 'timed_out')
        self.assertTrue((self.root / 'child-ready').exists())
        time.sleep(2)
        self.assertFalse((self.root / 'escaped').exists())

    def test_prepare_does_not_run_commands(self):
        campaign.prepare(self.plan, self.output, env=self.env)
        self.assertFalse((self.root / 'count').exists())
        self.assertEqual(self.run_plan(), 0)

    def test_cli_plan_run_resume_and_missing_artifact(self):
        path = self.root / 'plan.json'
        path.write_text(json.dumps(self.plan))
        base = [sys.executable, str(Path(campaign.__file__).resolve())]
        options = ['--plan', str(path), '--output', str(self.output)]
        for action in [('plan',), ('run',), ('run', '--resume')]:
            result = subprocess.run([*base, *action, *options], env=self.env,
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'count').read_text(), '1')
        self.plan['steps'][0]['artifacts'] = ['never-created']
        self.assertEqual(campaign.run(self.plan, self.root / 'other', env=self.env), 1)
        receipt = json.loads(next((self.root / 'other' / 'one').glob('*.json')).read_text())
        self.assertEqual(receipt['category'], 'artifact_missing_or_invalid')

    def test_missing_completion_fields_are_not_reused(self):
        self.run_plan()
        path = next((self.output / 'one').glob('*.json'))
        receipt = json.loads(path.read_text())
        del receipt['ended_utc']
        path.write_text(json.dumps(receipt))
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual((self.root / 'count').read_text(), '2')
        self.assertEqual(self.attempts()[-1]['reason'], 'corrupt_receipt')

    def test_changed_log_and_missing_executable(self):
        self.run_plan()
        next((self.output / 'one').glob('*.log')).write_text('altered')
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual(self.attempts()[-1]['reason'], 'log_changed')
        self.plan['steps'][0]['argv'] = [str(self.root / 'absent-executable')]
        self.assertEqual(campaign.run(self.plan, self.root / 'spawn', env=self.env), 1)
        receipt = json.loads(next((self.root / 'spawn' / 'one').glob('*.json')).read_text())
        self.assertEqual(receipt['category'], 'execution_error')
        self.assertEqual(receipt['error_type'], 'FileNotFoundError')

    def test_argv_is_not_shell_and_preserves_empty_argument(self):
        code = "import sys; from pathlib import Path; Path('count').write_text(repr(sys.argv[1:]))"
        self.plan['steps'][0]['argv'] = [sys.executable, '-c', code, '', '$(touch injected)']
        self.assertEqual(self.run_plan(), 0)
        self.assertEqual((self.root / 'count').read_text(), "['', '$(touch injected)']")
        self.assertFalse((self.root / 'injected').exists())

    def test_reject_invalid_steps_and_symlinks(self):
        variants = [[], [self.plan['steps'][0]] * 2,
                    [self.step('../escape', 'pass')], [self.step('one', 'pass', timeout=0)],
                    [self.step('one', 'pass', timeout=float('nan'))]]
        for steps in variants:
            with self.subTest(steps=steps), self.assertRaises(ValueError):
                campaign.validate({**self.plan, 'steps': steps})
        link = self.root / 'link'; link.symlink_to(self.input)
        for path in [link, self.root]:
            with self.subTest(path=path), self.assertRaises(ValueError):
                campaign.binding({**self.plan, 'inputs': [str(path)]}, self.env)

    def test_downstream_reruns_after_upstream_evidence_invalidated(self):
        self.plan['steps'].append(self.step('two', "from pathlib import Path; Path('two').write_text(Path('count').read_text())", artifacts=['two']))
        self.run_plan()
        (self.root / 'count').write_text('8')
        self.assertEqual(self.run_plan(True), 0)
        self.assertEqual((self.root / 'two').read_text(), '9')
        self.assertEqual(self.attempts('two')[-1]['reason'], 'upstream_rerun')


if __name__ == '__main__':
    unittest.main()
