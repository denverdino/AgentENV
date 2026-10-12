#!/usr/bin/env python3
"""Real VM transport probe before running task scores."""
import asyncio
import json
from pathlib import Path
import sys
import shlex
import time
from urllib.parse import urlsplit

from agentenv_environment import AgentENVEnvironment
from harbor.models.trial.paths import TrialPaths
from harbor.models.task.task import Task
from prepare_tasks import task_environment_name


TRANSPORT_TIMEOUT_SEC = 180
CLEANUP_TIMEOUT_SEC = 60


def probe_task(config):
    return Path(config["tasks"][min(1, len(config['tasks']) - 1)]["path"])


def probe_outer_timeout(config):
    startup = Task(probe_task(config)).config.environment.build_timeout_sec
    return startup + TRANSPORT_TIMEOUT_SEC + CLEANUP_TIMEOUT_SEC + 15


def make_probe_environment(config, output):
    task = probe_task(config)
    task_config = Task(task).config.environment
    overrides = {name: value for name, value in config['environment'].items()
                 if name in ('override_cpus', 'override_memory_mb')}
    return AgentENVEnvironment(environment_dir=task / "environment",
        environment_name=task_environment_name(task), session_id="transport-probe",
        trial_paths=TrialPaths(output), task_env_config=task_config,
        network_policy=task_config.resolve_baseline(), **overrides,
        persistent_env={"HB_PERSISTENT": "base", **config['environment'].get('env', {})},
        **config["environment"]["kwargs"])


async def check_transport(environment, config, output, checks):
    checks['network_mode'] = environment.network_policy.network_mode.value
    if checks['network_mode'] == 'no-network':
        result = await environment.exec("curl --noproxy '*' -k -I -sS --connect-timeout 2 --max-time 4 https://1.1.1.1", user='root', timeout_sec=6)
        assert result.return_code in (7, 28), result
        checks['guest_public_egress_blocked'] = True
        checks['egress_probe_exit_code'] = result.return_code
    result = await environment.exec('id -u agent >/dev/null 2>&1 || useradd -m -s /bin/bash agent', user='root')
    assert result.return_code == 0, result
    if config['environment'].get('env', {}).get('https_proxy'):
        available = await environment.exec('command -v python3', user='root', timeout_sec=5)
        if available.return_code == 0:
            script = """import urllib.request,json,threading
from http.server import BaseHTTPRequestHandler,HTTPServer
class Local(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
    def log_message(self,*args): pass
egress = [urllib.request.urlopen(url, timeout=10).status for url in ['http://deb.debian.org/debian/README', 'https://pypi.org/pypi/pytest/8.4.1/json']]
with HTTPServer(('0.0.0.0',0),Local) as server:
    threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        local = [urllib.request.urlopen('http://'+host+':'+str(server.server_port),timeout=5).status for host in ('0.0.0.0','127.0.0.1','localhost')]
    finally:
        server.shutdown()
print(json.dumps({'egress':egress,'local':local}))
"""
            result = await environment.exec('python3 -c ' + shlex.quote(script), user='root', timeout_sec=25)
            assert result.return_code == 0 and json.loads(result.stdout) == {
                'egress': [200, 200], 'local': [200, 200, 200]}, result
            checks['guest_http_https_egress'] = True
            checks['guest_local_http_proxy_bypass'] = True
        else:
            # Official minimal images need not ship Python or curl. Probe
            # HTTP and CONNECT using bash, without changing the task image.
            proxy = urlsplit(config['environment']['env']['https_proxy'])
            for method, target, host in [('GET', 'http://deb.debian.org/debian/README', 'deb.debian.org'),
                                         ('CONNECT', 'pypi.org:443', 'pypi.org:443')]:
                wire = f'{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n'
                command = f'exec 3<>/dev/tcp/{proxy.hostname}/{proxy.port}; ' + \
                    f'printf %s {shlex.quote(wire)} >&3; IFS= read -r status <&3; printf "%s" "$status"'
                result = await environment.exec(command, user='root', timeout_sec=15)
                assert result.return_code == 0 and result.stdout.split()[1:2] == ['200'], result
            checks['guest_http_and_https_connect'] = True
            checks['guest_tls_application_probe'] = 'not checked: image has no Python'
    with environment.with_default_user("agent"):
        result = await environment.exec("printf '%s|%s|%s' \"$(whoami)\" \"$HB_PERSISTENT\" \"$HB_EXEC\"; exit 7",
            env={"HB_PERSISTENT": "override", "HB_EXEC": "per-call"})
    assert (result.return_code, result.stdout) == (7, "agent|override|per-call"), result
    checks["user_env_and_nonzero_exit"] = True
    before = time.monotonic()
    result = await environment.exec("sleep 30", user="root", timeout_sec=1)
    elapsed = time.monotonic() - before
    assert result.return_code == 124 and elapsed < 5, (result, elapsed)
    checks["guest_timeout_seconds"] = elapsed
    tree = output / "tree"
    tree.mkdir()
    (tree / "empty").mkdir()
    (tree / "script").write_bytes(b"#!/bin/sh\nprintf checked")
    (tree / "script").chmod(0o755)
    (tree / "link").symlink_to("script")
    (tree / "binary").write_bytes(bytes(range(256)))
    await environment.upload_dir(tree, "/tmp/harbor-tree")
    result = await environment.exec("/tmp/harbor-tree/link", user="root")
    assert result.return_code == 0 and result.stdout == "checked"
    downloaded = output / "downloaded"
    await environment.download_dir("/tmp/harbor-tree", downloaded)
    assert (downloaded / "empty").is_dir()
    assert (downloaded / "link").is_symlink()
    assert (downloaded / "link").readlink() == Path("script")
    assert (downloaded / "script").stat().st_mode & 0o111
    assert (downloaded / "binary").read_bytes() == bytes(range(256))
    checks["directory_and_binary_fidelity"] = True


