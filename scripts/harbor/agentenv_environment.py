"""Harbor sandbox adapter for an isolated AgentENV Dragonball test server.

Images are built before the job and bound to Harbor's environment content hash.
No model credentials or VM implementation belong in this adapter.
"""
import asyncio
import base64
import json
import logging
import os
from pathlib import Path
import shlex
import struct
import tarfile
import tempfile
import time
import uuid

import httpx
from harbor.environments.base import BaseEnvironment, ExecResult, transfer_tar_filter
from harbor.environments.capabilities import EnvironmentCapabilities, EnvironmentResourceCapabilities
from harbor.models.task.config import NetworkMode

START_FAILURE_CLEANUP_TIMEOUT_SEC = 60
TRANSFER_CLEANUP_TIMEOUT_SEC = 5


def decode_process(body: bytes) -> ExecResult:
    output = {"stdout": bytearray(), "stderr": bytearray()}
    exit_code = None
    terminal = False
    while body:
        if len(body) < 5 or terminal:
            raise ValueError("Invalid or trailing Connect envelope")
        flag, size = struct.unpack(">BI", body[:5])
        if flag not in (0, 2) or len(body) < 5 + size:
            raise ValueError("Truncated or unsupported Connect envelope")
        message = json.loads(body[5:5 + size])
        body = body[5 + size:]
        if flag == 2:
            if message.get("error"):
                raise ValueError("Connect error: " + str(message["error"]))
            terminal = True
            continue
        event = message.get("event", {})
        for name in output:
            if name in event.get("data", {}):
                output[name].extend(base64.b64decode(event["data"][name], validate=True))
        if "end" in event:
            exit_code = event["end"].get("exitCode", 0)
    if not terminal or exit_code is None:
        raise ValueError("Process response has no terminal exit")
    return ExecResult(stdout=output["stdout"].decode(errors="replace"),
                      stderr=output["stderr"].decode(errors="replace"), return_code=exit_code)


