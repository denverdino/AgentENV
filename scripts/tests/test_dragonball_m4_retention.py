"""Retention refuses ambiguous ownership and preserves verified evidence."""
import importlib
import json
import os
import signal
import socket
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


class RetentionTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(Path(__file__).with_name('dragonball_m4_retention.py').exists(),
                        'bounded M4 retention is missing')
        self.m = importlib.import_module('dragonball_m4_retention')
        self.r = importlib.import_module('dragonball_m4_recovery')
        self.temp = tempfile.TemporaryDirectory(prefix='m4-retention-', dir='/tmp')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        for name in ('runs', 'proc', 'dev', 'cgroups'):
            (self.base / name).mkdir()
        for key, value in {'RUNS': self.base / 'runs', 'PROC': self.base / 'proc',
                           'DEV': self.base / 'dev', 'CGROUPS': self.base / 'cgroups',
                           'OWNER_UID': os.getuid()}.items():
            context = patch.object(self.r, key, value)
            context.start()
            self.addCleanup(context.stop)
        real_lock = self.m.acquire_host_lock
        context = patch.object(self.m, 'acquire_host_lock',
                               side_effect=lambda: real_lock(self.base / 'host.lock'))
        context.start()
        self.addCleanup(context.stop)
        (self.base / 'proc/sys/kernel/random').mkdir(parents=True)
        (self.base / 'proc/sys/kernel/random/boot_id').write_text('boot')
        self.root = self.base / 'runs/db-m4.retention'
        self.root.mkdir()
        self.manifest = {'schema': 1, 'root': str(self.root), 'root_identity': self.r.file_identity(self.root),
                         'boot_id': 'boot', 'cgroup_identity': [0, 0], 'namespaces': [],
                         'controller': {'pid': 123, 'start_time': '100'}, 'state': 'running',
                         'run_user': 'aenv'}
        self.r.save_manifest(self.root, self.manifest)
        self.result = {'root': str(self.root), 'cleanup': 'passed', 'inputs_verified': True,
                       'measurements': {'groups': {'create': {'count': 1, 'successes': 1}}}}
        self.save_result()
        (self.root / 'a/home/logs').mkdir(parents=True)
        (self.root / 'a/home/payload').write_bytes(b'x' * 8192)
        (self.root / 'a/home/logs/server.log').write_text('important evidence')
        (self.root / 'a/config.toml').write_text('configuration')
        (self.root / 'repo').mkdir()
        (self.root / 'repo/data').write_bytes(b'x' * 8192)
        (self.root / 'private-tools.ext4').write_bytes(b'x' * 8192)

    def save_result(self):
        (self.root / 'result.json').write_text(json.dumps(self.result))

    def archive(self):
        return self.m.archive(self.root)

    def collect(self, digest, **kwargs):
        return self.m.retain(self.root, archive_sha256=digest, **kwargs)

    def test_audit_preserves_payload_and_apply_is_idempotent(self):
        digest = self.archive()
        report = self.collect(digest)
        self.assertEqual(report['status'], 'eligible')
        self.assertTrue((self.root / 'a/home/payload').exists())
        self.assertEqual(report['deleted_paths'], [])
        report = self.collect(digest, apply=True)
        self.assertEqual(report['status'], 'reclaimed')
        self.assertGreater(report['bytes_before'], report['bytes_after'])
        self.assertFalse((self.root / 'a/home').exists())
        self.assertEqual((self.root / 'evidence/a/home/logs/server.log').read_text(), 'important evidence')
        self.assertTrue((self.root / 'a/config.toml').exists())
        self.assertEqual(self.collect(digest, apply=True)['status'], 'already_reclaimed')
        receipts = [json.loads(p.read_text()) for p in self.root.glob('retention-*.json')]
        self.assertEqual(len(receipts), 3)
        self.assertTrue(all('free_bytes_before' in row and 'errors' in row for row in receipts))

    def test_live_reference_kinds_refuse_deletion(self):
        digest = self.archive()
        proc = self.base / 'proc/456'
        (proc / 'fd').mkdir(parents=True)
        (proc / 'ns').mkdir()
        for name, target in [('fd/7', self.root), ('cwd', self.root / 'a'), ('root', self.root)]:
            with self.subTest(name=name):
                (proc / name).symlink_to(target)
                self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
                (proc / name).unlink()
        for name in ('mountinfo', 'maps'):
            (proc / name).write_text('references ' + str(self.root / 'a/home'))
            self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
            (proc / name).unlink()
        self.assertTrue((self.root / 'a/home/payload').exists())

    def test_controller_cgroup_and_devices_refuse_deletion(self):
        digest = self.archive()
        proc = self.base / 'proc/123'
        proc.mkdir()
        (proc / 'stat').write_text('123 (controller) ' + ' '.join(['S'] + ['0'] * 18 + ['100']))
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        (proc / 'stat').unlink()
        device = self.base / 'dev/ublkc27'
        device.write_text('reused device')
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue(device.exists())
        device.unlink()
        (self.base / 'cgroups' / self.root.name).mkdir()
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.root / 'a/home/payload').exists())

    def test_namespace_reference_refuses_deletion(self):
        self.manifest['namespaces'] = ['net:[42]']
        self.r.save_manifest(self.root, self.manifest)
        digest = self.archive()
        proc = self.base / 'proc/456'
        (proc / 'ns').mkdir(parents=True)
        (proc / 'ns/net').symlink_to('net:[42]')
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')

    def test_failed_unfinished_or_failed_samples_cannot_be_archived(self):
        for changes in ({'error': 'failed'}, {'cleanup': 'unfinished'}, {'inputs_verified': False},
                        {'measurements': {'groups': {'create': {'count': 2, 'successes': 1}}}}):
            with self.subTest(changes=changes):
                original = dict(self.result)
                self.result.update(changes)
                self.save_result()
                with self.assertRaises(ValueError):
                    self.archive()
                self.result = original

    def test_pin_refuses_archive_and_cleanup(self):
        pin = self.root / 'PIN'
        pin.write_text('keep')
        with self.assertRaises(ValueError):
            self.archive()
        pin.unlink()
        digest = self.archive()
        pin.write_text('keep')
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')

    def test_missing_untrusted_or_modified_archive_refuses(self):
        self.assertEqual(self.collect('0' * 64, apply=True)['status'], 'refused')
        digest = self.archive()
        receipt = self.root / 'archive-receipt.json'
        receipt.chmod(0o666)
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        receipt.chmod(0o644)
        receipt.write_text(receipt.read_text() + ' ')
        receipt.chmod(0o444)
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.root / 'a/home/payload').exists())

    def test_corrupt_evidence_or_changed_result_refuses(self):
        digest = self.archive()
        evidence = self.root / 'evidence/a/home/logs/server.log'
        evidence.chmod(0o644)
        evidence.write_text('changed')
        evidence.chmod(0o444)
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        evidence.chmod(0o644)
        evidence.write_text('important evidence')
        evidence.chmod(0o444)
        self.result['error'] = 'later failure'
        self.save_result()
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')

    def test_symlinks_unknown_layout_and_escape_refuse_archive(self):
        for name in ('a/unknown', 'unknown'):
            path = self.root / name
            path.mkdir()
            with self.assertRaises(ValueError):
                self.archive()
            path.rmdir()
        link = self.root / 'a/home/escape'
        link.symlink_to(self.base)
        with self.assertRaises(ValueError):
            self.archive()
        link.unlink()
        with self.assertRaises(ValueError):
            self.m.archive(self.root, evidence=['../outside'])
        self.assertEqual(self.m.retain(self.root / '..', archive_sha256='0' * 64, apply=True)['status'], 'refused')

    def test_payload_identity_replacement_refuses_cleanup(self):
        digest = self.archive()
        original = self.root / 'a/home'
        original.rename(self.base / 'saved')
        original.mkdir()
        (original / 'unrelated').write_text('keep')
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertEqual((original / 'unrelated').read_text(), 'keep')

    def test_root_replacement_refuses_cleanup(self):
        digest = self.archive()
        saved = self.root.with_name('db-m4.saved')
        self.root.rename(saved)
        self.root.mkdir()
        (self.root / 'lab-manifest.json').write_bytes((saved / 'lab-manifest.json').read_bytes())
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')

    def test_manifest_pin_and_unrecorded_deletion_refuse(self):
        self.manifest['pinned'] = True
        self.r.save_manifest(self.root, self.manifest)
        with self.assertRaisesRegex(ValueError, 'pinned'):
            self.archive()
        self.manifest.pop('pinned')
        self.r.save_manifest(self.root, self.manifest)
        digest = self.archive()
        (self.root / 'private-tools.ext4').unlink()
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.root / 'a/home').exists())

    def test_symlink_substitution_after_archive_refuses(self):
        digest = self.archive()
        (self.root / 'a/home').rename(self.base / 'saved')
        (self.root / 'a/home').symlink_to(self.base / 'saved')
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.base / 'saved/payload').exists())

    def test_partial_cleanup_has_durable_receipt_and_can_retry(self):
        digest = self.archive()
        remove = self.m.shutil.rmtree
        def fail_repo(path):
            if path.name == 'repo':
                raise OSError('injected busy repository')
            remove(path)
        with patch.object(self.m.shutil, 'rmtree', side_effect=fail_repo) as mocked:
            mocked.avoids_symlink_attacks = True
            report = self.collect(digest, apply=True)
        self.assertEqual(report['status'], 'refused')
        self.assertIn('a/home', report['deleted_paths'])
        self.assertIn('injected busy repository', report['errors'][0])
        self.assertTrue((self.root / 'repo/data').exists())
        self.assertEqual(self.collect(digest, apply=True)['status'], 'reclaimed')

    def partial_removal(self, failure):
        digest = self.archive()
        def interrupt_remove(path):
            (path / 'payload').unlink()
            raise failure
        with patch.object(self.m.shutil, 'rmtree', side_effect=interrupt_remove) as mocked:
            mocked.avoids_symlink_attacks = True
            if isinstance(failure, Exception):
                self.collect(digest, apply=True)
            else:
                with self.assertRaises(type(failure)):
                    self.collect(digest, apply=True)
        receipt = json.loads(next(self.root.glob('retention-*.json')).read_text())
        self.assertEqual(receipt['status'], 'refused')
        self.assertTrue(receipt['errors'])
        self.assertTrue(receipt['bytes_after'] is None or receipt['bytes_after'] < receipt['bytes_before'])
        self.assertEqual(receipt['deleted_paths'], [])
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.root / 'a/home/logs/server.log').exists())

    def test_keyboard_interrupt_after_partial_unlink_records_failure(self):
        self.partial_removal(KeyboardInterrupt())

    def test_oserror_after_partial_unlink_recomputes_accounting(self):
        self.partial_removal(OSError('partial deletion'))

    def test_cli_sigterm_records_partial_failure_and_restores_handler(self):
        digest = self.archive()
        previous = signal.getsignal(signal.SIGTERM)
        def terminate(path):
            (path / 'payload').unlink()
            handler = signal.getsignal(signal.SIGTERM)
            self.assertTrue(callable(handler), 'CLI needs a catchable SIGTERM handler')
            os.kill(os.getpid(), signal.SIGTERM)
        argv = ['retention', 'apply', '--root', str(self.root), '--archive-sha256', digest]
        with patch('sys.argv', argv), patch.object(self.m.os, 'geteuid', return_value=0), \
                patch.object(self.m.shutil, 'rmtree', side_effect=terminate) as mocked:
            mocked.avoids_symlink_attacks = True
            with self.assertRaises(SystemExit) as raised:
                self.m.main()
        self.assertEqual(raised.exception.code, 128 + signal.SIGTERM)
        self.assertEqual(signal.getsignal(signal.SIGTERM), previous)
        receipt = json.loads(next(self.root.glob('retention-*.json')).read_text())
        self.assertEqual(receipt['status'], 'refused')
        self.assertTrue(receipt['errors'])
        self.assertTrue(receipt['bytes_after'] is None or receipt['bytes_after'] < receipt['bytes_before'])
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')

    def test_identity_changed_during_apply_is_refused(self):
        digest = self.archive()
        audit = self.r.audit
        calls = 0
        def replace_after_first_check(root):
            nonlocal calls
            calls += 1
            if calls == 2:
                (root / 'a/home').rename(self.base / 'saved')
                (root / 'a/home').mkdir()
                (root / 'a/home/new').write_text('unrelated')
            return audit(root)
        with patch.object(self.r, 'audit', side_effect=replace_after_first_check):
            report = self.collect(digest, apply=True)
        self.assertEqual(report['status'], 'refused')
        self.assertEqual((self.root / 'a/home/new').read_text(), 'unrelated')

    def test_bound_and_unbound_runtime_socket_is_retained(self):
        runtime = self.root / 'a/run'
        runtime.mkdir()
        (runtime / 'diagnostic.json').write_text('runtime evidence')
        path = runtime / 'ublk-daemon.sock'
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(str(path))
            server.listen(1)
            identity = self.r.file_identity(path)
            digest = self.archive()
            report = self.collect(digest, apply=True)
            self.assertEqual(report['status'], 'reclaimed')
            self.assertEqual(report['retained_paths'], ['a/run'])
            self.assertIsInstance(report['retained_bytes'], int)
            self.assertEqual(self.r.file_identity(path), identity)
            self.assertEqual((runtime / 'diagnostic.json').read_text(), 'runtime evidence')
            self.assertEqual((self.root / 'evidence/a/run/diagnostic.json').read_text(), 'runtime evidence')
            # The bound endpoint still accepts connections after payload reclamation.
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(path))
                accepted, _ = server.accept()
                accepted.close()
        self.assertEqual(self.collect(digest, apply=True)['status'], 'already_reclaimed')
        self.assertEqual(self.r.file_identity(path), identity)

    def test_runtime_identity_replacement_refuses_cleanup(self):
        runtime = self.root / 'a/run'
        runtime.mkdir()
        digest = self.archive()
        runtime.rename(self.base / 'saved-run')
        runtime.mkdir()
        self.assertEqual(self.collect(digest, apply=True)['status'], 'refused')
        self.assertTrue((self.root / 'a/home/payload').exists())

    def test_socket_outside_retained_runtime_and_runtime_fifo_are_refused(self):
        path = self.root / 'a/home/unknown.sock'
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(str(path))
            with self.assertRaisesRegex(ValueError, 'special file'):
                self.archive()
        path.unlink()
        runtime = self.root / 'a/run'
        runtime.mkdir()
        os.mkfifo(runtime / 'unknown.fifo')
        with self.assertRaisesRegex(ValueError, 'special file'):
            self.archive()

    def test_operations_take_common_host_lock(self):
        import dragonball_m4_host
        lock = dragonball_m4_host.acquire_host_lock(self.base / 'host.lock')
        try:
            with self.assertRaisesRegex(RuntimeError, 'busy'):
                self.archive()
            with self.assertRaisesRegex(RuntimeError, 'busy'):
                self.collect('0' * 64, apply=True)
        finally:
            os.close(lock)

    def test_disk_admission_requires_peak_and_retention_reserve(self):
        with patch.object(self.m.shutil, 'disk_usage', return_value=(1000, 800, 200)):
            report = self.m.admit(self.root, reserve_bytes=100, planned_bytes=75, retained_bytes=25)
            self.assertTrue(report['admitted'])
            with self.assertRaisesRegex(ValueError, 'space'):
                self.m.admit(self.root, reserve_bytes=100, planned_bytes=76, retained_bytes=25)
            with self.assertRaises(ValueError):
                self.m.admit(self.root, reserve_bytes=-1, planned_bytes=0, retained_bytes=0)


if __name__ == '__main__':
    unittest.main()
