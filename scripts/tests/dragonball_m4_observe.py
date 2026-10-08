"""Optional test observations: host-clock guest liveness, S3 IP bytes, memory.

Liveness gaps include network/host scheduling and are not exact VMM pause time.
Wire counters include TCP/IP headers and retransmissions, not logical object size.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import threading
import time
import uuid


def heartbeat_gaps(rows, start, end):
    if start > end or len(rows) < 2 or rows[0][0] >= start or rows[-1][0] <= end:
        raise ValueError('heartbeat does not bracket action')
    if any(b[1] != a[1] + 1 or b[0] < a[0] for a, b in zip(rows, rows[1:])):
        raise ValueError('heartbeat packet loss, reorder or duplicate invalidates gap')
    gaps = [b[0] - a[0] for a, b in zip(rows, rows[1:]) if a[0] <= end and b[0] >= start]
    baseline = [b[0] - a[0] for a, b in zip(rows, rows[1:]) if b[0] < start]
    return {'max_gap_seconds': max(gaps), 'baseline_max_gap_seconds': max(baseline, default=None),
            'lost_packets': 0, 'received_packets': len(rows), 'interval_seconds': .005,
            'scope': 'host_receive_gap_includes_guest_host_scheduling_and_network'}


class HeartbeatCollector:
    def __init__(self, address):
        self.token = uuid.uuid4().hex
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.bind((address, 0))
        self.socket.settimeout(.05)
        self.port = self.socket.getsockname()[1]
        self.rows = []
        self.error = None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._receive, daemon=True)
        self.thread.start()

    def _receive(self):
        try:
            while not self.stop.is_set():
                try:
                    data, _ = self.socket.recvfrom(128)
                except socket.timeout:
                    continue
                now = time.monotonic()
                # Old restored probes have different tokens and cannot join this stream.
                token, _, value = data.decode('ascii').partition(' ')
                if token == self.token:
                    self.rows.append((now, int(value)))
        except Exception as error:
            self.error = error

    def wait_samples(self, count, timeout):
        deadline = time.monotonic() + timeout
        while len(self.rows) < count:
            if self.error:
                raise RuntimeError('heartbeat receiver failed') from self.error
            if time.monotonic() >= deadline:
                raise TimeoutError('guest heartbeat not ready')
            time.sleep(.005)

    def result(self, start, end):
        if self.error:
            raise RuntimeError('heartbeat receiver failed') from self.error
        return {**heartbeat_gaps(list(self.rows), start, end),
                'host_monotonic_samples': list(self.rows), 'action_start': start, 'action_end': end}

    def close(self):
        self.stop.set()
        self.thread.join(timeout=1)
        self.socket.close()
        if self.thread.is_alive():
            raise RuntimeError('heartbeat receiver did not stop')


class MemorySampler:
    def __init__(self, path, interval=.01):
        self.path, self.interval = Path(path), interval
        self.values, self.error = [], None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._sample, daemon=True)
        self.thread.start()

    def _sample(self):
        try:
            while True:
                self.values.append((time.monotonic(), int(self.path.read_text())))
                if self.stop.wait(self.interval):
                    break
        except Exception as error:
            self.error = error

    def finish(self):
        self.stop.set(); self.thread.join(timeout=1)
        if self.thread.is_alive() or self.error or not self.values:
            raise RuntimeError('memory sampler failed: ' + str(self.error))
        return {'observed_peak_bytes': max(value for _, value in self.values),
                'sample_count': len(self.values), 'interval_seconds': self.interval,
                'scope': 'sampled_node_memory_current_lower_bound_on_true_peak'}


def wire_values(document):
    values = {row['counter']['name']: row['counter']['bytes'] for row in document['nftables']
              if 'counter' in row and row['counter'].get('name') in ('s3_upload', 's3_download')}
    if set(values) != {'s3_upload', 's3_download'} or any(type(v) is not int or v < 0 for v in values.values()):
        raise ValueError('S3 wire counters missing or invalid')
    return values


def wire_delta(before, after):
    delta = {key: after[key] - before[key] for key in before}
    if any(value < 0 for value in delta.values()):
        raise ValueError('S3 wire counter reset')
    return delta


class WireCounters:
    def __init__(self):
        parent = os.environ.get('AENV_M4_PARENT_NET')
        if not parent or parent == os.readlink('/proc/self/ns/net'):
            raise RuntimeError('S3 wire counters require private Lab network namespace')
        self.name = 'm4_observe_' + uuid.uuid4().hex[:12]
        script = f'''table inet {self.name} {{
 counter s3_upload {{ }}
 counter s3_download {{ }}
 chain input {{ type filter hook input priority 0; policy accept;
  ip daddr 10.241.0.1 tcp dport 9000 counter name s3_upload
 }}
 chain output {{ type filter hook output priority 0; policy accept;
  ip saddr 10.241.0.1 tcp sport 9000 counter name s3_download
 }}
}}
'''
        subprocess.run(['nft', '-f', '-'], input=script, text=True, check=True, capture_output=True, timeout=10)

    def read(self):
        raw = subprocess.check_output(['nft', '-j', 'list', 'table', 'inet', self.name], text=True, timeout=10)
        return wire_values(json.loads(raw))

    def close(self):
        subprocess.run(['nft', 'delete', 'table', 'inet', self.name], check=True, capture_output=True, timeout=10)
