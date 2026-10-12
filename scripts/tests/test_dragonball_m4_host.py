"""Real-process contracts for mutually exclusive Dragonball test entrypoints."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class HostLockTests(unittest.TestCase):
    def test_stale_resources_block_new_work_without_deleting_them(self):
        import dragonball_m4_host as host
        self.assertTrue(hasattr(host, "require_clean_host"), "stale host resource gate missing")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            groups, devices = root / "groups", root / "devices"
            groups.mkdir()
            devices.mkdir()
            host.require_clean_host(groups, devices)
            stale = groups / "db-m4.abandoned"
            stale.mkdir()
            with self.assertRaisesRegex(RuntimeError, "audit"):
                host.require_clean_host(groups, devices)
            self.assertTrue(stale.exists())
            stale.rmdir()
            device = devices / "ublkc27"
            device.touch()
            with self.assertRaisesRegex(RuntimeError, "audit"):
                host.require_clean_host(groups, devices)
            self.assertTrue(device.exists())

    def test_root_and_test_user_can_share_a_lock_created_with_restrictive_umask(self):
        from dragonball_m4_host import acquire_host_lock
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.lock"
            old = os.umask(0o077)
            try:
                fd = acquire_host_lock(path)
                os.close(fd)
            finally:
                os.umask(old)
            self.assertTrue(path.stat().st_mode & 0o004, "delegated account must be able to open root lock")

    def test_other_entrypoint_is_refused_until_owner_exits(self):
        from dragonball_m4_host import acquire_host_lock
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.lock"
            code = """
