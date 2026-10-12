"""External task inputs are verified before spending model budget."""
import asyncio
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shlex
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest

import material_inputs
from dependencies import task_digest


class MaterialInputTests(unittest.TestCase):
    @contextmanager
    def server(self, payload=b'original public data', status=200, delay=0):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(status)
                self.end_headers()
                time.sleep(delay)
                try:
                    self.wfile.write(payload)
                except BrokenPipeError:
                    pass

            def log_message(self, *args):
                pass

        with ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                yield 'http://127.0.0.1:' + str(server.server_port) + '/input'
            finally:
                server.shutdown()
                thread.join()

    def http_row(self, url):
        return {'id': 'data', 'source_id': 'fixture:original', 'kind': 'http', 'url': url,
                'sha256': hashlib.sha256(b'original public data').hexdigest(),
                'bytes': 20, 'max_bytes': 32, 'timeout_sec': 2}

    def manifest(self, root, *, name='fixture-task', row=None):
        task = root / name
        task.mkdir(exist_ok=True)
        (task / 'instruction.md').write_text('Use the original public input.')
        document = {'schema': 1, 'harbor_revision': material_inputs.HARBOR_REVISION,
                    'task_revision': material_inputs.TASK_REVISION, 'tasks': {name: {
                        'task_sha256': task_digest(task), 'inputs': [row or {
                            'id': 'input', 'source_id': 'fixture:original', 'kind': 'unchecked',
                            'url': 'https://example.test/input', 'reason': 'No extractor'}]}}}
        path = root / 'materials.json'
        path.write_text(json.dumps(document))
        return path, task, document

    def test_known_required_task_cannot_run_without_explicit_preflight(self):
        with tempfile.TemporaryDirectory() as directory:
            task = Path(directory) / 'extract-moves-from-video'
            task.mkdir()
            (task / 'instruction.md').write_text('Download the specified Zork video.')
            manifest = material_inputs.load_manifest(None, {task.name: task})
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name))
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['status'], 'missing')

    def test_task_source_and_pinned_revisions_must_match(self):
        with tempfile.TemporaryDirectory() as directory:
            path, task, document = self.manifest(Path(directory))
            material_inputs.load_manifest(path, {task.name: task})
            for field in ('harbor_revision', 'task_revision'):
                changed = {**document, field: 'changed'}
                path.write_text(json.dumps(changed))
                with self.assertRaisesRegex(ValueError, 'revision'):
                    material_inputs.load_manifest(path, {task.name: task})
            path.write_text(json.dumps(document))
            (task / 'instruction.md').write_text('Different real source.')
            with self.assertRaisesRegex(ValueError, 'task source'):
                material_inputs.load_manifest(path, {task.name: task})

    def test_youtube_page_is_unchecked_and_guessed_repository_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, task, document = self.manifest(root, name='extract-moves-from-video', row={
                'id': 'zork-video', 'kind': 'unchecked', 'source_id': 'youtube:ZCbvyPbhRfA',
                'url': 'https://www.youtube.com/watch?v=ZCbvyPbhRfA', 'reason': 'No media extractor'})
            manifest = material_inputs.load_manifest(path, {task.name: task})
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name))
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['status'], 'unchecked')
            row = document['tasks'][task.name]['inputs'][0]
            row.update(kind='http', sha256='a' * 64, bytes=10, max_bytes=100, timeout_sec=2)
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(ValueError, 'media'):
                material_inputs.load_manifest(path, {task.name: task})
            path, task, document = self.manifest(root, name='mteb-leaderboard', row={
                'id': 'results', 'kind': 'unchecked', 'source_id': 'huggingface:guessed-repository',
                'url': 'https://huggingface.co/guessed/repository', 'reason': 'unauthorized'})
            with self.assertRaisesRegex(ValueError, 'source identity'):
                material_inputs.load_manifest(path, {task.name: task})

    def test_http_body_is_retrieved_and_bound_instead_of_only_checking_status(self):
        with tempfile.TemporaryDirectory() as directory, self.server() as url:
            path, task, _ = self.manifest(Path(directory), row=self.http_row(url))
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                material_inputs.load_manifest(path, {task.name: task}), task.name))
            self.assertTrue(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['bytes'], 20)
            self.assertEqual(receipt['inputs'][0]['status'], 'passed')

    def test_http_unauthorized_forbidden_missing_changed_and_oversized_fail_closed(self):
        for status, payload, reason in [(401, b'unauthorized', 'HTTP 401'),
                (403, b'forbidden', 'HTTP 403'), (404, b'missing', 'HTTP 404'),
                (200, b'<html>login</html>', 'digest'), (200, b'x' * 33, 'bound')]:
            with self.subTest(status=status, payload=payload), \
                    tempfile.TemporaryDirectory() as directory, self.server(payload, status) as url:
                path, task, _ = self.manifest(Path(directory), row=self.http_row(url))
                receipt = asyncio.run(material_inputs.probe_material_inputs(
                    material_inputs.load_manifest(path, {task.name: task}), task.name))
                self.assertFalse(receipt['passed'])
                self.assertEqual(receipt['inputs'][0]['status'], 'failed')
                self.assertIn(reason, receipt['inputs'][0]['reason'])

    def test_cache_hit_rehashes_content_and_missing_or_corrupt_cache_never_falls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = root / 'public-input'
            payload.write_bytes(b'original public data')
            row = {**self.http_row('https://example.test/input'),
                   'kind': 'cache', 'cache_path': 'public-input'}
            path, task, _ = self.manifest(root, row=row)
            manifest = material_inputs.load_manifest(path, {task.name: task})
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name))
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['status'], 'cache_verified')
            self.assertEqual(receipt['inputs'][0]['availability'], 'unchecked')
            self.assertEqual(receipt['inputs'][0]['scope'], 'host-cache')
            payload.write_bytes(b'changed public data!')
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name))
            self.assertFalse(receipt['passed'])
            self.assertIn('digest', receipt['inputs'][0]['reason'])
            payload.unlink()
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name))
            self.assertFalse(receipt['passed'])

    def test_youtube_cached_login_page_with_matching_digest_is_not_video(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = root / 'video.mp4'
            payload.write_bytes(b'<html>Sign in to confirm not a bot</html>')
            row = {'id': 'video', 'source_id': 'youtube:ZCbvyPbhRfA', 'kind': 'cache',
                   'url': 'https://www.youtube.com/watch?v=ZCbvyPbhRfA', 'cache_path': 'video.mp4',
                   'format': 'mp4', 'sha256': hashlib.sha256(payload.read_bytes()).hexdigest(),
                   'bytes': payload.stat().st_size, 'max_bytes': 100, 'timeout_sec': 2}
            path, task, _ = self.manifest(root, name='extract-moves-from-video', row=row)
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                material_inputs.load_manifest(path, {task.name: task}), task.name))
            self.assertFalse(receipt['passed'])
            self.assertIn('media', receipt['inputs'][0]['reason'])

    def test_guest_transport_failure_is_a_task_receipt_and_zero_exit_is_insufficient(self):
        async def broken_guest(command, **kwargs):
            raise RuntimeError('transport failed')

        async def false_success(command, **kwargs):
            return SimpleNamespace(return_code=0, stdout='200', stderr='')

        with tempfile.TemporaryDirectory() as directory:
            path, task, _ = self.manifest(Path(directory), row=self.http_row('https://example.test/input'))
            manifest = material_inputs.load_manifest(path, {task.name: task})
            for adapter in (broken_guest, false_success):
                receipt = asyncio.run(material_inputs.probe_material_inputs(
                    manifest, task.name, exec_guest=adapter))
                self.assertFalse(receipt['passed'])
                self.assertEqual(receipt['inputs'][0]['status'], 'failed')

    def test_guest_adapter_executes_real_fetch_script_and_rejects_fabricated_content(self):
        async def local_guest(command, **kwargs):
            script = shlex.split(command)[2]
            compile(script, '<material-guest-probe>', 'exec')
            process = await asyncio.create_subprocess_exec(sys.executable, '-B', '-c', script,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, stderr = await process.communicate()
            return SimpleNamespace(return_code=process.returncode,
                                   stdout=stdout.decode(), stderr=stderr.decode())

        with tempfile.TemporaryDirectory() as directory, self.server() as url:
            path, task, _ = self.manifest(Path(directory), row=self.http_row(url))
            manifest = material_inputs.load_manifest(path, {task.name: task})
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                manifest, task.name, exec_guest=local_guest))
            self.assertTrue(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['scope'], 'guest')
            for output in ('{}', '[]', json.dumps({'status': 'passed',
                    'source_id': 'fixture:original', 'bytes': 20, 'sha256': 'a' * 64})):
                async def fabricated_guest(command, **kwargs):
                    return SimpleNamespace(return_code=0, stdout=output, stderr='')
                receipt = asyncio.run(material_inputs.probe_material_inputs(
                    manifest, task.name, exec_guest=fabricated_guest))
                self.assertFalse(receipt['passed'])

    def test_guest_material_deadline_is_bounded(self):
        async def stalled_guest(command, **kwargs):
            await asyncio.sleep(10)

        with tempfile.TemporaryDirectory() as directory:
            row = {**self.http_row('https://example.test/input'), 'timeout_sec': 0.01}
            path, task, _ = self.manifest(Path(directory), row=row)
            before = time.monotonic()
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                material_inputs.load_manifest(path, {task.name: task}), task.name, exec_guest=stalled_guest))
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['reason'], 'TimeoutError')
            self.assertLess(time.monotonic() - before, 3)

    def test_existing_download_cache_reuse_validates_binding_and_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = root / 'input'
            payload.write_bytes(b'original public data')
            row = self.http_row('http://example.test/input')
            path, task, document = self.manifest(root, row=row)
            manifest = material_inputs.load_manifest(path, {task.name: task})
            cached = {'task_name': task.name, 'task_sha256': document['tasks'][task.name]['task_sha256'],
                      'sha256': row['sha256'], 'bytes': row['bytes'], 'path': str(payload)}
            cache = {row['url']: cached}
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name, cache=cache))
            self.assertEqual(receipt['inputs'][0]['status'], 'cache_verified')
            self.assertFalse(receipt['passed'])
            cached['task_sha256'] = '0' * 64
            receipt = asyncio.run(material_inputs.probe_material_inputs(manifest, task.name, cache=cache))
            self.assertIn('binding', receipt['inputs'][0]['reason'])

    def test_unselected_task_cannot_receive_vacuous_admission(self):
        manifest = material_inputs.load_manifest(None, {})
        with self.assertRaisesRegex(ValueError, 'selected'):
            asyncio.run(material_inputs.probe_material_inputs(manifest, 'unselected-task'))

    def test_http_slow_body_hits_deadline_instead_of_hanging_model_admission(self):
        with tempfile.TemporaryDirectory() as directory, self.server(delay=0.2) as url:
            row = {**self.http_row(url), 'timeout_sec': 0.05}
            path, task, _ = self.manifest(Path(directory), row=row)
            before = time.monotonic()
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                material_inputs.load_manifest(path, {task.name: task}), task.name))
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['inputs'][0]['status'], 'failed')
            self.assertLess(time.monotonic() - before, 3)

    def test_cache_symlink_is_not_followed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'actual').write_bytes(b'original public data')
            (root / 'input').symlink_to('actual')
            row = {**self.http_row('https://example.test/input'), 'kind': 'cache', 'cache_path': 'input'}
            path, task, _ = self.manifest(root, row=row)
            receipt = asyncio.run(material_inputs.probe_material_inputs(
                material_inputs.load_manifest(path, {task.name: task}), task.name))
            self.assertFalse(receipt['passed'])
            self.assertIn('symlink', receipt['inputs'][0]['reason'])


if __name__ == '__main__':
    unittest.main()