async def run_probe(config, output):
    output.mkdir()
    task = probe_task(config)
    startup = Task(task).config.environment.build_timeout_sec
    environment = make_probe_environment(config, output)
    checks = {'task': task.name, 'purpose': 'transport preflight with benchmark resources and policy',
              'cpus': environment._effective_cpus, 'memory_mb': environment._effective_memory_mb,
              'passed': False, 'cleanup': 'pending', 'phase': 'startup',
              'budgets_sec': {'startup': startup, 'transport': TRANSPORT_TIMEOUT_SEC,
                              'cleanup': CLEANUP_TIMEOUT_SEC}, 'phase_seconds': {}}

    def save():
        (output / 'checks.json').write_text(json.dumps(checks, indent=2))

    active_error = False
    started = time.monotonic()
    save()
    try:
        async with asyncio.timeout(startup):
            await environment.start(False)
        checks['phase_seconds']['startup'] = time.monotonic() - started
        checks['phase'] = 'transport'
        started = time.monotonic()
        save()
        async with asyncio.timeout(TRANSPORT_TIMEOUT_SEC):
            await check_transport(environment, config, output, checks)
        checks['phase_seconds']['transport'] = time.monotonic() - started
        checks['passed'] = True
    except BaseException as error:
        active_error = True
        checks['failed_phase'] = checks['phase']
        checks['phase_seconds'][checks['phase']] = time.monotonic() - started
        checks['error_type'] = type(error).__name__
        raise
    finally:
        checks['phase'] = 'cleanup'
        started = time.monotonic()
        save()
        try:
            async with asyncio.timeout(CLEANUP_TIMEOUT_SEC):
                await environment.stop(True)
            checks['cleanup'] = 'passed'
        except BaseException as error:
            checks.update(passed=False, cleanup='failed', cleanup_error_type=type(error).__name__)
            if not active_error:
                checks['failed_phase'] = 'cleanup'
                raise
        finally:
            checks['phase_seconds']['cleanup'] = time.monotonic() - started
            checks['phase'] = 'finished'
            save()
    return checks


async def main():
    path = Path(sys.argv[1])
    checks = await run_probe(json.loads(path.read_text()), path.parent / "transport-probe")
    print(json.dumps(checks))


if __name__ == "__main__":
    asyncio.run(main())