import sys
from pathlib import Path
from dragonball_m4_host import exec_locked
exec_locked([sys.executable, '-c', "import sys; print('locked', flush=True); sys.stdin.read()"], Path(sys.argv[1]))
"""
            child = subprocess.Popen([sys.executable, "-c", code, str(path)],
                                     env={**os.environ, "PYTHONPATH": str(Path(__file__).parent)},
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            try:
                import selectors
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(timeout=5), "lock helper did not become ready")
                self.assertEqual(child.stdout.readline().strip(), "locked")
                with self.assertRaisesRegex(RuntimeError, "test host is busy"):
                    acquire_host_lock(path)
                child.kill()
                child.wait(timeout=5)
                fd = acquire_host_lock(path)
                os.close(fd)
                self.assertTrue(path.exists(), "never unlink a lock file while other openers may exist")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=5)
                child.stdin.close()
                child.stdout.close()

    def test_lock_refuses_symlinks_without_modifying_the_target(self):
        from dragonball_m4_host import acquire_host_lock
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "keep"
            target.write_text("preserve")
            path = Path(directory) / "host.lock"
            path.symlink_to(target)
            with self.assertRaises(OSError):
                acquire_host_lock(path)
            self.assertEqual(target.read_text(), "preserve")



class InheritedLockTests(unittest.TestCase):
    def test_nested_entry_uses_actual_inherited_lease(self):
        import dragonball_m4_host as host
        self.assertTrue(hasattr(host, 'host_lock'), 'missing scoped inheritable lease')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'host.lock'
            with host.host_lock(path) as fd:
                code = ('import os,sys; from pathlib import Path; '
                        'from dragonball_m4_host import host_lock; '
                        'lease=host_lock(Path(sys.argv[1])); '
                        'lease.__enter__(); print("nested"); lease.__exit__(None,None,None)')
                result = subprocess.run([sys.executable, '-c', code, str(path)],
                    env={**os.environ, 'PYTHONPATH': str(Path(__file__).parent)},
                    pass_fds=(fd,), capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), 'nested')
                with self.assertRaisesRegex(RuntimeError, 'busy'):
                    host.acquire_host_lock(path)

    def test_isolated_controller_preserves_parent_lease_in_child(self):
        import dragonball_m4_host as host
        from dragonball_m4_scenarios import isolated_controller
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'host.lock'
            code = ('import sys; from pathlib import Path; '
                    'from dragonball_m4_host import lock_fds; '
                    'assert lock_fds(Path(sys.argv[1]))')
            with patch.object(host, 'LOCK_PATH', path), host.host_lock(path):
                result = isolated_controller([sys.executable, '-c', code, str(path)],
                    {**os.environ, 'PYTHONPATH': str(Path(__file__).parent)})
                self.assertEqual(result, 0)
                with self.assertRaisesRegex(RuntimeError, 'busy'):
                    host.acquire_host_lock(path)

    def test_wrong_descriptor_cannot_bypass_a_busy_host(self):
        import dragonball_m4_host as host
        from unittest.mock import patch
        self.assertTrue(hasattr(host, 'host_lock'), 'missing scoped inheritable lease')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'host.lock'
            fd = host.acquire_host_lock(path)
            try:
                with open(Path(directory) / 'unrelated', 'w') as unrelated:
                    with patch.dict(os.environ, {'AENV_TEST_HOST_LOCK_FD': str(unrelated.fileno())}):
                        with self.assertRaisesRegex(ValueError, 'inherited host lock'):
                            with host.host_lock(path):
                                self.fail('unrelated descriptor admitted')
            finally:
                os.close(fd)

    def test_separately_opened_same_inode_cannot_impersonate_owner(self):
        import dragonball_m4_host as host
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'host.lock'
            fd = host.acquire_host_lock(path)
            try:
                with path.open('r') as impostor:
                    with patch.dict(os.environ, {'AENV_TEST_HOST_LOCK_FD': str(impostor.fileno())}):
                        with self.assertRaisesRegex(ValueError, 'inherited host lock'):
                            with host.host_lock(path):
                                self.fail('unlocked descriptor admitted over real owner')
            finally:
                os.close(fd)

    @unittest.skipUnless(sys.platform == 'linux' and os.geteuid() == 0,
                         'explicit root-to-aenv Linux descriptor qualification')
    def test_runuser_preserves_lease_for_nonroot_child(self):
        import dragonball_m4_host as host
        import pwd
        import selectors
        uid = pwd.getpwnam('aenv').pw_uid
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root.chmod(0o755)
            path = root / 'host.lock'
            with host.host_lock(path) as fd:
                code = ('import os,sys; from pathlib import Path; '
                        'from dragonball_m4_host import lock_fds; '
                        'assert lock_fds(Path(sys.argv[1])); '
                        'print(os.geteuid(),flush=True); sys.stdin.read()')
                child = subprocess.Popen(['runuser', '-u', 'aenv', '--', sys.executable, '-c', code, str(path)],
                    env={**os.environ, 'PYTHONPATH': str(Path(__file__).parent.resolve())},
                    pass_fds=(fd,), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
                try:
                    with selectors.DefaultSelector() as selector:
                        selector.register(child.stdout, selectors.EVENT_READ)
                        self.assertTrue(selector.select(5), 'delegated child not ready')
                    self.assertEqual(child.stdout.readline().strip(), str(uid))
                    with self.assertRaisesRegex(RuntimeError, 'busy'):
                        host.acquire_host_lock(path)
                finally:
                    child.communicate(timeout=5)
                self.assertEqual(child.returncode, 0)

    @unittest.skipUnless(sys.platform == 'linux' and os.geteuid() == 0,
                         'explicit root capability-wrapper Linux qualification')
    def test_capability_wrapper_preserves_lease_and_workload_identity(self):
        import json
        import pwd
        import dragonball_m4_host as host
        with tempfile.TemporaryDirectory(dir='/var/tmp') as directory:
            root = Path(directory)
            root.chmod(0o755)
            path = root / 'host.lock'
            wrapper = Path(__file__).resolve().parents[1] / 'run-with-capabilities.sh'
            child = ('import json,os,sys; from pathlib import Path; '
                     'from dragonball_m4_host import lock_fds; '
                     'assert lock_fds(Path(sys.argv[1])); '
                     'print(json.dumps({"uid":os.geteuid(),"status":Path("/proc/self/status").read_text()}))')
            command = ['bash', str(wrapper), sys.executable, '-c', child, str(path)]
            controller = ('import json,sys; from pathlib import Path; '
                          'from dragonball_m4_host import exec_locked; '
                          'exec_locked(json.loads(sys.argv[1]),Path(sys.argv[2]))')
            result = subprocess.run([sys.executable, '-c', controller, json.dumps(command), str(path)],
                env={**os.environ, 'PYTHONPATH': str(Path(__file__).parent.resolve()),
                     'AENV_RUN_USER': 'aenv', 'AENV_HOME_PATH': str(root / 'home'),
                     'AENV_RUNTIME_PATH': str(root / 'run')},
                text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads(result.stdout)
            self.assertEqual(data['uid'], pwd.getpwnam('aenv').pw_uid)
            effective = int(next(line.split()[1] for line in data['status'].splitlines()
                                 if line.startswith('CapEff:')), 16)
            self.assertEqual(effective & ((1 << 12) | (1 << 21)), (1 << 12) | (1 << 21))

    def test_child_retains_lock_after_controller_is_killed(self):
        import dragonball_m4_host as host
        self.assertTrue(hasattr(host, 'host_lock'), 'missing scoped inheritable lease')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'host.lock'
            code = '''
import os,subprocess,sys
from pathlib import Path
from dragonball_m4_host import host_lock, lock_fds
with host_lock(Path(sys.argv[1])):
    child = subprocess.Popen([sys.executable, '-c', "import sys; print('ready',flush=True); sys.stdin.read()"], pass_fds=lock_fds(Path(sys.argv[1])))
    print(child.pid, flush=True)
    child.wait()
'''
            parent = subprocess.Popen([sys.executable, '-c', code, str(path)],
                env={**os.environ, 'PYTHONPATH': str(Path(__file__).parent)},
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            try:
                import selectors
                with selectors.DefaultSelector() as selector:
                    selector.register(parent.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(5), 'controller not ready')
                child_pid = int(parent.stdout.readline())
                self.assertEqual(parent.stdout.readline().strip(), 'ready')
                parent.kill()
                parent.wait(timeout=5)
                with self.assertRaisesRegex(RuntimeError, 'busy'):
                    host.acquire_host_lock(path)
                parent.stdin.close()
                # EOF means the child exits and closes its inherited descriptor.
                self.assertEqual(parent.stdout.read(), '')
                fd = host.acquire_host_lock(path)
                os.close(fd)
            finally:
                if parent.poll() is None:
                    parent.kill()
                    parent.wait(timeout=5)
                if not parent.stdin.closed:
                    parent.stdin.close()
                parent.stdout.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
