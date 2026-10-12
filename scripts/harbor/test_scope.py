import unittest


class ScopeTests(unittest.TestCase):
    def test_nested_vms_are_excluded_but_linux_graphics_are_retained(self):
        from scope import filter_microvm_tasks
        included, excluded = filter_microvm_tasks([
            'install-windows-3.11', 'qemu-alpine-ssh', 'qemu-startup',
            'make-doom-for-mips', 'make-mips-interpreter', 'build-pov-ray',
            'filter-js-from-html', 'sam-cell-seg', 'mcmc-sampling-stan'])
        self.assertEqual(included, ('make-doom-for-mips', 'make-mips-interpreter',
                                   'build-pov-ray', 'filter-js-from-html', 'sam-cell-seg',
                                   'mcmc-sampling-stan'))
        self.assertEqual({row['task'] for row in excluded},
                         {'install-windows-3.11', 'qemu-alpine-ssh', 'qemu-startup'})
        self.assertTrue(all(row['reason'] and row['evidence'] for row in excluded))


if __name__ == '__main__':
    unittest.main()
