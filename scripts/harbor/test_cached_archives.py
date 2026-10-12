"""Cache reuse binds every OCI blob to the original frozen task image."""
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest


class CachedArchiveTests(unittest.TestCase):
    def fixture(self, root, corrupt=False):
        config = b'{"architecture":"amd64","os":"linux"}';layer = b'image layer'
        digest = lambda body: 'sha256:' + hashlib.sha256(body).hexdigest()
        manifest = json.dumps({'schemaVersion': 2, 'config': {'digest': digest(config), 'size': len(config)},
                               'layers': [{'digest': digest(layer), 'size': len(layer)}]}).encode()
        image = {'source_platform_digest': digest(manifest), 'config_digest': digest(config)}
        archive = root / 'image.tar'
        blobs = {'index.json': json.dumps({'manifests': [{'digest': digest(manifest)}]}).encode()}
        for body in (config, layer, manifest):blobs['blobs/sha256/' + digest(body).split(':')[1]] = body
        if corrupt:blobs['blobs/sha256/' + digest(layer).split(':')[1]] = b'wrong layer'
        with tarfile.open(archive, 'w') as out:
            for name, body in blobs.items():
                info = tarfile.TarInfo(name);info.size = len(body);out.addfile(info, io.BytesIO(body))
        record = {'archive': str(archive), 'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
                  'bytes': archive.stat().st_size}
        return image, record

    def test_verified_archive_is_reused_without_an_old_batch_receipt(self):
        from prepare_acr import verify_cached_archive
        with tempfile.TemporaryDirectory() as directory:
            image, record = self.fixture(Path(directory))
            self.assertEqual(verify_cached_archive(record, image), record['archive_sha256'])
            for changed in ({'source_platform_digest': 'sha256:' + 'a' * 64}, {'config_digest': 'sha256:' + 'b' * 64}):
                with self.assertRaises(ValueError):verify_cached_archive(record, {**image, **changed})
            with self.assertRaises(ValueError):verify_cached_archive({**record, 'bytes': 1}, image)
            with self.assertRaises(ValueError):verify_cached_archive({**record, 'archive_sha256': 'a' * 64}, image)

    def test_rehashed_outer_archive_cannot_hide_a_corrupt_oci_blob(self):
        from prepare_acr import verify_cached_archive
        with tempfile.TemporaryDirectory() as directory:
            image, record = self.fixture(Path(directory), corrupt=True)
            with self.assertRaises(ValueError):verify_cached_archive(record, image)

    def test_a_layer_directory_cannot_replace_a_referenced_blob(self):
        from prepare_acr import verify_cached_archive
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);image, record = self.fixture(root)
            archive = Path(record['archive']);replacement = root / 'replacement.tar'
            with tarfile.open(archive) as source, tarfile.open(replacement, 'w') as target:
                for entry in source.getmembers():
                    if entry.name == 'blobs/sha256/' + hashlib.sha256(b'image layer').hexdigest():
                        entry.type = tarfile.DIRTYPE;target.addfile(entry)
                    else:target.addfile(entry, source.extractfile(entry))
            replacement.replace(archive)
            record.update(archive_sha256=hashlib.sha256(archive.read_bytes()).hexdigest(), bytes=archive.stat().st_size)
            with self.assertRaises(ValueError):verify_cached_archive(record, image)
