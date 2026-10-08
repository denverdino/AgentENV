import copy
import importlib
from pathlib import Path
import tempfile
import unittest


class PhaseTests(unittest.TestCase):
    def setUp(self):
        self.meta = dict(operation_id='run/0/resume/0', backend='dragonball',
                         operation='resume', sandbox_id='sb1', timing_version='db-overlap-v1')
        self.ready = 'sandbox_id=sb1 resumed_us=5 envd_us=25 daemon_notified_us=28 initialized_us=35 identity_us=35 attempts=0 ready_us=40 Dragonball readiness cumulative timing'
        self.prep = 'sandbox_id=sb1 network_us=2 disks_us=10 runner_us=4 memory_us=3 load_us=7 post_load_us=1 total_us=30 restore preparation phase timing'

    def parse(self, text=None, **meta):
        return importlib.import_module('dragonball_m4_phases').parse_operation(
            self.prep + '\n' + self.ready if text is None else text, **{**self.meta, **meta})

    def test_order_independent_join_and_overlap_not_summed(self):
        first = self.parse()
        self.assertEqual(first, self.parse(self.ready + '\n' + self.prep))
        self.assertEqual(first['parts']['health_us'], 20)
        self.assertEqual(first['parts']['preparation_through_release_us'], 35)
        self.assertEqual(first['parts']['measured_internal_us'], 70)
        self.assertEqual(first['parts']['runner_us'], 4)

    def test_missing_duplicate_wrong_sandbox_and_invalid_durations_rejected(self):
        for text in (self.ready, self.prep, self.prep + '\n' + self.ready + '\n' + self.ready,
                     self.prep + '\n' + self.ready.replace('sb1', 'sb2'),
                     self.prep + '\n' + self.ready.replace('envd_us=25', 'envd_us=3'),
                     'backend=firecracker ' + self.prep + '\n' + self.ready,
                     self.prep + ' release_us=5\n' + self.ready,
                     self.prep.replace('load_us=7', 'load_us=-1') + '\n' + self.ready):
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.parse(text)

    def test_cold_requires_only_readiness_and_fc_release_not_double_counted(self):
        row = self.parse(self.ready, operation='cold')
        self.assertNotIn('preparation_through_release_us', row['parts'])
        ready = 'sandbox_id=sb1 envd_us=20 initialized_us=30 ready_us=35 readiness cumulative timing'
        row = self.parse(self.prep + ' release_us=5\n' + ready,
                         backend='firecracker', timing_version='fc-v1')
        self.assertEqual(row['parts']['preparation_through_release_us'], 30)
        self.assertEqual(row['parts']['measured_internal_us'], 65)

    def test_unknown_version_and_operation_rejected(self):
        for meta in ({'timing_version': 'future'}, {'operation': 'guess'}, {'operation_id': ''}):
            with self.subTest(meta=meta), self.assertRaises(ValueError):
                self.parse(**meta)

    def test_window_excludes_previous_same_id_and_detects_truncation(self):
        m = importlib.import_module('dragonball_m4_phases')
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / 'server.log'
            log.write_text(self.prep + '\n' + self.ready + '\n')
            window = m.Window(log)
            with log.open('a') as out:
                out.write(self.ready + '\n' + self.prep + '\n')
            row = window.finish([{'ok': True, 'value': {'sandboxID': 'sb1'}}],
                                backend='dragonball', operation='resume',
                                operation_id='run/0/resume', timing_version='db-overlap-v1')[0]
            self.assertEqual(row['operation_id'], 'run/0/resume/0')
            self.assertEqual(row['parts']['health_us'], 20)
            log.write_text('')
            with self.assertRaises(ValueError):
                window.finish([], backend='dragonball', operation='resume',
                              operation_id='run/0/resume', timing_version='db-overlap-v1')

    def test_analysis_refuses_missing_duplicate_mixed_version_or_changed_parts(self):
        m = importlib.import_module('dragonball_m4_phases')
        row = self.parse()
        report = m.analyze([row], [row['operation_id']])
        self.assertEqual(report['summary'][0]['parts']['health_us']['mean'], 20)
        changed = copy.deepcopy(row)
        changed['parts']['health_us'] = 100
        other = self.parse(operation_id='run/1/resume/0', timing_version='db-serial-v1')
        for rows, expected in (([], [row['operation_id']]), ([row, row], [row['operation_id']]),
                               ([row, other], [row['operation_id'], other['operation_id']]),
                               ([changed], [row['operation_id']])):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                m.analyze(rows, expected)


if __name__ == '__main__':
    unittest.main()
