"""Use real HTTP deadlines/disconnects while the server's capture keeps running."""
import asyncio
import json
import tempfile
import unittest

import httpx

from agentenv_environment import AgentENVEnvironment
import test_pause_resume as lifecycle_tests


class LifecycleHTTPServer:
    def __init__(self):
        self.state = 'running'
        self.pause_started = asyncio.Event()
        self.resume_started = asyncio.Event()
        self.release_capture = asyncio.Event()
        self.release_capture.set()
        self.capture = None
        self.handlers = set()
        self.pauses = 0
        self.resumes = 0
        self.lose_resume_response = False

    async def start(self):
        self.server = await asyncio.start_server(self.handle, '127.0.0.1', 0)
        self.url = 'http://127.0.0.1:' + str(self.server.sockets[0].getsockname()[1])

    async def close(self):
        self.server.close()
        await self.server.wait_closed()
        self.release_capture.set()
        if self.capture:
            await self.capture
        await asyncio.gather(*self.handlers, return_exceptions=True)

    async def capture_memory(self):
        self.state = 'pausing'
        self.pause_started.set()
        await self.release_capture.wait()
        self.state = 'paused'

    async def handle(self, reader, writer):
        handler = asyncio.current_task()
        self.handlers.add(handler)
        try:
            header = (await reader.readuntil(b'\r\n\r\n')).decode()
            method, path, _ = header.splitlines()[0].split()
            headers = dict(line.lower().split(':', 1) for line in header.splitlines()[1:] if ':' in line)
            await reader.readexactly(int(headers.get('content-length', '0')))
            status = '200 OK'
            body = b''
            if method == 'POST' and path.endswith('/pause'):
                self.pauses += 1
                if self.state == 'running':
                    self.capture = asyncio.create_task(self.capture_memory())
                if self.capture:
                    await asyncio.shield(self.capture)
                status = '204 No Content'
            elif method == 'POST' and path.endswith('/resume'):
                self.resumes += 1
                self.resume_started.set()
                if self.state == 'pausing':
                    await asyncio.shield(self.capture)
                self.state = 'running'
                if self.lose_resume_response:
                    self.lose_resume_response = False
                    return  # The VM is running, but no HTTP response reaches the client.
                body = json.dumps({'sandboxID': 'original',
                    'envdAccessToken': 'token-' + str(self.resumes)}).encode()
            elif method == 'GET' and path == '/state':
                body = json.dumps({'state': self.state}).encode()
            else:
                status = '404 Not Found'
            writer.write(f'HTTP/1.1 {status}\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
            await writer.drain()
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except ConnectionError:
                pass
            self.handlers.remove(handler)


class HTTPLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server = LifecycleHTTPServer()
        await self.server.start()
        self.addAsyncCleanup(self.server.close)
        self.client = httpx.AsyncClient(base_url=self.server.url, timeout=2, trust_env=False)
        self.addAsyncCleanup(self.client.aclose)
        self.env = lifecycle_tests.EnvironmentLifecycleTests.environment(self)
        self.env.client = self.client
        self.env.request_timeout_sec = 2
        self.env._request = AgentENVEnvironment._request.__get__(self.env)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.agent = lifecycle_tests.InferenceLifecycleTests.agent(self, directory.name, [])
        self.agent._inference_environment = self.env

    async def test_cancelled_or_timed_out_pause_waits_for_capture_before_recovery(self):
        for cancelled in (True, False):
            with self.subTest(cancelled=cancelled):
                self.server.release_capture.clear()
                self.server.pause_started.clear()
                self.server.resume_started.clear()
                self.env.request_timeout_sec = 2 if cancelled else 0.1
                inferred = False

                async def infer():
                    nonlocal inferred
                    inferred = True

                async with asyncio.timeout(5):
                    task = asyncio.create_task(self.agent._call_with_paused_taskvm(infer))
                    try:
                        await self.server.pause_started.wait()
                        # The pause request already owns its deadline; recovery gets a fresh budget.
                        self.env.request_timeout_sec = 2
                        if cancelled:
                            task.cancel()
                        await self.server.resume_started.wait()
                        self.assertEqual(self.server.state, 'pausing')
                        self.assertFalse(task.done())
                        self.assertFalse(inferred)
                        self.server.release_capture.set()
                        with self.assertRaises(asyncio.CancelledError if cancelled else TimeoutError):
                            await task
                        self.assertEqual(self.server.state, 'running')
                        self.assertFalse(self.env._resume_pending)
                    finally:
                        self.server.release_capture.set()
                        if not task.done():
                            task.cancel()
                        await asyncio.gather(task, return_exceptions=True)

    async def test_resume_disconnect_requires_another_pause_before_inference(self):
        self.server.lose_resume_response = True

        async def infer():
            response = await self.env._request('GET', '/state')
            self.assertEqual(response.json()['state'], 'paused')
            return 'response'

        async with asyncio.timeout(5):
            with self.assertRaises(httpx.RemoteProtocolError):
                await self.agent._call_with_paused_taskvm(infer)
            self.assertEqual(self.server.state, 'running')
            self.assertEqual(await self.agent._call_with_paused_taskvm(infer), 'response')
        self.assertEqual((self.server.pauses, self.server.resumes), (2, 2))
        self.assertEqual(self.env.sandbox['sandboxID'], 'original')
        self.assertEqual(self.env.sandbox['envdAccessToken'], 'token-2')
