#!/usr/bin/env python3
"""HTTP/CONNECT egress through a listener inherited from the isolated lab.

The listener belongs to the lab network; outbound connections use the parent's
network. Run as the test user, in the fixture cgroup. No host routes or NAT rules
are changed. This is a test transport, not a general-purpose proxy service.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import select
import socket
import re
from pathlib import Path
from urllib.parse import urlsplit


def benchmark_proxy_env():
    env = {name: 'http://198.18.0.1:3128' for name in
           ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY')}
    local = 'localhost,127.0.0.1,0.0.0.0,::1,10.241.0.11'
    env.update({'no_proxy': local, 'NO_PROXY': local})
    return env


class Handler(BaseHTTPRequestHandler):
    rbufsize = 0
    protocol_version = 'HTTP/1.1'

    def relay(self, remote):
        # Harbor's task deadline and the owned fixture lifecycle bound this
        # connection. An extra total deadline truncates valid long downloads.
        sockets = (self.connection, remote)
        while True:
            ready, _, _ = select.select(sockets, [], [], 5)
            for source in ready:
                data = source.recv(65536)
                if not data:
                    return
                target = remote if source is self.connection else self.connection
                target.sendall(data)

    def forward(self):
        self.close_connection = True
        from download_cache import lookup
        url = self.path
        if self.path.startswith('/harbor-cache/'):
            url = 'http://198.18.0.1:3128' + self.path
        cached = lookup(getattr(self.server, 'download_cache', {}), url, self.headers)
        if cached and self.command in ('GET', 'HEAD'):
            from dependencies import file_digest
            if file_digest(cached['path']) != cached['sha256']:
                self.send_error(502, 'Frozen download checksum mismatch')
                return
            start, end = 0, cached['bytes'] - 1
            partial = 'Range' in self.headers
            if partial:
                match = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers['Range'])
                if match:
                    start = int(match[1])
                    end = min(int(match[2]) if match[2] else end, end)
                if not match or not 0 <= start <= end < cached['bytes']:
                    self.send_response(416)
                    self.send_header('Content-Range', 'bytes */' + str(cached['bytes']))
                    self.send_header('Content-Length', '0')
                    self.end_headers()
                    return
            self.send_response(206 if partial else 200)
            self.send_header('Content-Length', str(end - start + 1))
            self.send_header('Accept-Ranges', 'bytes')
            if partial:
                self.send_header('Content-Range', f'bytes {start}-{end}/{cached["bytes"]}')
            self.send_header('Content-Type', 'application/octet-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            if self.command == 'GET':
                with Path(cached['path']).open('rb') as source:
                    source.seek(start)
                    remaining = end - start + 1
                    while remaining and (chunk := source.read(min(65536, remaining))):
                        self.wfile.write(chunk)
                        remaining -= len(chunk)
            return
        target = urlsplit('//' + self.path if self.command == 'CONNECT' else self.path)
        try:
            if not target.hostname or (self.command != 'CONNECT' and target.scheme != 'http'):
                self.send_error(400)
                return
            with socket.create_connection((target.hostname, target.port or 80), timeout=30) as remote:
                self.connection.settimeout(30)
                if self.command == 'CONNECT':
                    self.send_response(200)
                    self.end_headers()
                else:
                    path = target.path or '/'
                    if target.query:
                        path += '?' + target.query
                    headers = ''.join(f'{name}: {value}\r\n' for name, value in self.headers.items()
                                      if name.lower() not in ('connection', 'proxy-connection', 'proxy-authorization'))
                    remote.sendall((f'{self.command} {path} HTTP/1.1\r\n' + headers +
                                    'Connection: close\r\n\r\n').encode('latin-1'))
                self.relay(remote)
        except (OSError, ValueError):
            # Never log payloads or authorization headers.
            self.close_connection = True

    do_CONNECT = do_GET = do_POST = do_HEAD = forward

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--listen-fd', type=int, required=True)
    parser.add_argument('--download-cache', type=Path)
    args = parser.parse_args()
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler, bind_and_activate=False)
    server.socket.close()
    server.socket = socket.socket(fileno=args.listen_fd)
    server.server_address = server.socket.getsockname()
    if args.download_cache:
        from download_cache import load_cache
        server.download_cache = load_cache(args.download_cache)
    server.serve_forever()


if __name__ == '__main__':
    main()
