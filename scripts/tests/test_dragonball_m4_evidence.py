"""Evidence transfer keeps interrupted/unverified bytes out of final filenames."""
import importlib
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base / 'source'
        self.root.mkdir()
        (self.root / 'result.json').write_text('{"status":"passed"}')
        self.m = importlib.import_module('dragonball_m4_evidence')
        self.bundle = self.base / 'bundle'

    def pack(self):
        return self.m.pack(self.root, ['result.json'], {'status': 'complete', 'tests': 1}, self.bundle)

    def test_summary_first_then_verified_full_archive(self):
        receipt = self.pack()
        calls = []
        def copy(host, source, target):
            calls.append(Path(source).name)
            shutil.copyfile(source, target)
        output = self.base / 'download'
        first = self.m.fetch('fixture', str(self.bundle), output, copy=copy)
        self.assertFalse(first['archive_downloaded'])
        self.assertEqual(calls, ['receipt.json', 'summary.json'])
        self.assertFalse((output / 'evidence.tar.gz').exists())
        final = self.m.fetch('fixture', str(self.bundle), output, full=True, copy=copy)
        self.assertTrue(final['archive_downloaded'])
        self.assertEqual(self.m.digest(output / 'evidence.tar.gz'), receipt['archive_sha256'])
        self.assertEqual(final['files_verified'], 2)

    def test_corrupt_or_interrupted_transfer_never_publishes_archive(self):
        self.pack()
        for mode in ('corrupt', 'interrupt'):
            def copy(host, source, target):
                if Path(source).name == 'evidence.tar.gz':
                    Path(target).write_bytes(b'partial')
                    if mode == 'interrupt':
                        raise TimeoutError('interrupted')
                else:
                    shutil.copyfile(source, target)
            output = self.base / mode
            with self.assertRaises((ValueError, TimeoutError)):
                self.m.fetch('fixture', str(self.bundle), output, full=True, copy=copy)
            self.assertFalse((output / 'evidence.tar.gz').exists())
            self.assertTrue((output / 'evidence.tar.gz.partial').exists())

    def test_changed_remote_summary_does_not_overwrite_verified_local_summary(self):
        self.pack()
        copy = lambda host, source, target: shutil.copyfile(source, target)
        output = self.base / 'download'
        self.m.fetch('fixture', str(self.bundle), output, copy=copy)
        before = (output / 'summary.json').read_bytes()
        (self.bundle / 'summary.json').write_text('{}')
        self.m.fetch('fixture', str(self.bundle), output, copy=copy)
        self.assertEqual((output / 'summary.json').read_bytes(), before)

    def test_pack_refuses_links_traversal_duplicates_and_overwrite(self):
        (self.root / 'link').symlink_to(self.root / 'result.json')
        for files in (['link'], ['../escape'], ['result.json', 'result.json']):
            with self.assertRaises(ValueError):
                self.m.pack(self.root, files, {}, self.base / 'invalid')
        self.pack()
        with self.assertRaises(FileExistsError):
            self.pack()

    def test_ssh_destination_validation(self):
        for host, path in (('-oProxyCommand=bad', '/tmp/a'), ('host', 'relative'), ('host;command', '/tmp/a')):
            with self.assertRaises(ValueError):
                self.m.fetch(host, path, self.base / 'bad')

    def test_regular_hardlinked_evidence_is_stored_as_independent_file_members(self):
        os.link(self.root / 'result.json', self.root / 'copy.json')
        receipt = self.m.pack(self.root, ['result.json', 'copy.json'], {}, self.bundle)
        self.assertEqual(self.m.verify(self.bundle / 'evidence.tar.gz', receipt), 3)

    def test_member_verification_rejects_links_and_wrong_manifest_even_with_matching_archive_digest(self):
        receipt = self.pack()
        wrong = dict(receipt, files={**receipt['files'], 'summary.json': '0' * 64})
        with self.assertRaisesRegex(ValueError, 'member checksum'):
            self.m.verify(self.bundle / 'evidence.tar.gz', wrong)
        path = self.base / 'link.tar.gz'
        with tarfile.open(path, 'w:gz') as archive:
            info = tarfile.TarInfo('summary.json')
            info.type, info.linkname = tarfile.SYMTYPE, '/etc/passwd'
            archive.addfile(info)
        receipt['archive_sha256'] = self.m.digest(path)
        with self.assertRaisesRegex(ValueError, 'non-file'):
            self.m.verify(path, receipt)


if __name__ == '__main__':
    unittest.main()
