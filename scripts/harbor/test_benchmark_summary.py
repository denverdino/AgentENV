"""Read-only benchmark summaries preserve denominators and measured boundaries."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name('benchmark_summary.py')


def native(identity='native-1', task='long', reward=0):
    return {'id': identity, 'task_name': task, 'agent_info': {'name': 'fixture'},
        'started_at': '2026-10-11T00:00:00Z', 'finished_at': '2026-10-11T00:00:10Z',
        'environment_setup': {'started_at': '2026-10-11T00:00:00Z', 'finished_at': '2026-10-11T00:00:01Z'},
        'agent_setup': {'started_at': '2026-10-11T00:00:01Z', 'finished_at': '2026-10-11T00:00:03Z'},
        'agent_execution': {'started_at': '2026-10-11T00:00:03Z', 'finished_at': '2026-10-11T00:00:09Z'},
        'verifier': {'started_at': '2026-10-11T00:00:09Z', 'finished_at': '2026-10-11T00:00:10Z'},
        'agent_result': {'n_input_tokens': 100, 'n_cache_tokens': 40, 'n_output_tokens': 20, 'cost_usd': .25},
        'verifier_result': {'rewards': {'reward': reward}},
        'exception_info': {'exception_type': 'AgentTimeoutError',
                           'exception_message': 'do not export secret-message',
                           'exception_traceback': 'do not export secret-trajectory'}}


class BenchmarkSummaryTests(unittest.TestCase):
    def run_summary(self, root, report, *, success=True):
        report_path = root / 'report.json'
        report_path.write_text(json.dumps(report))
        output = root / 'summary.json'
        before = report_path.read_bytes()
        result = subprocess.run([sys.executable, '-B', str(SCRIPT), '--report', str(report_path),
                                 '--output', str(output)], capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0 if success else 1, result.stderr)
        self.assertEqual(report_path.read_bytes(), before, 'Summary modified its original report')
        return json.loads(output.read_text()) if success else result

    def test_mixed_trial_slot_integral_uses_scheduler_window_and_separates_vmm_peak(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); (root / 'trials').mkdir()
            events = [
                {'event': 'admitted', 'time': 10, 'active_slots': 1},
                {'event': 'admitted', 'time': 10, 'active_slots': 2},
                {'event': 'imported', 'time': 11, 'active_slots': 0},
                {'event': 'cleaned', 'time': 12, 'active_slots': 2},
                {'event': 'finished', 'time': 12, 'active_slots': 1},
                {'event': 'admitted', 'time': 12, 'active_slots': 2},
                {'event': 'finished', 'time': 14, 'active_slots': 1},
                {'event': 'finished', 'time': 20, 'active_slots': 0}]
            (root / 'trials/events.jsonl').write_text(''.join(json.dumps(row) + '\n' for row in events))
            (root / 'campaign-monitor.json').write_text(json.dumps({'max_live_vmm_count': 1}))
            report = {'selected_task_names': ['long', 'short', 'refill'], 'results': [],
                      'resource_plan': {'concurrency': 2}, 'started': 0, 'finished': 100}
            summary = self.run_summary(root, report)
            slots = summary['trial_slots']
            self.assertEqual(slots['window_started'], 10)
            self.assertEqual(slots['window_finished'], 20)
            self.assertEqual(slots['window_seconds'], 10)
            self.assertEqual(slots['occupied_slot_seconds'], 14)
            self.assertAlmostEqual(slots['utilization_fraction'], .7)
            self.assertEqual(summary['measured_vmm_peak'], 1)
            self.assertEqual(summary['report_wall_seconds'], 100)
            self.assertEqual(summary['unmeasured_phase_seconds'],
                             {'preparation': None, 'restore': None, 'install': None, 'cleanup': None})

    def test_blocked_and_unexecuted_tasks_remain_in_fixed_selected_denominator(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = {'selected_task_names': ['long', 'blocked', 'pending'],
                      'prepared': {'task_names': ['long']}, 'results': [native()],
                      'material_gate': {'blocked_task_names': ['blocked']}}
            summary = self.run_summary(root, report)
            self.assertEqual(summary['selected_task_names'], ['long', 'blocked', 'pending'])
            self.assertEqual(summary['coverage'], {'selected_tasks': 3, 'executed_tasks': 1,
                'material_blocked_tasks': 1, 'not_executed_tasks': 2, 'other_not_executed_tasks': 1,
                'executed_task_fraction': 1 / 3})
            self.assertEqual(summary['not_executed_task_names'], ['blocked', 'pending'])
            self.assertEqual(summary['trials'][0]['reward'], 0)
            self.assertEqual(summary['trials'][0]['exception_type'], 'AgentTimeoutError')

    def test_native_phases_and_usage_export_only_known_scalar_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            row = native(); row['agent_result']['metadata'] = {'OPENAI_API_KEY': 'metadata-secret'}
            report = {'selected_task_names': ['long'], 'results': [row],
                'driver_sha256': {'benchmark_summary.py': 'd' * 64},
                'effective_profile_sha256': 'a' * 64, 'runtime_sha256': {'tools': 'b' * 64},
                'credentials': {'token': 'report-secret'}}
            summary = self.run_summary(root, report)
            trial = summary['trials'][0]
            self.assertEqual(trial['phase_seconds'],
                             {'environment_setup': 1, 'agent_setup': 2, 'agent_execution': 6, 'verifier': 1})
            self.assertEqual(trial['token_usage'],
                             {'n_input_tokens': 100, 'n_cache_tokens': 40, 'n_output_tokens': 20, 'cost_usd': .25})
            self.assertTrue(summary['input_tokens_include_cache'])
            self.assertEqual(summary['binding']['effective_profile_sha256'], 'a' * 64)
            text = json.dumps(summary)
            for secret in ('secret-message', 'secret-trajectory', 'metadata-secret', 'report-secret'):
                self.assertNotIn(secret, text)

    def test_missing_or_unfinished_phases_and_usage_stay_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            row = native()
            row.update(environment_setup=None, agent_setup={}, verifier={'started_at': '2026-10-11T00:00:09Z'},
                       agent_result=None)
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': [row]})
            self.assertEqual(summary['trials'][0]['phase_seconds'],
                             {'environment_setup': None, 'agent_setup': None, 'agent_execution': 6, 'verifier': None})
            self.assertIsNone(summary['phase_totals']['environment_setup']['known_seconds'])
            self.assertEqual(summary['phase_totals']['agent_execution']['known_seconds'], 6)
            self.assertEqual(summary['usage_totals']['cost_usd'],
                             {'known_total': None, 'known_trials': 0, 'missing_trials': 1})
            self.assertIsNone(summary['trial_slots']['occupied_slot_seconds'])
            self.assertIsNone(summary['measured_vmm_peak'])

    def test_zero_framework_cost_is_preserved_without_claiming_a_zero_bill(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); row = native(); row['agent_result']['cost_usd'] = 0
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': [row]})
            self.assertEqual(summary['trials'][0]['token_usage']['cost_usd'], 0)
            self.assertEqual(summary.get('cost_usd_basis'), 'native_framework_cost')
            self.assertIsNone(summary['billing_cost_usd'])

    def test_cache_binding_does_not_claim_consumption_and_actual_template_ids_are_distinct(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            receipt = {'sandbox_id': 'template-child', 'start_mode': 'template',
                       'initial_workspace_sha256': 'a' * 64}
            report = {'selected_task_names': ['long'], 'results': [],
                'dependencies': {'binding_sha256': 'a' * 64}, 'downloads': {'binding_sha256': 'b' * 64},
                'sandbox_receipts': [receipt, dict(receipt)], 'shared_templates': {'mode': 'reuse'}}
            summary = self.run_summary(root, report)
            self.assertIn('cache_evidence', summary)
            for kind in ('dependencies', 'downloads'):
                self.assertEqual(summary['cache_evidence'][kind],
                                 {'prepared': None, 'bound': True, 'consumed': None})
            templates = summary['cache_evidence']['templates']
            self.assertEqual(templates['actual_distinct_launches'], 1)
            self.assertEqual(templates['initial_workspace_verified_launches'], 1)
            self.assertTrue(templates['consumed'])
            self.assertIsNone(templates['prepared'])

    def test_preflight_events_do_not_expand_the_trial_measurement_window(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); (root / 'trials').mkdir()
            events = [{'event': 'admitted', 'time': 1, 'active_slots': 1, 'id': 'preflight-long'},
                      {'event': 'finished', 'time': 4, 'active_slots': 0, 'id': 'preflight-long'},
                      {'event': 'admitted', 'time': 10, 'active_slots': 1, 'id': '00000-long'},
                      {'event': 'finished', 'time': 20, 'active_slots': 0, 'id': '00000-long'}]
            (root / 'trials/events.jsonl').write_text(''.join(json.dumps(row) + '\n' for row in events))
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': [],
                                             'resource_plan': {'concurrency': 2}})
            self.assertEqual(summary['trial_slots']['window_seconds'], 10)
            self.assertEqual(summary['trial_slots']['occupied_slot_seconds'], 10)
            self.assertEqual(summary['trial_slots']['utilization_fraction'], .5)

    def test_single_slot_sample_leaves_interval_integral_and_utilization_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'continuous-events.jsonl').write_text(json.dumps(
                {'event': 'admitted', 'time': 10, 'active_slots': 1}) + '\n')
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': []})
            self.assertIsNone(summary['trial_slots']['window_seconds'])
            self.assertIsNone(summary['trial_slots']['occupied_slot_seconds'])
            self.assertIsNone(summary['trial_slots']['utilization_fraction'])

    def test_model_material_block_with_successful_oracle_does_not_count_as_model_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            model = native(); model['agent_info']['name'] = 'terminus-2'
            oracle = native(identity='oracle-blocked', task='blocked', reward=1)
            oracle.update(agent_info={'name': 'oracle'}, exception_info=None)
            report = {'selected_task_names': ['long', 'blocked', 'pending'], 'results': [model, oracle],
                'material_gate': {'blocked_task_names': ['blocked']},
                'effective_profile': {'agents': [{'import_path': 'dashscope_agent:DashScopeTerminus2'}]}}
            summary = self.run_summary(root, report)
            self.assertEqual(summary['coverage']['executed_tasks'], 1)
            self.assertEqual(summary['coverage']['not_executed_tasks'], 2)
            self.assertEqual(summary.get('coverage_scope'), 'model_trials')
            self.assertEqual(summary.get('evaluation_status_counts'),
                             {'passed': 0, 'normal_zero_reward': 0, 'exception': 1, 'unknown': 0})
            self.assertEqual(summary.get('control_status_counts'),
                             {'passed': 1, 'normal_zero_reward': 0, 'exception': 0, 'unknown': 0})

    def test_trial_status_counts_are_disjoint_and_exception_takes_priority_over_zero_reward(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [native(identity='passed', reward=1), native(identity='normal-zero'),
                    native(identity='exception-zero'), native(identity='unknown')]
            rows[0]['exception_info'] = rows[1]['exception_info'] = rows[3]['exception_info'] = None
            rows[3]['verifier_result'] = None
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': rows})
            self.assertEqual(summary.get('trial_status_counts'),
                             {'passed': 1, 'normal_zero_reward': 1, 'exception': 1, 'unknown': 1})

    def test_metadata_profile_binding_is_whitelisted_and_conflicts_are_rejected(self):
        from benchmark_summary import summarize
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); report = root / 'report.json'
            report.write_text(json.dumps({'selected_task_names': ['long'], 'results': [],
                                          'effective_profile_sha256': 'a' * 64}))
            summary = summarize(report, metadata={'candidate_profile_sha256': 'b' * 64,
                                                   'OPENAI_API_KEY': 'metadata-secret'})
            self.assertEqual(summary['binding']['candidate_profile_sha256'], 'b' * 64)
            self.assertNotIn('metadata-secret', json.dumps(summary))
            with self.assertRaises(ValueError):
                summarize(report, metadata={'effective_profile_sha256': 'b' * 64})

    def test_duplicate_native_id_with_equal_content_is_counted_once_and_tracks_source_sha(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); row = native()
            source = root / 'trials/first/jobs/job/trial/result.json'
            duplicate = root / 'jobs/legacy/trial/result.json'
            for path in (source, duplicate):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(row, indent=2 if path == source else 4))
            original = source.read_bytes()
            summary = self.run_summary(root, {'selected_task_names': ['long'], 'results': [row]})
            self.assertEqual(len(summary['trials']), 1)
            self.assertEqual(summary['usage_totals']['n_input_tokens']['known_total'], 100)
            sources = summary['trials'][0]['sources']
            self.assertEqual(len(sources), 3)
            native_sources = [item for item in sources if item['kind'] == 'native_file']
            self.assertIn(hashlib.sha256(original).hexdigest(), [item['sha256'] for item in native_sources])
            self.assertEqual(source.read_bytes(), original)

    def test_duplicate_native_id_with_conflicting_content_is_rejected_without_raw_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); row = native()
            altered = copy.deepcopy(row); altered['verifier_result']['rewards']['reward'] = 1
            result = self.run_summary(root, {'selected_task_names': ['long'], 'results': [row, altered]}, success=False)
            self.assertNotIn('secret-message', result.stderr)
            self.assertFalse((root / 'summary.json').exists())

    def test_unknown_native_task_is_rejected_instead_of_expanding_selected_denominator(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.run_summary(root, {'selected_task_names': ['long'], 'results': [native(task='other')]}, success=False)

    def test_unknown_material_block_is_rejected_instead_of_expanding_selected_denominator(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.run_summary(root, {'selected_task_names': ['long'], 'results': [],
                                   'material_gate': {'blocked_task_names': ['other']}}, success=False)

    def test_repeated_cli_summary_is_identical_and_never_overwrites_report(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); report = {'selected_task_names': ['long'], 'results': [native()]}
            first = self.run_summary(root, report)
            self.assertEqual(self.run_summary(root, report), first)
            result = subprocess.run([sys.executable, '-B', str(SCRIPT), '--report', str(root / 'report.json'),
                                     '--output', str(root / 'report.json')], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads((root / 'report.json').read_text()), report)

    def test_cli_cannot_overwrite_measured_monitor_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = {'selected_task_names': ['long'], 'results': []}
            (root / 'report.json').write_text(json.dumps(report))
            monitor = root / 'campaign-monitor.json'
            monitor.write_text('{"max_live_vmm_count": 1}')
            original = monitor.read_bytes()
            result = subprocess.run([sys.executable, '-B', str(SCRIPT), '--report', str(root / 'report.json'),
                                     '--output', str(monitor)], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(monitor.read_bytes(), original)


if __name__ == '__main__':
    unittest.main()


class SummaryOutputProtectionTests(unittest.TestCase):
    def test_existing_non_summary_file_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / 'report.json'
            report.write_text(json.dumps({'selected_task_names': ['task'], 'results': []}))
            for name, content in [('source.py', 'original source'), ('.env', 'PRIVATE=original'),
                                  ('another-report.json', '{"schema":1,"results":[]}')]:
                with self.subTest(name=name):
                    output = root / name
                    output.write_text(content)
                    result = subprocess.run([sys.executable, '-B', str(SCRIPT), '--report', str(report),
                        '--output', str(output)], capture_output=True, text=True, timeout=5)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(output.read_text(), content)
                    self.assertNotIn('PRIVATE', result.stderr)

    def test_output_symlink_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / 'report.json'
            report.write_text(json.dumps({'selected_task_names': ['task'], 'results': []}))
            source = root / 'source.txt'; source.write_text('original source')
            output = root / 'summary.json'; output.symlink_to(source)
            result = subprocess.run([sys.executable, '-B', str(SCRIPT), '--report', str(report),
                '--output', str(output)], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertTrue(output.is_symlink())
            self.assertEqual(source.read_text(), 'original source')