class AgentENVEnvironment(BaseEnvironment):
    def __init__(self, *args, api_url: str, images_path: str,
                 request_timeout_sec: float = 180, sandbox_timeout_sec: int = 1800,
                 templates_path=None, template_run_id=None, runtime_assets=None, template_repository=None,
                 material_manifest=None, material_required=False, **kwargs):
        self.api_url = api_url.rstrip("/")
        self.images = json.loads(Path(images_path).read_text())
        self.request_timeout_sec = request_timeout_sec
        self.sandbox_timeout_sec = sandbox_timeout_sec
        self.sandbox = None
        self.template_start_requested = templates_path is not None
        self.templates = json.loads(Path(templates_path).read_text()) if self.template_start_requested else None
        self.template_run_id = template_run_id
        self.template_repository = template_repository
        self.runtime_assets = runtime_assets
        self.initial_workspace_sha256 = None
        self.material_manifest = material_manifest
        self.material_required = material_required
        self.client = httpx.AsyncClient(base_url=self.api_url,
            headers={"X-API-Key": os.environ["AENV_API_KEY"]},
            timeout=request_timeout_sec, trust_env=False)
        super().__init__(*args, **kwargs)

    @staticmethod
    def type() -> str:
        return "agentenv-dragonball"

    @classmethod
    def preflight(cls):
        if not os.environ.get("AENV_API_KEY"):
            raise SystemExit("Set AENV_API_KEY for the isolated AgentENV server")

    @classmethod
    def resource_capabilities(cls):
        return EnvironmentResourceCapabilities(cpu_limit=True, memory_limit=True)

    @property
    def capabilities(self):
        return EnvironmentCapabilities(disable_internet=True)

    def _validate_definition(self):
        entry = self.images.get(self.environment_name)
        if not entry or entry["environment_id"] != self.environment_id:
            raise ValueError("Task environment changed or image missing; prepare images again")
        if not entry["image"].split("@")[-1].startswith("sha256:"):
            raise ValueError("Prepared image must use an immutable digest")
        self._template_binding()

    def _template_binding(self):
        if not getattr(self, 'template_start_requested', self.templates is not None):
            return None
        from dependencies import task_digest
        from prepare_templates import validate_launch_binding
        if not isinstance(self.templates, dict) or self.templates.get('schema') != 1 or not self.templates.get('passed'):
            raise ValueError('Template preparation did not pass')
        binding = self.templates.get('tasks', {}).get(self.environment_name)
        if not binding:
            raise ValueError('Task template is missing')
        validate_launch_binding(binding, self.images[self.environment_name],
            {'cpus': self._effective_cpus, 'memory_mb': self._effective_memory_mb},
            self.template_run_id, self.runtime_assets, task_digest(self.environment_dir.parent),
            repository=getattr(self, 'template_repository', None))
        return binding

    def _request_headers(self, *, guest=False, user=None, headers=None):
        headers = dict(headers or {})
        if guest:
            if not self.sandbox:
                raise RuntimeError("Sandbox has not been started")
            headers.update({"x-agentenv-sandbox-id": self.sandbox["sandboxID"],
                            "x-agentenv-target-port": "49983"})
            if self.sandbox.get("envdAccessToken"):
                headers["X-Access-Token"] = self.sandbox["envdAccessToken"]
            if user is not None:
                headers["Authorization"] = "Basic " + base64.b64encode(f"{user}:".encode()).decode()
        return headers

    async def _request(self, method, path, *, guest=False, user=None, **kwargs):
        headers = self._request_headers(guest=guest, user=user, headers=kwargs.pop("headers", {}))
        async with asyncio.timeout(self.request_timeout_sec):
            response = await self.client.request(method, path, headers=headers, **kwargs)
        response.raise_for_status()
        return response

    def _receipt(self, state):
        self.trial_paths.trial_dir.mkdir(parents=True, exist_ok=True)
        (self.trial_paths.trial_dir / "agentenv.json").write_text(json.dumps({
            "backend": self.type(), "sandbox_id": self.sandbox["sandboxID"],
            "template_id": self.sandbox["templateID"], "state": state,
            "cpu_count": self._effective_cpus or 1,
            "memory_mb": self._effective_memory_mb or 2048,
            "storage_declared_mb": getattr(self.task_env_config, 'storage_mb', None),
            "storage_limit_enforced": False,
            "virtual_disk_capacity_bytes": 64 * 1024**3,
            "harbor_session": self.session_id,
            "guest_disk": getattr(self, 'guest_disk', {}),
            "material_inputs": getattr(self, 'material_checks', None),
            "image": self.images[self.environment_name], "api_url": self.api_url,
            "start_mode": 'template' if self.templates else 'cold',
            "template_key": self._template_binding()['key'] if self.templates else None,
            "initial_workspace_sha256": self.initial_workspace_sha256}, indent=2))

    async def _create_sandbox(self):
        binding = self._template_binding()
        payload = {"timeout": self.sandbox_timeout_sec, "autoPause": False, "secure": True,
            "envVars": self._startup_env(),
            "allow_internet_access": self.network_policy.network_mode != NetworkMode.NO_NETWORK,
            "metadata": {"harbor_session": self.session_id}}
        if binding:
            payload['templateID'] = binding['snapshot_id']
            path = '/sandboxes'
        else:
            payload.update(image=self.images[self.environment_name]['image'],
                cpuCount=self._effective_cpus or 1, memoryMB=self._effective_memory_mb or 2048)
            path = '/sandboxes-cold'
        response = await self._request('POST', path, json=payload)
        self.sandbox = response.json()
        self._receipt('running')

    async def start(self, force_build: bool):
        if force_build:
            raise ValueError("Images are prepared separately; rebuild before starting the job")
        try:
            await self._create_sandbox()
            identity = await self.exec("printf '%s' \"$E2B_SANDBOX_ID\"", user="root", timeout_sec=15)
            if identity.return_code or identity.stdout != self.sandbox["sandboxID"]:
                raise RuntimeError("First guest command did not return the created Sandbox ID")
            await self._record_guest_disk('started')
            binding = self._template_binding()
            if binding:
                from prepare_templates import workspace_digest
                if self.sandbox['sandboxID'] == binding['source_sandbox_id']:
                    raise RuntimeError('Template launch reused the builder Sandbox ID')
                self.initial_workspace_sha256 = await workspace_digest(
                    self, self.images[self.environment_name].get('workdir'))
                if self.initial_workspace_sha256 != binding['workspace_sha256']:
                    raise RuntimeError('Template launch changed the pristine task workspace')
                self._receipt('running')
            await self.ensure_dirs(self._mount_targets(writable_only=True))
            await self._upload_environment_dir_after_start()
            await self._check_material_inputs()
        except BaseException:
            try:
                async with asyncio.timeout(START_FAILURE_CLEANUP_TIMEOUT_SEC):
                    await self.stop(delete=True)
            except BaseException as error:
                logging.getLogger(__name__).warning(
                    'Startup cleanup failed (%s); preserving the startup error', type(error).__name__)
            raise

    async def _check_material_inputs(self):
        if not getattr(self, 'material_required', False):
            return
        from material_inputs import load_manifest, probe_material_inputs
        task = self.environment_dir.parent
        manifest = load_manifest(self.material_manifest, {task.name: task})
        started = time.monotonic()
        self.material_checks = await probe_material_inputs(manifest, task.name, exec_guest=self.exec)
        self.material_checks.update(phase='guest', seconds=time.monotonic() - started)
        self.trial_paths.trial_dir.mkdir(parents=True, exist_ok=True)
        (self.trial_paths.trial_dir / 'material-inputs.json').write_text(json.dumps(self.material_checks, indent=2))
        if not self.material_checks['passed']:
            raise ValueError('Required task materials unavailable; see material-inputs.json')

    async def stop(self, delete: bool):
        # Harbor's keep-environment mode retains the running VM for inspection.
        if not delete:
            return
        try:
            if self.sandbox:
                try:
                    from failure_evidence import collect_trial_failure
                    await collect_trial_failure(self)
                except Exception as error:
                    logging.getLogger(__name__).warning('Failure evidence unavailable (%s)', type(error).__name__)
                finally:
                    await self._record_guest_disk('before_delete')
                    await self._request("DELETE", f'/sandboxes/{self.sandbox["sandboxID"]}')
                    self._receipt("deleted")
                    self.sandbox = None
        finally:
            await self.client.aclose()

    async def _record_guest_disk(self, phase):
        if not hasattr(self, 'guest_disk'):
            self.guest_disk = {}
        try:
            async with asyncio.timeout(5):
                result = await self.exec('df -Pk /', cwd='/', user='root', timeout_sec=2)
            row = result.stdout.strip().splitlines()[-1].split()
            if result.return_code or len(row) < 6:
                raise ValueError('Guest disk statistics unavailable')
            self.guest_disk[phase] = dict(zip(('total_bytes', 'used_bytes', 'available_bytes'),
                                              (int(value) * 1024 for value in row[1:4])))
        except BaseException as error:
            if isinstance(error, asyncio.CancelledError):
                raise
            self.guest_disk[phase] = {'error_type': type(error).__name__}

    async def exec(self, command: str, cwd=None, env=None, timeout_sec=None, user=None):
        user = self._resolve_user(user)
        entry = self.images[self.environment_name]
        process = {"cmd": "/bin/bash", "args": ["-c", command],
                   "envs": self._merge_env(env) or {}}
        cwd = cwd or self.task_env_config.workdir or entry.get("workdir")
        if cwd:
            process["cwd"] = cwd
        # timeout also restores inherited SIGINT, including verifier commands
        # without an explicit budget (0 disables its timer). envd can inherit
        # ignored SIGINT from the guest bootstrap's background supervisor.
        process["args"] = ["-k", "2", str(0 if timeout_sec is None else timeout_sec), "/bin/bash", "-c", command]
        process["cmd"] = "/usr/bin/timeout"
        payload = json.dumps({"process": process, "stdin": False}).encode()
        budget = (timeout_sec + 5) if timeout_sec is not None else self.request_timeout_sec
        async with asyncio.timeout(budget):
            response = await self._request("POST", "/process.Process/Start", guest=True, user=user,
                headers={"Content-Type": "application/connect+json", "Connect-Protocol-Version": "1"},
                content=struct.pack(">BI", 0, len(payload)) + payload)
        return decode_process(response.content)

    async def upload_file(self, source_path, target_path):
        with Path(source_path).open('rb') as stream:
            await self._request("POST", "/files", guest=True,
                params={"path": target_path, "username": "root"},
                files={"file": ("upload", stream, "application/octet-stream")})

    async def download_file(self, source_path, target_path):
        target = Path(target_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        pending = target.with_name(target.name + '.hb-' + uuid.uuid4().hex + '.part')
        try:
            async with asyncio.timeout(self.request_timeout_sec):
                async with self.client.stream("GET", "/files", headers=self._request_headers(guest=True),
                        params={"path": source_path, "username": "root"}) as response:
                    response.raise_for_status()
                    with pending.open('wb') as stream:
                        async for block in response.aiter_bytes():
                            stream.write(block)
            pending.replace(target)
        finally:
            pending.unlink(missing_ok=True)

    async def _checked_exec(self, command):
        result = await self.exec(command, user="root", timeout_sec=120)
        if result.return_code:
            raise RuntimeError(f"File transfer failed ({result.return_code}): {result.stderr}")

    async def upload_dir(self, source_dir, target_dir):
        # A task image may run systemd-tmpfiles' boot-time /tmp cleanup
        # after envd is reachable, including after a template launch.
        remote = "/var/tmp/hb-" + uuid.uuid4().hex + ".tar.gz"
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "tree.tar.gz"
            with tarfile.open(archive, "w:gz") as stream:
                stream.add(source_dir, arcname=".")
            await self.upload_file(archive, remote)
        active_error = False
        try:
            await self._checked_exec(f"mkdir -p {shlex.quote(target_dir)} && "
                f"tar xzf {remote} -C {shlex.quote(target_dir)}")
        except BaseException:
            active_error = True
            raise
        finally:
            await self._remove_transfer_archive(remote, active_error=active_error)

    async def download_dir(self, source_dir, target_dir):
        # A task image may run systemd-tmpfiles' boot-time /tmp cleanup
        # after envd is reachable, including after a template launch.
        remote = "/var/tmp/hb-" + uuid.uuid4().hex + ".tar.gz"
        active_error = False
        try:
            await self._checked_exec(f"mkdir -p /var/tmp && tar czf {remote} -C {shlex.quote(source_dir)} .")
            with tempfile.TemporaryDirectory() as directory:
                archive = Path(directory) / "tree.tar.gz"
                await self.download_file(remote, archive)
                with tarfile.open(archive) as stream:
                    stream.extractall(target_dir, filter=transfer_tar_filter)
        except BaseException:
            active_error = True
            raise
        finally:
            await self._remove_transfer_archive(remote, active_error=active_error)

    async def _remove_transfer_archive(self, remote, *, active_error):
        try:
            async with asyncio.timeout(TRANSFER_CLEANUP_TIMEOUT_SEC):
                await self._checked_exec(f"rm -f {remote}")
        except BaseException as error:
            if not active_error:
                raise
            logging.getLogger(__name__).warning(
                'Transfer cleanup failed (%s); preserving the transfer error', type(error).__name__)
