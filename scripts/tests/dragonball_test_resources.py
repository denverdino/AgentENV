"""Evidence-only receipts for supported DB preparation, regression and M4 runs.

Ownership is recorded for diagnosis, never interpreted as deletion permission.
M4 payload collection continues to require its existing recovery/retention audit.
"""
import os
from pathlib import Path
import stat
import time
import uuid

from dragonball_m4_campaign import file_hash, save
from dragonball_m4_recovery import process_start
from dragonball_m4_retention import PAYLOADS


def owned_directory(path):
    path = Path(path)
    info = path.lstat()
    if (not path.is_absolute() or path != path.resolve()
            or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid()
            or info.st_mode & 0o022):
        raise ValueError('expected an exact owned directory: ' + str(path))
    return {'path': str(path), 'identity': [info.st_dev, info.st_ino], 'uid': info.st_uid}


class RunRecord:
    def __init__(self, evidence, family, *, owned=(), inputs=None):
        self.evidence = Path(evidence)
        evidence_identity = owned_directory(self.evidence)
        resources = [owned_directory(path) for path in owned]
        run_id = uuid.uuid4().hex
        self.path = self.evidence / ('resources-' + run_id + '.json')
        boot = Path('/proc/sys/kernel/random/boot_id')
        self.record = {'schema': 1, 'run_id': run_id, 'family': family,
                       'evidence_root': evidence_identity, 'resources': resources,
                       'controller': {'pid': os.getpid(), 'start_time': process_start(os.getpid()),
                                      'uid': os.geteuid()},
                       'boot_id': boot.read_text().strip() if boot.exists() else None,
                       'inputs': inputs, 'state': 'running', 'started': time.time(),
                       'cleanup': 'unverified', 'evidence': {},
                       'retention': 'retain; no automatic deletion authorization'}

    def __enter__(self):
        save(self.path, self.record)
        return self

    def __exit__(self, kind, error, traceback):
        self.record.update(state='interrupted' if kind and issubclass(kind, KeyboardInterrupt)
                           else 'failed' if kind or self.record.get('exit_code', 0) else 'completed', ended=time.time())
        # Hash only direct regular evidence. State trees and sockets are retained,
        # and no failed run or unrelated directory becomes a GC candidate.
        for path in sorted(self.evidence.iterdir()):
            if path != self.path and path.name not in PAYLOADS and not path.name.startswith('resources-') and path.is_file() and not path.is_symlink():
                self.record['evidence'][path.name] = file_hash(path)
        save(self.path, self.record)
