"""Frozen public downloads remain byte-identical across batches."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from http.server import ThreadingHTTPServer
import http.client
import threading
from unittest.mock import patch
import io


class DownloadCacheTests(unittest.TestCase):
    def test_preparation_reuses_verified_payload_without_network(self):
        import prepare_downloads
        data = b'published archive'
        digest = hashlib.sha256(data).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            with patch('urllib.request.urlopen', return_value=io.BytesIO(data)) as request:
                first = prepare_downloads.fetch('https://example.test/archive', directory, expected=digest)
                self.assertEqual(request.call_count, 1)
            with patch('urllib.request.urlopen', side_effect=AssertionError('network on cache hit')):
                second = prepare_downloads.fetch('https://example.test/archive', directory, expected=digest)
            self.assertFalse(first['cache_hit'])
            self.assertTrue(second['cache_hit'])
            self.assertEqual(Path(first['path']).stat().st_mode & 0o777, 0o444)

    def test_partial_download_is_not_published(self):
        import prepare_downloads
        with tempfile.TemporaryDirectory() as directory:
            with patch('urllib.request.urlopen', return_value=io.BytesIO(b'partial archive')):
                with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                    prepare_downloads.fetch('https://example.test/archive', directory, expected='a' * 64)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_proxy_serves_frozen_bytes_and_resume_without_contacting_origin(self):
        import download_cache
        import egress_proxy
        with tempfile.TemporaryDirectory() as directory:
            path, payload, row = self.fixture(Path(directory))
            cache = download_cache.load_cache(path);url = next(iter(cache))
            with ThreadingHTTPServer(('127.0.0.1', 0), egress_proxy.Handler) as server:
                server.download_cache = cache
                thread = threading.Thread(target=server.serve_forever, daemon=True);thread.start()
                try:
                    for method, headers, status, body in [('GET', {}, 200, b'fixed dataset'),
                            ('HEAD', {}, 200, b''), ('GET', {'Range': 'bytes=6-'}, 206, b'dataset'),
                            ('GET', {'Range': 'bytes=999-'}, 416, b'')]:
                        connection = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                        try:
                            connection.request(method, url, headers=headers)
                            response = connection.getresponse()
                            self.assertEqual(response.status, status)
                            self.assertEqual(response.read(), body)
                        finally:
                            connection.close()
                    payload.write_bytes(b'other dataset')
                    connection = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                    try:
                        connection.request('GET', url)
                        response = connection.getresponse()
                        self.assertEqual(response.status, 502)
                        response.read()
                    finally:
                        connection.close()
                finally:
                    server.shutdown()

    def fixture(self, root):
        payload = root / 'payload';payload.write_bytes(b'fixed dataset')
        row = {'sha256': hashlib.sha256(payload.read_bytes()).hexdigest(),
               'bytes': payload.stat().st_size, 'path': str(payload),
               'task_name': 'caffe-cifar-10', 'task_sha256': 'a' * 64,
               'image_digest': 'sha256:' + 'b' * 64}
        path = root / 'downloads.json'
        path.write_text(json.dumps({'schema': 1, 'downloads': {
            'http://www.cs.toronto.edu/~kriz/cifar-10-binary.tar.gz': row}}))
        return path, payload, row

    def test_corruption_is_rejected_before_proxy_start(self):
        import download_cache
        with tempfile.TemporaryDirectory() as directory:
            path, payload, row = self.fixture(Path(directory))
            self.assertEqual(len(download_cache.load_cache(path)), 1)
            payload.write_bytes(b'changed dataset')
            with self.assertRaises(ValueError):
                download_cache.load_cache(path)

    def test_task_and_image_binding_are_required(self):
        import download_cache
        with tempfile.TemporaryDirectory() as directory:
            path, payload, row = self.fixture(Path(directory))
            entries = download_cache.load_cache(path)
            download_cache.validate_bindings(entries, {'caffe-cifar-10': {
                'task_sha256': row['task_sha256'], 'image_digest': row['image_digest']}})
            with self.assertRaises(ValueError):
                download_cache.validate_bindings(entries, {'caffe-cifar-10': {
                    'task_sha256': 'c' * 64, 'image_digest': row['image_digest']}})

    def test_authenticated_or_unprepared_requests_are_not_cache_hits(self):
        import download_cache
        with tempfile.TemporaryDirectory() as directory:
            path, payload, row = self.fixture(Path(directory))
            cache = download_cache.load_cache(path);url = next(iter(cache))
            self.assertIsNone(download_cache.lookup(cache, url, {'Authorization': 'secret'}))
            self.assertIsNone(download_cache.lookup(cache, url + '?different', {}))
            self.assertEqual(download_cache.lookup(cache, url, {})['sha256'], row['sha256'])


if __name__ == '__main__':
    unittest.main()
