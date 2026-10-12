import importlib
from pathlib import Path
import threading
import time
import unittest
from disk_budget import DiskBudget


class ContinuousTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('continuous.py').exists(), 'Continuous worker scheduler is missing')
        return importlib.import_module('continuous')

    def run(self, result=None):
        return super().run(result)

    def test_refill_waits_for_cleanup_but_not_slow_neighbors(self):
        m = self.module(); events = []; slow = threading.Event(); delete = threading.Event()
        started = threading.Event(); ninth = threading.Event(); all_started = threading.Barrier(8)
        def execute(t, prepared, stop):
            if t['id'] < 8: all_started.wait(timeout=5)
            if t['id'] == 0: started.set()
            elif t['id'] < 8: self.assertTrue(slow.wait(5))
            else: ninth.set()
            return {'native': t['id']}
        def cleanup(t):
            if t['id'] == 0: self.assertTrue(delete.wait(5))
            return True
        output = {}
        thread = threading.Thread(target=lambda: output.update(m.run_continuous(
            [{'id': i, 'disk_bytes': 10} for i in range(9)], concurrency=8,
            prepare=lambda t: {}, execute=execute, verify_cleanup=cleanup,
            capacity=DiskBudget(20, lambda: 200), record=events.append, cancelled=lambda: False)))
        thread.start()
        try:
            self.assertTrue(started.wait(5)); self.assertFalse(ninth.wait(.1))
            delete.set(); self.assertTrue(ninth.wait(5)); self.assertFalse(slow.is_set())
        finally: slow.set(); delete.set(); thread.join(6)
        self.assertFalse(thread.is_alive()); self.assertEqual(len(output['completed']), 9)
        self.assertLessEqual(max(e['active_slots'] for e in events), 8)
        self.assertLess(next(e['time'] for e in events if e['event'] == 'admitted' and e['id'] == 8),
                        next(e['time'] for e in events if e['event'] == 'cleaned' and e['id'] == 1))

    def test_cleanup_failure_holds_reservation_and_stops_refill(self):
        m = self.module(); b = DiskBudget(20, lambda: 200)
        result = m.run_continuous([{'id': i, 'disk_bytes': 10} for i in range(3)], concurrency=1,
            prepare=lambda t: {}, execute=lambda t, p, s: {'exit_code': 0}, verify_cleanup=lambda t: False,
            capacity=b, record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(len(result['cleanup_errors']), 1); self.assertEqual(len(result['pending']), 2)
        self.assertEqual(b.snapshot()['future_bytes'], 10)

    def test_failed_preparation_can_refill_only_after_proving_cleanup(self):
        m = self.module(); calls = []
        def prepare(t):
            if t['id'] == 0: raise RuntimeError('lookup failed')
            return {}
        r = m.run_continuous([{'id': i, 'disk_bytes': 10} for i in range(2)], concurrency=1,
            prepare=prepare, execute=lambda t, p, s: calls.append(t['id']), verify_cleanup=lambda t: True,
            capacity=DiskBudget(20, lambda: 200), record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(calls, [1]); self.assertEqual(len(r['completed']), 2)
        self.assertEqual(r['completed'][0]['error'], 'RuntimeError: lookup failed')

    def test_cancel_and_low_water_are_environment_interruptions(self):
        m = self.module(); free = [200]; events = []
        def execute(t, p, stop):
            free[0] = 19
            self.assertTrue(stop.wait(5))
            return {'exit_code': 143, 'original': 'native error retained'}
        r = m.run_continuous([{'id': i, 'disk_bytes': 10} for i in range(2)], concurrency=1,
            prepare=lambda t: {}, execute=execute, verify_cleanup=lambda t: True,
            capacity=DiskBudget(20, lambda: free[0]), record=events.append, cancelled=lambda: False)
        self.assertEqual(r['reason'], 'low_watermark'); self.assertEqual(len(r['interrupted']), 1)
        self.assertEqual(r['interrupted'][0]['result']['original'], 'native error retained')
        self.assertEqual(len(r['pending']), 1)
        r = m.run_continuous([{'id': 0, 'disk_bytes': 10}], concurrency=1,
            prepare=lambda t: self.fail('cancelled trial prepared'), execute=None, verify_cleanup=None,
            capacity=DiskBudget(20, lambda: 200), record=lambda e: None, cancelled=lambda: True)
        self.assertEqual(r['reason'], 'cancelled'); self.assertEqual(len(r['pending']), 1)

    def test_unadmittable_idle_queue_returns_without_spinning(self):
        m = self.module()
        r = m.run_continuous([{'id': 0, 'disk_bytes': 100}], concurrency=8,
            prepare=None, execute=None, verify_cleanup=None, capacity=DiskBudget(20, lambda: 100),
            record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(r['reason'], 'blocked_disk'); self.assertEqual(len(r['pending']), 1)

    def test_outer_native_deadline_is_interruption_even_without_global_cancel(self):
        m = self.module()
        r = m.run_continuous([{'id': 0, 'disk_bytes': 10}], concurrency=1,
            prepare=lambda t: {}, execute=lambda t, p, s: {'exit_code': 143, 'interruption': 'deadline'},
            verify_cleanup=lambda t: True, capacity=DiskBudget(20, lambda: 200),
            record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(len(r['interrupted']), 1); self.assertEqual(r['completed'], [])

    def test_preflight_shared_lease_survives_failed_private_admission(self):
        m = self.module(); budget = DiskBudget(20, lambda: 100)
        budget.admit('layer', 8); budget.materialized('layer', 2)
        r = m.run_continuous([{'id': 0, 'disk_bytes': 80, 'shared': {'layer': 8}}], concurrency=1,
            prepare=None, execute=None, verify_cleanup=None, capacity=budget,
            record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(r['reason'], 'blocked_disk'); self.assertEqual(budget.snapshot()['future_bytes'], 6)

    def test_preparation_disk_loss_prevents_guest_start(self):
        m = self.module(); free = [200]; calls = []
        def prepare(t): free[0] = 19; return {}
        r = m.run_continuous([{'id': 0, 'disk_bytes': 10}], concurrency=1, prepare=prepare,
            execute=lambda t, p, s: calls.append('VM started'), verify_cleanup=lambda t: True,
            capacity=DiskBudget(20, lambda: free[0]), record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(calls, []); self.assertEqual(len(r['interrupted']), 1)
        self.assertEqual(r['reason'], 'low_watermark')

    def test_unknown_private_materialization_does_not_cancel_a_fitting_active_trial(self):
        m = self.module(); free = [200]; barrier = threading.Barrier(2)
        allocated = threading.Event(); second = threading.Event(); calls = []
        def prepare(t):
            barrier.wait(timeout=3)
            if t['id'] == 1: allocated.wait(3)
            return {}
        def execute(t, p, stop):
            calls.append(t['id'])
            if t['id'] == 0:
                free[0] = 170; allocated.set(); second.wait(.2)
            else: second.set()
            return {}
        m.run_continuous([{'id': i, 'disk_bytes': 80} for i in range(2)], concurrency=2,
            prepare=prepare, execute=execute, verify_cleanup=lambda t: True,
            capacity=DiskBudget(20, lambda: free[0]), record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(sorted(calls), [0, 1])

    def test_disk_wait_does_not_block_a_smaller_fitting_task(self):
        m = self.module(); calls = []
        r = m.run_continuous([{'id': 0, 'disk_bytes': 100}, {'id': 1, 'disk_bytes': 10}], concurrency=8,
            prepare=lambda t: {}, execute=lambda t, p, s: calls.append(t['id']), verify_cleanup=lambda t: True,
            capacity=DiskBudget(20, lambda: 100), record=lambda e: None, cancelled=lambda: False)
        self.assertEqual(calls, [1]); self.assertEqual([t['id'] for t in r['pending']], [0])


if __name__ == '__main__': unittest.main()
