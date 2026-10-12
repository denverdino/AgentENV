"""Bounded failure evidence; rewards remain owned by the pinned verifier."""
import asyncio
import json
import logging
import os
from pathlib import Path
import re
import time

from harbor.verifier.verifier import Verifier

CAPTURE_TIMEOUT_SEC = 15
TAIL_BYTES = 256 * 1024


def _redact(text, environment):
    # Read only known host credentials for masking; never dump guest/host env.
    secrets = [os.environ.get(name) for name in (
        'OPENAI_API_KEY', 'DASHSCOPE_API_KEY', 'AENV_API_KEY',
        'HARBOR_OSS_ACCESS_KEY_SECRET', 'HARBOR_OSS_SECURITY_TOKEN',
        'AWS_SECRET_ACCESS_KEY', 'AWS_SESSION_TOKEN')]
    secrets.append((getattr(environment, 'sandbox', None) or {}).get('envdAccessToken'))
    headers = getattr(getattr(environment, 'client', None), 'headers', {})
    secrets.extend(headers.get(name) for name in ('X-API-Key', 'X-Access-Token', 'Authorization'))
    for secret in sorted({value for value in secrets if value}, key=len, reverse=True):
        text = text.replace(secret, '[REDACTED]')
    text = re.sub(r'(?im)(\bauthorization["\']?\s*[:=]\s*["\']?(?:(?:bearer|basic)\s+)?)[^\s"\',;}]+',
                  r'\1[REDACTED]', text)
    key = r'\b[\w-]*(?:api[_-]?key|access[_-]?key[_-]?(?:secret|id)|secret[_-]?access[_-]?key|password|passwd|token|secret|signature)[\w-]*["\']?\s*[:=]\s*'
    text = re.sub(r'(?i)(' + key + r')(["\'])(.*?)\2', r'\1\2[REDACTED]\2', text)
    text = re.sub(r'(?i)(' + key + r')(?!["\'])[^\s"\',;}]+', r'\1[REDACTED]', text)
    text = re.sub(r'(?<=//)[^/\s:@]+:[^/@\s]+@', '[REDACTED]@', text)
    return re.sub(r'\bsk-[A-Za-z0-9_-]+', '[REDACTED]', text)


async def collect_trial_failure(environment):
    """Pinned Harbor writes exception.txt before stop, result.json after stop."""
    paths = getattr(environment, 'trial_paths', None)
    if paths is None:
        return
    root = Path(paths.trial_dir)
    if (root / 'failure-evidence/receipt.json').exists():
        return
    path = getattr(paths, 'exception_message_path', root / 'exception.txt')
    try:
        with Path(path).open('rb') as stream:
            stream.seek(0, 2)
            stream.seek(max(0, stream.tell() - TAIL_BYTES))
            traceback_tail = stream.read(TAIL_BYTES).decode('utf-8', errors='replace')
    except FileNotFoundError:
        return
    if not traceback_tail.strip():
        return
    names = re.findall(r'^(?:\w+\.)*([A-Za-z_]\w*(?:Error|Exception))(?::|$)',
                       traceback_tail, re.MULTILINE)
    await collect_failure(environment, root, None,
                          primary_error_type=names[-1] if names else 'TrialError')


async def collect_failure(environment, trial_dir, error, *, primary_error_type=None):
    output = Path(trial_dir) / 'failure-evidence'
    output.mkdir(parents=True, exist_ok=True)
    receipt = {'status': 'pending', 'started': time.time(),
               'primary_error_type': primary_error_type or type(error).__name__, 'budget_sec': CAPTURE_TIMEOUT_SEC,
               'sandbox_id': (getattr(environment, 'sandbox', None) or {}).get('sandboxID')}
    path = output / 'receipt.json'
    path.write_text(json.dumps(receipt, indent=2))
    try:
        async with asyncio.timeout(CAPTURE_TIMEOUT_SEC):
            commands = {
                'resources.txt': 'df -Pk /; cat /proc/meminfo; ps -eo pid,ppid,stat,etimes,rss,comm; '
                    'for f in /sys/fs/cgroup/memory.events /sys/fs/cgroup/memory.current; do '
                    'if test -r "$f"; then cat "$f"; fi; done',
                'dmesg.txt': f'set -o pipefail; dmesg 2>&1 | tail -c {TAIL_BYTES}',
                'verifier-tail.txt': f'set -o pipefail; for f in /logs/verifier/test-stdout.txt /logs/verifier/test-stderr.txt; do '
                    f'if test -f "$f" && ! test -L "$f"; then tail -c {TAIL_BYTES // 2} -- "$f"; fi; done | tail -c {TAIL_BYTES}',
                'opam-coq-tail.txt': f'if test -f /tmp/opam_coq.log && ! test -L /tmp/opam_coq.log; then tail -c {TAIL_BYTES} /tmp/opam_coq.log; fi',
            }
            for name, command in commands.items():
                try:
                    async with asyncio.timeout(3):
                        result = await environment.exec(command, cwd='/', user='root', timeout_sec=2)
                    clean = _redact(result.stdout, environment).encode('utf-8')[-TAIL_BYTES:]
                    (output / name).write_text(clean.decode('utf-8', errors='ignore'))
                    receipt.setdefault('commands', {})[name] = {'return_code': result.return_code}
                except Exception as capture_error:
                    receipt.setdefault('commands', {})[name] = {'error_type': type(capture_error).__name__}
            rows = receipt['commands'].values()
            receipt['status'] = ('passed' if all(row.get('return_code') == 0 for row in rows)
                                 else 'partial' if any('return_code' in row for row in rows) else 'failed')
    except BaseException as capture_error:
        receipt.update(status='failed', capture_error_type=type(capture_error).__name__)
        if isinstance(capture_error, asyncio.CancelledError):
            raise
    finally:
        receipt['finished'] = time.time()
        path.write_text(json.dumps(receipt, indent=2))


class EvidenceVerifier(Verifier):
    def __init__(self, *args, downloads_path=None, **kwargs):
        self.downloads_path = downloads_path
        super().__init__(*args, **kwargs)

    async def verify(self):
        original_env = self.override_env
        try:
            if getattr(self, 'downloads_path', None):
                from download_cache import load_cache, validate_bindings
                from dependencies import task_digest
                name = self.task.name.split('/')[-1]
                entries = load_cache(self.downloads_path)
                for row in entries.values():
                    if row['task_name'] == name and row.get('kind') == 'uv':
                        validate_bindings(entries, {name: {'task_sha256': task_digest(self.task.paths.task_dir),
                                                           'image_digest': row['image_digest']}})
                        if 'UV_DOWNLOAD_URL' in {**self.task.config.verifier.env,
                                **(self.verifier_env or {}), **self.override_env}:
                            raise ValueError('Verifier uv mirror configuration conflicts with its cache')
                        self.override_env = {**self.override_env, 'UV_DOWNLOAD_URL': row['mirror_base']}
            return await super().verify()
        except BaseException as error:
            try:
                await collect_failure(self.environment, self.trial_paths.trial_dir, error)
            except BaseException as capture_error:
                logging.getLogger(__name__).warning('Failure evidence unavailable (%s)',
                                                     type(capture_error).__name__)
            raise
        finally:
            self.override_env = original_env
