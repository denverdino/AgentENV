"""Real producer processes must exit successfully before publication and checks."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest


class DeliveryChecksTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.partial = self.root / 'candidate.part'
        self.archive = self.root / 'candidate.tar.gz'
        self.candidate = self.root / 'candidate'
        self.receipts = self.root / 'receipts'
        self.producer = self.root / 'producer.tar.gz'
        code = b'from pathlib import Path\nPath(__file__).resolve().parents[4].joinpath("checked").write_text("ok")\n'
        name = 'scripts/harbor/check_tools.py'
        with tarfile.open(self.producer, 'w:gz') as bundle:
            for path, data in [('source/' + name, code), ('driver-sha256.json',
                    json.dumps({name: hashlib.sha256(code).hexdigest()}).encode())]:
                member = tarfile.TarInfo(path);member.size = len(data)
                bundle.addfile(member, io.BytesIO(data))
        self.digest = hashlib.sha256(self.producer.read_bytes()).hexdigest()
        self.size = self.producer.stat().st_size

    def run_delivery(self, code, timeout=10):
        from delivery_checks import run_delivery
        return run_delivery(self.partial, self.archive, self.candidate, self.digest, self.size,
                            [sys.executable, '-c', code], self.receipts, timeout=timeout)

    def copy_code(self):
        return f'from pathlib import Path; Path({str(self.partial)!r}).write_bytes(Path({str(self.producer)!r}).read_bytes())'

    def test_nonzero_producer_is_rejected_even_after_writing_complete_bytes(self):
        self.assertEqual(self.run_delivery(self.copy_code() + '; raise SystemExit(7)'), 1)
        attempt = json.loads(next((self.receipts / 'transfer').glob('*.json')).read_text())
        self.assertEqual(attempt['exit_code'], 7)
        self.assertFalse(self.archive.exists());self.assertFalse(self.candidate.exists())

    def test_truncated_transfer_cannot_start_checks(self):
        self.assertEqual(self.run_delivery(
            f'from pathlib import Path; Path({str(self.partial)!r}).write_bytes(b"truncated")'), 1)
        summary = json.loads((self.receipts / 'summary.json').read_text())
        self.assertEqual(summary['unattempted'], ['check'])
        self.assertFalse(self.archive.exists());self.assertFalse(self.candidate.exists())

    def test_timed_out_producer_cannot_publish_complete_bytes(self):
        self.assertEqual(self.run_delivery(self.copy_code() + '; import time; time.sleep(60)', timeout=0.2), 1)
        self.assertFalse(self.archive.exists());self.assertFalse(self.candidate.exists())

    def test_success_verifies_manifest_and_keeps_producer_copy_independent(self):
        self.assertEqual(self.run_delivery(self.copy_code()), 0)
        self.assertEqual((self.root / 'checked').read_text(), 'ok')
        self.partial.write_bytes(b'producer modified after exit')
        self.assertEqual(hashlib.sha256(self.archive.read_bytes()).hexdigest(), self.digest)

    def test_invalid_manifest_never_executes_candidate_code(self):
        with tarfile.open(self.producer) as bundle:
            content = [(member, bundle.extractfile(member).read()) for member in bundle.getmembers()]
        with tarfile.open(self.producer, 'w:gz') as bundle:
            for member, data in content:
                if member.name == 'driver-sha256.json':
                    data = json.dumps({'scripts/harbor/check_tools.py': '0' * 64}).encode()
                member.size = len(data)
                bundle.addfile(member, io.BytesIO(data))
        self.digest = hashlib.sha256(self.producer.read_bytes()).hexdigest()
        self.size = self.producer.stat().st_size
        self.assertEqual(self.run_delivery(self.copy_code()), 1)
        self.assertFalse((self.root / 'checked').exists())

    def test_unsafe_archive_is_rejected_before_extraction(self):
        with tarfile.open(self.producer, 'w:gz') as bundle:
            member = tarfile.TarInfo('../escaped')
            bundle.addfile(member, io.BytesIO())
        self.digest = hashlib.sha256(self.producer.read_bytes()).hexdigest()
        self.size = self.producer.stat().st_size
        self.assertEqual(self.run_delivery(self.copy_code()), 1)
        self.assertFalse(self.candidate.exists())
        self.assertFalse((self.root / 'escaped').exists())


    def test_slow_producer_is_awaited_before_publication(self):
        from delivery_checks import __file__ as entry
        ready = self.root / 'ready';gate = self.root / 'release'
        code = (f'from pathlib import Path; import time; data=Path({str(self.producer)!r}).read_bytes(); '
                f'p=Path({str(self.partial)!r}); p.write_bytes(data[:10]); Path({str(ready)!r}).touch(); '
                f'gate=Path({str(gate)!r})\nwhile not gate.exists():time.sleep(0.01)\np.write_bytes(data)')
        command = [sys.executable, '-B', entry, '--partial', str(self.partial), '--archive', str(self.archive),
                   '--candidate', str(self.candidate), '--sha256', self.digest, '--bytes', str(self.size),
                   '--receipts', str(self.receipts), '--timeout', '10', '--transfer', sys.executable, '-c', code]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
        try:
            deadline = time.monotonic() + 5
            while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(ready.exists())
            self.assertIsNone(process.poll())
            self.assertFalse(self.archive.exists());self.assertFalse(self.candidate.exists())
            gate.touch()
            stdout, stderr = process.communicate(timeout=15)
            self.assertEqual(process.returncode, 0, (stdout, stderr))
            self.assertEqual((self.root / 'checked').read_text(), 'ok')
        finally:
            if process.poll() is None:
                gate.touch();process.kill();process.communicate(timeout=5)
