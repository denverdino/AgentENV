import importlib
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


class ObservationTests(unittest.TestCase):
    def test_wire_counters_reject_missing_and_reset(self):
        m = importlib.import_module('dragonball_m4_observe')
        raw = {'nftables': [{'counter': {'name': name, 'bytes': size, 'packets': 1}}
                           for name, size in [('s3_upload', 100), ('s3_download', 200)]]}
        before = m.wire_values(raw)
        self.assertEqual(m.wire_delta(before, {**before, 's3_download': 250})['s3_download'], 50)
        with self.assertRaises(ValueError): m.wire_values({'nftables': []})
        with self.assertRaises(ValueError): m.wire_delta(before, {**before, 's3_upload': 99})

    def test_heartbeat_requires_bracketing_and_contiguous_sequence(self):
        m = importlib.import_module('dragonball_m4_observe')
        rows = [(0., 1), (.01, 2), (.15, 3), (.16, 4)]
        result = m.heartbeat_gaps(rows, .02, .14)
        self.assertAlmostEqual(result['max_gap_seconds'], .14)
        self.assertEqual(result['lost_packets'], 0)
        with self.assertRaises(ValueError): m.heartbeat_gaps(rows[:2], .02, .14)
        with self.assertRaises(ValueError): m.heartbeat_gaps([(0., 1), (.15, 3)], .02, .14)
        with self.assertRaises(ValueError): m.heartbeat_gaps([(0., 2), (.15, 1)], .02, .14)

    def test_memory_sampler_preserves_peak_and_reports_read_failure(self):
        m = importlib.import_module('dragonball_m4_observe')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'memory.current'
            path.write_text('100')
            sampler = m.MemorySampler(path, interval=.005)
            time.sleep(.02); path.write_text('200'); time.sleep(.03)
            result = sampler.finish()
            self.assertGreaterEqual(result['observed_peak_bytes'], 200)
            sampler = m.MemorySampler(path, interval=.005)
            path.unlink(); time.sleep(.02)
            with self.assertRaises(RuntimeError): sampler.finish()

    @unittest.skipUnless(sys.platform == 'linux' and shutil.which('cc'), 'guest sender executes on Linux')
    def test_guest_sender_pause_is_observed_on_host_clock(self):
        m = importlib.import_module('dragonball_m4_observe')
        source = Path(__file__).with_name('dragonball_m4_heartbeat.c')
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'heartbeat'
            subprocess.run(['cc', '-O2', '-Wall', '-Wextra', '-Werror', str(source), '-o', str(binary)], check=True)
            collector = m.HeartbeatCollector('127.0.0.1')
            child = subprocess.Popen([str(binary), '127.0.0.1', str(collector.port), collector.token])
            try:
                collector.wait_samples(10, 2)
                start = time.monotonic(); child.send_signal(signal.SIGSTOP)
                time.sleep(.12); child.send_signal(signal.SIGCONT); end = time.monotonic()
                time.sleep(.05)
                result = collector.result(start, end)
                self.assertGreater(result['max_gap_seconds'], .10)
                self.assertLess(result['max_gap_seconds'], 1.)
                self.assertEqual(result['lost_packets'], 0)
            finally:
                child.send_signal(signal.SIGCONT); child.terminate(); child.wait(timeout=2)
                collector.close()
