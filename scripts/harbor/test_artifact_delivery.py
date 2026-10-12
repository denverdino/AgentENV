"""A partial transport can never expose a usable final artifact."""
import hashlib
import os
from pathlib import Path
import tempfile
import threading
import unittest


class ArtifactDeliveryTests(unittest.TestCase):
    def test_truncated_corrupt_and_missing_inputs_leave_no_final_file(self):
        from artifact_delivery import publish_verified
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);partial = root / 'image.part';final = root / 'image.tar'
            digest = hashlib.sha256(b'complete').hexdigest()
            for data in (None, b'comp', b'corrupt!'):
                if data is not None:partial.write_bytes(data)
                with self.assertRaises((OSError, ValueError)):
                    publish_verified(partial, final, digest, 8)
                self.assertFalse(final.exists())

    def test_success_is_an_immutable_copy_and_never_overwrites_existing_artifacts(self):
        from artifact_delivery import publish_verified
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);partial = root / 'image.part';final = root / 'image.tar'
            partial.write_bytes(b'complete');digest = hashlib.sha256(b'complete').hexdigest()
            receipt = publish_verified(partial, final, digest, 8)
            self.assertEqual(receipt['sha256'], digest)
            partial.write_bytes(b'later producer writes')
            self.assertEqual(final.read_bytes(), b'complete')
            partial.write_bytes(b'complete')
            with self.assertRaises(FileExistsError):publish_verified(partial, final, digest, 8)
            self.assertEqual(final.read_bytes(), b'complete')

    def test_slow_producer_is_rejected_until_all_expected_bytes_arrive(self):
        from artifact_delivery import publish_verified
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);partial = root / 'image.part';final = root / 'image.tar'
            started = threading.Event();finish = threading.Event()
            def produce():
                with partial.open('wb') as stream:
                    stream.write(b'comp');stream.flush();started.set()
                    finish.wait(5);stream.write(b'lete')
            thread = threading.Thread(target=produce);thread.start()
            try:
                self.assertTrue(started.wait(5))
                with self.assertRaises(ValueError):
                    publish_verified(partial, final, hashlib.sha256(b'complete').hexdigest(), 8)
                self.assertFalse(final.exists())
            finally:
                finish.set();thread.join(5)
            publish_verified(partial, final, hashlib.sha256(b'complete').hexdigest(), 8)
            self.assertEqual(final.read_bytes(), b'complete')

    def test_symlinks_and_special_files_are_rejected_without_blocking(self):
        from artifact_delivery import publish_verified
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);partial = root / 'image.part';final = root / 'image.tar'
            source = root / 'source';source.write_bytes(b'x');partial.symlink_to(source)
            with self.assertRaises((OSError, ValueError)):publish_verified(partial, final, 'a' * 64, 1)
            partial.unlink();os.mkfifo(partial)
            with self.assertRaises(ValueError):publish_verified(partial, final, 'a' * 64, 1)
            self.assertFalse(final.exists())
