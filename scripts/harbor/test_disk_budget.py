import importlib
from pathlib import Path
import threading
import unittest

GIB = 1024**3


class DiskBudgetTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('disk_budget.py').exists(),
                        'Stage-aware disk admission is missing')
        return importlib.import_module('disk_budget')

    def test_reuse_reserves_materialization_but_not_a_template_builder(self):
        m = self.module()
        reuse = m.task_disk_budget({'storage_mb': 10240}, memory_mb=8192,
            template_mode='reuse', archive_bytes=GIB, cached_bytes=0)
        self.assertEqual(reuse['future_bytes'], 19 * GIB)
        self.assertEqual(reuse['builder_bytes'], 0)
        self.assertEqual(reuse['virtual_bytes'], 64 * GIB)
        self.assertFalse(reuse['storage_limit_enforced'])
        build = m.task_disk_budget({'storage_mb': 10240}, memory_mb=8192,
            template_mode='prepare', archive_bytes=GIB, cached_bytes=0)
        self.assertEqual(build['future_bytes'], 29 * GIB)

    def test_cached_template_materialization_is_not_reserved_twice(self):
        m = self.module()
        budget = m.task_disk_budget({'storage_mb': 10240}, memory_mb=8192,
            template_mode='reuse', archive_bytes=0, cached_bytes=8 * GIB)
        self.assertEqual(budget['future_bytes'], 10 * GIB)
        free = [40 * GIB]
        account = m.DiskBudget(20 * GIB, lambda: free[0])
        self.assertTrue(account.admit('shared:snapshot', 8 * GIB))
        self.assertTrue(account.admit('shared:snapshot', 8 * GIB))
        self.assertEqual(account.snapshot()['future_bytes'], 8 * GIB)
        self.assertFalse(account.admit('trial:one', 13 * GIB))

    def test_materialization_moves_reservation_to_real_free_space(self):
        account = self.module().DiskBudget(20 * GIB, lambda: free[0])
        free = [40 * GIB]
        self.assertTrue(account.admit('one', 18 * GIB))
        free[0] -= 8 * GIB
        account.materialized('one', 8 * GIB)
        account.materialized('one', 8 * GIB)
        self.assertEqual(account.snapshot()['future_bytes'], 10 * GIB)
        self.assertTrue(account.admit('two', 2 * GIB))
        self.assertFalse(account.admit('three', 1))
        account.release('one')
        account.release('one')
        self.assertEqual(account.snapshot()['free_bytes'], 32 * GIB)
        self.assertEqual(account.snapshot()['future_bytes'], 2 * GIB)

    def test_eight_private_reservations_obey_safety_margin_atomically(self):
        account = self.module().DiskBudget(20 * GIB, lambda: 100 * GIB)
        results = []
        threads = [threading.Thread(target=lambda i=i: results.append(account.admit(str(i), 10 * GIB)))
                   for i in range(16)]
        for t in threads: t.start()
        for t in threads: t.join()
        self.assertEqual(results.count(True), 8)
        self.assertEqual(account.snapshot()['future_bytes'], 80 * GIB)

    def test_extra_writes_or_external_allocations_block_new_admission(self):
        free = [50 * GIB]
        account = self.module().DiskBudget(20 * GIB, lambda: free[0])
        self.assertTrue(account.admit('one', 10 * GIB))
        free[0] = 19 * GIB
        account.materialized('one', 31 * GIB)
        self.assertFalse(account.admit('two', 1))
        self.assertTrue(account.snapshot()['low_watermark'])

    def test_invalid_or_conflicting_budget_cannot_create_capacity(self):
        account = self.module().DiskBudget(20 * GIB, lambda: 100 * GIB)
        self.assertTrue(account.admit('one', 10 * GIB))
        for action in (lambda: account.admit('one', 11 * GIB),
                       lambda: account.admit('bad', -1),
                       lambda: account.materialized('one', -1),
                       lambda: account.materialized('missing', GIB)):
            with self.assertRaises(ValueError): action()
        account.materialized('one', GIB)
        with self.assertRaises(ValueError): account.materialized('one', 0)
        self.assertEqual(account.snapshot()['future_bytes'], 9 * GIB)


if __name__ == '__main__':
    unittest.main()
