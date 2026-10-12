"""A real HTTP origin verifies HTTP forwarding and CONNECT byte fidelity."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import os
import socket
import subprocess
import sys
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import patch
import urllib.request

import egress_proxy


class Origin(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'origin-pong'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class EgressTests(unittest.TestCase):
    def test_guest_local_service_bypasses_external_proxy(self):
        self.assertTrue(hasattr(egress_proxy, 'benchmark_proxy_env'), 'Guest proxy configuration missing')
        with ThreadingHTTPServer(('0.0.0.0', 0), Origin) as origin, \
                ThreadingHTTPServer(('127.0.0.1', 0), Origin) as proxy:
            proxy_requests = []
            original = proxy.RequestHandlerClass
            class Sentinel(original):
                def do_GET(self):
                    proxy_requests.append(self.path)
                    self.send_error(502, 'Local request reached external proxy')
            proxy.RequestHandlerClass = Sentinel
            for server in (origin, proxy):
                threading.Thread(target=server.serve_forever, daemon=True).start()
            env = egress_proxy.benchmark_proxy_env()
            for key in ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY'):
                env[key] = 'http://127.0.0.1:' + str(proxy.server_port)
            try:
                with patch.dict(os.environ, env, clear=True):
                    opener = urllib.request.build_opener()
                    for host in ('0.0.0.0', 'localhost', '127.0.0.1'):
                        try:
                            with opener.open('http://' + host + ':' + str(origin.server_port), timeout=3) as response:
                                self.assertEqual(response.read(), b'origin-pong')
                        except urllib.error.HTTPError as error:
                            self.fail(f'Guest local request reached proxy: {error}')
                self.assertEqual(proxy_requests, [])
            finally:
                origin.shutdown()
                proxy.shutdown()

    def test_active_connection_survives_previous_absolute_budget(self):
        client, incoming = socket.socketpair()
        remote, origin = socket.socketpair()
        for sock in (client, incoming, remote, origin):
            sock.settimeout(2)
        handler = object.__new__(egress_proxy.Handler)
        handler.connection = incoming
        errors = []
        def relay():
            try:
                handler.relay(remote)
            except OSError as error:
                errors.append(error)
            finally:
                incoming.close()
                remote.close()
        ticks = iter((0, 0, 0, 200, 200))
        clock = SimpleNamespace(monotonic=lambda: next(ticks, 200))
        thread = threading.Thread(target=relay, daemon=True)
        try:
            with patch.object(egress_proxy, 'time', clock, create=True):
                thread.start()
                client.sendall(b'first-chunk')
                self.assertEqual(origin.recv(1024), b'first-chunk')
                try:
                    origin.sendall(b'next-chunk')
                    response = client.recv(1024)
                except OSError:
                    response = b''
                self.assertEqual(response, b'next-chunk')
                client.shutdown(socket.SHUT_WR)
                thread.join(timeout=3)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
        finally:
            for sock in (client, incoming, remote, origin):
                sock.close()

    def test_http_and_connect_use_inherited_listener(self):
        script = Path(__file__).with_name('egress_proxy.py')
        self.assertTrue(script.exists(), 'Namespace egress proxy is missing')
        with ThreadingHTTPServer(('127.0.0.1', 0), Origin) as origin, socket.socket() as listener:
            threading.Thread(target=origin.serve_forever, daemon=True).start()
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            child = subprocess.Popen([sys.executable, str(script), '--listen-fd', str(listener.fileno())],
                                     pass_fds=(listener.fileno(),), stderr=subprocess.PIPE)
            try:
                host = '127.0.0.1:' + str(origin.server_port)
                with socket.create_connection(listener.getsockname(), timeout=5) as connection:
                    connection.sendall(('GET http://' + host + '/ HTTP/1.1\r\nHost: ' + host + '\r\n\r\n').encode())
                    response = b''
                    while data := connection.recv(4096):
                        response += data
                    self.assertIn(b'200 OK', response)
                    self.assertTrue(response.endswith(b'origin-pong'))
                with socket.create_connection(listener.getsockname(), timeout=5) as connection:
                    connection.sendall(('CONNECT ' + host + ' HTTP/1.1\r\nHost: ' + host + '\r\n\r\n').encode())
                    response = b''
                    while not response.endswith(b'\r\n\r\n'):
                        response += connection.recv(1)
                    self.assertIn(b'200', response)
                    connection.sendall(('GET / HTTP/1.1\r\nHost: ' + host + '\r\n\r\n').encode())
                    response = b''
                    while data := connection.recv(4096):
                        response += data
                    self.assertTrue(response.endswith(b'origin-pong'))
            finally:
                child.terminate()
                child.communicate(timeout=5)
                origin.shutdown()


if __name__ == '__main__':
    unittest.main()
