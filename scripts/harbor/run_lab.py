#!/usr/bin/env python3
"""Run Harbor Oracle on Dragonball using the existing isolated M4 lab harness.

Root only orchestrates namespaces and resource cleanup. Harbor, fixtures, the
AgentENV server and VMM run as the explicitly configured non-root account.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
import pwd
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from dragonball_m4_lab import Lab, command as host_command
from dragonball_m4_cleanup import cleanup_owned_ublk
from dragonball_m4_scenarios import (API_KEY, Services, api, fixtures,
    isolated_controller, start_server, terminate_controller)
from prepare_tasks import (TASKS, BENCHMARK_TASKS, COMPUTER_TASKS, HARBOR_REVISION,
                           verify_source, verify_tree, verify_benchmark)
from prepare_acr import verify_delivery
from egress_proxy import benchmark_proxy_env
from resources import (add_resource_arguments, plan_resources, check_capacity, select_tasks,
                       job_budget, validate_receipt_resources)
from scope import filter_microvm_tasks
from probe import probe_outer_timeout
from dependencies import validate_lab_dependencies
from prepare_templates import dependency_disk_bytes, template_disk_bytes
from oss_env import read_env, snapshot_config, server_proxy_env

MATERIAL_HOST_TIMEOUT_SEC = 120


def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_report(output, report):
    """Persist the authoritative receipt before deriving an optional summary."""
    path = Path(output) / 'receipt.json'
    path.write_text(json.dumps(report, indent=2))
    pending = Path(output) / '.summary.json.tmp'
    try:
        from benchmark_summary import summarize
        summary = summarize(path)
        pending.write_text(json.dumps(summary, indent=2, allow_nan=False))
        pending.replace(Path(output) / 'summary.json')
    except Exception as error:
        (Path(output) / 'summary-error.json').write_text(json.dumps(
            {'error_type': type(error).__name__, 'source_report': 'receipt.json'}, indent=2))
    finally:
        pending.unlink(missing_ok=True)


def observe_vmm(proc, runner, expected_uid):
    try:
        if not (proc / 'exe').samefile(runner):
            return None
        uid = int(next(line.split()[1] for line in (proc / 'status').read_text().splitlines()
                       if line.startswith('Uid:')))
        assert uid == expected_uid, 'VMM ran with an unexpected UID'
        return {'pid': int(proc.name), 'uid': uid,
                'name': (proc / 'comm').read_text().strip(),
                'binary': os.readlink(proc / 'exe')}
    except FileNotFoundError:
        return None


def validate_results(results):
    expected = Counter((task, agent) for task in TASKS for agent in ("oracle", "nop"))
    actual = Counter((row["task_name"].split("/")[-1], row["agent_info"]["name"]) for row in results)
    if actual != expected:
        raise ValueError("Trial set does not match each required task/agent pair")
    for result in results:
        if result.get("exception_info"):
            raise ValueError("Trial failed: " + str(result["exception_info"]))
        task = result["task_name"].split("/")[-1]
        steps = result.get("step_results")
        if task == "hello-multi-step-simple":
            if not steps or [step["step_name"] for step in steps] != ["create-file", "append-content"]:
                raise ValueError("Multi-step trial is missing or reordered steps")
        elif steps:
            raise ValueError("Unexpected multi-step result")
        expected_reward = 1 if result["agent_info"]["name"] == "oracle" else 0
        for step in steps or [result]:
            verifier = step.get("verifier_result") or {}
            if step.get("exception_info") or (verifier.get("rewards") or {}).get("reward") != expected_reward:
                raise ValueError("Unexpected verifier score or failed step")


def validate_benchmark_results(results, model=False, task_names=BENCHMARK_TASKS,
                               strict_oracle=True, controls=True, model_task_names=None):
    expected = Counter((task, agent) for task in task_names
                       for agent in (('oracle', 'nop') if controls else ()))
    if model:
        expected.update((task, 'terminus-2') for task in
                        (task_names if model_task_names is None else model_task_names))
    actual = Counter((row['task_name'].split('/')[-1], row['agent_info']['name']) for row in results)
    if actual != expected:
        raise ValueError('Benchmark trial set does not match required task/agent pairs')
    for row in results:
        reward = ((row.get('verifier_result') or {}).get('rewards') or {}).get('reward')
        agent = row['agent_info']['name']
        allowed = (0, 1) if agent == 'terminus-2' or (agent == 'oracle' and not strict_oracle) else ((1,) if agent == 'oracle' else (0,))
        if row.get('exception_info') or row.get('step_results') or reward not in allowed:
            raise ValueError('Invalid benchmark score or trial exception')


def model_agent_config(model):
    # Harbor prioritizes registered names over import_path: omit name so the
    # credential-safe subclass is selected. Its name() remains 'terminus-2'.
    return {'import_path': 'dashscope_agent:DashScopeTerminus2', 'model_name': 'openai/' + model,
        'kwargs': {'api_base': 'https://dashscope.aliyuncs.com/compatible-mode/v1',
            'enable_summarize': False,
            'model_info': {'max_input_tokens': 1000000, 'max_output_tokens': 4096},
            'llm_call_kwargs': {'extra_body': {'enable_thinking': False}, 'max_tokens': 4096}}}


def computer_agent_config(model):
    return {'import_path': 'dashscope_agent:DashScopeComputer1', 'model_name': 'openai/' + model,
        'kwargs': {'api_base': 'https://dashscope.aliyuncs.com/compatible-mode/v1',
            'provider': 'litellm', 'enable_images': True, 'extra_tools': [], 'max_turns': 40,
            'start_url': 'file:///app/click_calibration.html',
            'model_info': {'max_input_tokens': 1000000, 'max_output_tokens': 4096},
            'llm_call_kwargs': {'extra_body': {'enable_thinking': False}, 'max_tokens': 4096}}}


def evaluation_agents(args, computer=False):
    controls = ([{'import_path': 'computer_control:ComputerCalibrationControl'}, {'name': 'nop'}]
                if computer else [{'name': 'oracle'}, {'name': 'nop'}])
    return ([] if args.model_only else controls) + (
        [computer_agent_config(args.model) if computer else model_agent_config(args.model)] if args.model else [])


def run_material_preflight(args, task_names, downloads):
    spec = args.output / 'material-preflight-inputs.json'
    spec.write_text(json.dumps({'tasks': {name: str(args.prepared.resolve() / 'tasks' / name) for name in task_names},
        'material_manifest': str(args.material_inputs.resolve()) if args.material_inputs else None,
        'model_required': bool(args.model), 'downloads': downloads}, indent=2))
    spec.chmod(0o644)
    command = ['nsenter', '--net=/proc/' + os.environ['AENV_HARBOR_PARENT_PID'] + '/ns/net',
        '--', 'runuser', '-u', args.run_user, '--', str(args.harbor.parent / 'python'), '-B',
        str(Path(__file__).with_name('material_inputs.py')), '--job', str(spec)]
    environment = {name: os.environ[name] for name in ('PATH', 'LANG', 'LC_ALL', 'PYTHONDONTWRITEBYTECODE',
        'HTTP_PROXY', 'HTTPS_PROXY', 'NO_PROXY', 'http_proxy', 'https_proxy', 'no_proxy') if name in os.environ}
    started = time.monotonic()
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             env=environment, start_new_session=True)
    try:
        stdout, _ = child.communicate(timeout=MATERIAL_HOST_TIMEOUT_SEC)
    except BaseException:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
        child.communicate(timeout=5)
        raise
    if child.returncode:
        raise ValueError('Material host preflight failed: exit ' + str(child.returncode))
    record = json.loads(stdout)
    record['material_gate'].update(seconds=time.monotonic() - started,
        budget_sec=MATERIAL_HOST_TIMEOUT_SEC, uid=pwd.getpwnam(args.run_user).pw_uid,
        coverage='Explicit manifest inputs and known required sources only')
    return record


def validate_template_options(args, full):
    if args.shared_templates and not (args.oss_env and args.template_start and full):
        raise ValueError('--shared-templates requires --oss-env, --template-start and full-suite prepared tasks')
    if args.template_start and not args.dependencies and args.shared_templates != 'reuse':
        raise ValueError('--template-start requires --dependencies or shared template reuse')
    if args.dependencies and (not full or args.model):
        raise ValueError('Dependency qualification requires frozen eligible tasks and controls only')


def validate_computer_results(results, model=False):
    agents = ['computer-calibration-control', 'nop'] + (['computer-1'] if model else [])
    expected = Counter(('computer-1/click-calibration', agent) for agent in agents)
    actual = Counter((row['task_name'], row['agent_info']['name']) for row in results)
    if actual != expected:
        raise ValueError('Computer Use trial set does not match required task/agent pairs')
    for row in results:
        reward = ((row.get('verifier_result') or {}).get('rewards') or {}).get('reward')
        agent = row['agent_info']['name']
        allowed = (0, 1) if agent == 'computer-1' else ((1,) if agent == 'computer-calibration-control' else (0,))
        if row.get('exception_info') or row.get('step_results') or reward not in allowed:
            raise ValueError('Invalid Computer Use score or trial exception')


def _main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--prepared", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--harbor", type=Path, required=True)
    parser.add_argument("--run-user", default="aenv")
    parser.add_argument("--inside", action="store_true")
    parser.add_argument("--model", choices=['qwen3.8-flash', 'qwen3.8-max'])
    parser.add_argument('--model-only', action='store_true',
                        help='Complete a full-suite task previously stopped by its Oracle diagnostic')
    parser.add_argument('--dependencies', type=Path, help='Verified immutable dependency binding')
    parser.add_argument('--template-start', action='store_true', help='Capture pristine batch-local templates')
    parser.add_argument('--oss-env', type=Path,
                        help='Private Harbor OSS .env; skips local MinIO without changing other tests')
    parser.add_argument('--shared-templates', choices=('prepare', 'reuse'),
                        help='Persistent shared OSS templates; prepare misses or reuse only')
    parser.add_argument('--downloads', type=Path, help='Verified persistent public download cache binding')
    parser.add_argument('--material-inputs', type=Path, help='Explicit source-bound external material manifest')
    parser.add_argument("--continuous", action="store_true", help="Persistent Lab with single-trial native Jobs")
    parser.add_argument("--acceptance-trials", type=int, default=0, help="No-model repeated frozen task scheduling diagnostic")
    add_resource_arguments(parser)
    args = parser.parse_args(argv)
    if args.continuous and (args.shared_templates != "reuse" or not args.oss_env or args.dependencies):
        parser.error("Continuous mode requires OSS shared-template reuse and the original verifier")
    if args.continuous and args.model and not args.model_only:
        parser.error("Continuous model evaluation requires explicit --model-only")
    if args.acceptance_trials and (not args.continuous or args.model or args.acceptance_trials < 1):
        parser.error("Acceptance repeats require continuous mode without a model")
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, terminate_controller)
    if os.geteuid() != 0:
        parser.error("Root namespace orchestration required; workloads use --run-user")
    if args.oss_env:
        try:
            read_env(args.oss_env, pwd.getpwnam(args.run_user).pw_uid)
        except (OSError, ValueError) as error:
            parser.error('Harbor OSS env validation failed (' + type(error).__name__ + ')')
    if not args.inside:
        env = {**os.environ, "AENV_M4_PARENT_NET": os.readlink("/proc/self/ns/net"),
               "AENV_HARBOR_PARENT_PID": str(os.getpid()),
               "AENV_RUN_USER": args.run_user, "PYTHONDONTWRITEBYTECODE": "1"}
        return isolated_controller(["unshare", "--mount", "--net", "--propagation", "private",
            sys.executable, __file__, *(sys.argv[1:] if argv is None else argv), "--inside"], env)
    inputs = json.loads(args.inputs.read_text())
    prepared = json.loads((args.prepared / "prepared.json").read_text())
    source = verify_source(Path(prepared["source_root"]), Path(prepared["source_manifest"]))
    benchmark = prepared.get('benchmark')
    full = prepared.get('acr_delivery')
    if args.model_only and not (full and args.model):
        parser.error('--model-only requires a frozen full-suite task and --model')
    computer = prepared.get('computer_use', False)
    task_names = COMPUTER_TASKS if computer else BENCHMARK_TASKS if benchmark else TASKS
    if full:
        source, entries = verify_delivery(Path(full))
        task_names = select_tasks(prepared['task_names'], entries)
        _, excluded = filter_microvm_tasks(task_names)
        if excluded:
            parser.error('Prepared tasks include excluded nested-VM workloads: ' + str(excluded))
    assert prepared["task_names"] == list(task_names), "Unexpected or duplicate task selection"
    try:
        validate_template_options(args, full)
    except ValueError as error:
        parser.error(str(error))
    if args.model and (not (benchmark or computer) or not os.environ.get('DASHSCOPE_API_KEY')):
        parser.error('Model evaluation requires prepared benchmark tasks and DASHSCOPE_API_KEY')
    model_api_key = os.environ.pop('DASHSCOPE_API_KEY', None)
    if benchmark and not full:
        source = verify_benchmark(Path(benchmark['source_root']), Path(benchmark['source_manifest']))
    for task in task_names:
        prefix = ('' if benchmark else 'examples/tasks/') + task + '/'
        verify_tree(args.prepared / "tasks" / task,
            {name[len(prefix):]: digest for name, digest in source["files"].items() if name.startswith(prefix)})
    from harbor.models.task.task import Task
    task_configs = {name: Task(args.prepared / 'tasks' / name).config.model_dump(mode='json')
                    for name in task_names}
    selected_task_names = list(task_names)
    dependencies = json.loads(args.dependencies.read_text()) if args.dependencies else None
    downloads = {}
    if args.downloads:
        from download_cache import load_cache, validate_bindings
        from dependencies import task_digest
        downloads = validate_bindings(load_cache(args.downloads), {
            name: {'task_sha256': task_digest(args.prepared / 'tasks' / name),
                   'image_digest': prepared['images'][name]['source_platform_digest']} for name in task_names})
        if not downloads:
            parser.error('Download cache has no binding for the selected tasks')
    dependency_files = []
    if dependencies:
        dependency_files = validate_lab_dependencies(dependencies,
            {name: args.prepared / 'tasks' / name for name in task_names}, entries, HARBOR_REVISION)
    # User-authorized daily history policy: after the host lease and residual
    # audit, before capacity checks, fixtures, VM creation or model calls.
    from dragonball_m4_host import require_clean_host
    from cleanup_history import plan_cleanup, apply_cleanup, inspect_host_references
    require_clean_host()
    args.output.mkdir(parents=True, exist_ok=False)
    report = {'root': None, 'prepared': prepared, 'started': time.time(),
              'cleanup': 'not-needed', 'passed': False, 'selected_task_names': selected_task_names,
              'task_configs': task_configs, 'results': [], 'sandbox_receipts': []}
    report['driver_sha256'] = {path.name: sha(path) for path in Path(__file__).parent.glob('*.py')}
    try:
        material = run_material_preflight(args, selected_task_names, downloads)
    except Exception as error:
        report.update(status='material-preflight-failed', error_type=type(error).__name__, finished=time.time())
        write_report(args.output, report)
        return 1
    args.material_manifest = material['manifest']
    report['material_gate'] = material['material_gate']
    model_task_names = material['material_gate']['eligible_task_names']
    (args.output / 'material-gate.json').write_text(json.dumps(material['material_gate'], indent=2))
    if args.model_only:
        if not model_task_names:
            report.update(status='material-blocked', finished=time.time())
            write_report(args.output, report)
            return 0
        task_names = tuple(model_task_names)
        prepared = {**prepared, 'task_names': list(task_names),
                    'images': {name: prepared['images'][name] for name in task_names}}
        report['prepared'] = prepared
    try:
        resource_plan = plan_resources(args,
            {name: task_configs[name]['environment'] for name in task_names}, controls=not args.model_only)
    except ValueError as error:
        parser.error(str(error))
    if dependencies or args.template_start:
        extra = template_disk_bytes(resource_plan, dependencies or {'tasks': {}},
            mode=args.shared_templates or 'prepare') if args.template_start else dependency_disk_bytes(resource_plan, dependencies)
        resource_plan['dependency_template_disk_bytes'] = extra
        resource_plan['planned_disk_bytes'] += extra
        resource_plan['required_disk_bytes'] += extra
    protected = {args.output.resolve(), args.prepared.resolve(), args.inputs.resolve()}
    protected.update(Path(value).resolve() for value in inputs.values() if isinstance(value, str) and value.startswith('/'))
    protected.update(Path(row['archive']).resolve() for row in prepared['images'].values())
    protected.update(Path(row['path']).resolve() for row in downloads.values())
    protected.update(Path(p).resolve() for p in dependency_files)
    if args.oss_env: protected.add(args.oss_env.resolve())
    if args.material_inputs: protected.add(args.material_inputs.resolve())
    protected.update(Path(row['cache_path']).resolve() for task in args.material_manifest['tasks'].values()
                     for row in task['inputs'] if row.get('cache_path'))
    history = plan_cleanup(sorted(Path('/var/tmp').glob('db-m4.*')), cutoff=time.time() - 86400,
                           protected=protected, inspect_references=inspect_host_references)
    (args.output / 'historical-cleanup-plan.json').write_text(json.dumps(history, indent=2))
    cleaned = apply_cleanup(history, inspect_references=inspect_host_references,
        record=lambda receipt: (args.output / 'historical-cleanup-receipt.json').write_text(json.dumps(receipt, indent=2)))
    (args.output / 'historical-cleanup-receipt.json').write_text(json.dumps(cleaned, indent=2))
    if args.continuous:
        # Per-trial disk leases decide admission in the persistent Lab. Do not
        # reserve every task's builder or a whole batch before starting it.
        resource_plan.update(planned_disk_bytes=0, required_disk_bytes=resource_plan['reserve_disk_bytes'])
    from resources import check_ublk
    check_ublk()
    subprocess.run(['runuser', '-u', args.run_user, '--', sys.executable, '-B', '-c',
        "import os; fd=os.open('/dev/ublk-control', os.O_RDWR); os.close(fd)"], check=True, timeout=10)
    capacity = {'cpu_count': len(os.sched_getaffinity(0)),
        'available_memory_bytes': int(next(line.split()[1] for line in Path('/proc/meminfo').read_text().splitlines()
                                          if line.startswith('MemAvailable:'))) * 1024,
        'free_disk_bytes': shutil.disk_usage('/var/tmp').free}
    check_capacity(resource_plan, **capacity)
    downloads_path = args.output / 'downloads.json'
    if downloads:
        downloads_path.write_text(json.dumps({'schema': 1, 'downloads': downloads}, indent=2))
    root = Path(tempfile.mkdtemp(prefix="db-m4.", dir="/var/tmp"))
    oss = None
    if args.oss_env:
        oss = snapshot_config(args.oss_env, 'shared-templates-v1' if args.shared_templates else root.name, uid=pwd.getpwnam(args.run_user).pw_uid)
    report.update(root=str(root), cleanup='pending', resource_plan=resource_plan, host_capacity=capacity)
    report['snapshot_store'] = {'backend': 'aliyun-oss' if oss else 'local-minio',
                              **({key: oss[key] for key in ('endpoint', 'region', 'bucket', 'prefix')} if oss else {})}
    if downloads:
        report['downloads'] = {'binding_sha256': sha(args.downloads),
                               'artifacts': [{key: row[key] for key in ('task_name', 'sha256', 'bytes', 'kind')}
                                             for row in downloads.values()]}
    if dependencies:
        report['dependencies'] = {'binding_sha256': sha(args.dependencies),
            'images': [row['verifier']['image'] for row in dependencies['tasks'].values() if row['verifier']],
            'git_protocol': 'file', 'template_start': args.template_start}
    lab = services = None
    exit_code = 1
    try:
        lab = Lab(root, args.run_user, preflight_profile={
            "disk_path": str(root), "reserve_bytes": resource_plan['reserve_disk_bytes'],
            "planned_bytes": resource_plan['planned_disk_bytes'], "retained_bytes": 0,
            "required_env": ["AENV_RUN_USER"],
            "input_files": {"assets": [inputs["assets"]],
                "executables": [inputs[key] for key in ("server", "daemon", "ublk_cli", "regctl", "registry",
                                                        *(() if oss else ('s3',)))],
                "json": [str(args.inputs), str(args.prepared / "prepared.json"),
                         *([str(args.dependencies)] if dependencies else [])],
                "files": [entry["archive"] for entry in prepared["images"].values()] + dependency_files +
                         ([str(args.oss_env)] if oss else []) + [row['path'] for row in downloads.values()]},
            "socket_paths": [str(root / "a/run/db-runtime-XXXXXXXX/runner.sock")]})
        services = Services(lab, memory_gib=resource_plan['services_memory_gib'])
        proxy_env = {}
        if args.shared_templates:
            report['shared_templates'] = {'mode': args.shared_templates, 'repository_retained': True}
        if benchmark or computer or oss:
            # Benchmark-only address stays outside AgentENV's private-address
            # denies. Both the address and route live in owned lab namespaces.
            host_command(['ip', 'addr', 'add', '198.18.0.1/32', 'dev', 'm4br'])
            listener = socket.socket()
            try:
                listener.bind(('198.18.0.1', 3128))
                listener.listen(128)
                def join_proxy():
                    (services.group / 'cgroup.procs').write_text(str(os.getpid()))
                with (root / 'egress.log').open('w') as log:
                    child = subprocess.Popen(['nsenter', '--net=/proc/' + os.environ['AENV_HARBOR_PARENT_PID'] + '/ns/net',
                        '--', 'runuser', '-u', args.run_user, '--', sys.executable,
                        str(Path(__file__).with_name('egress_proxy.py')), '--listen-fd', str(listener.fileno()),
                        *(['--download-cache', str(downloads_path)] if downloads else [])],
                        pass_fds=(listener.fileno(),), preexec_fn=join_proxy, stdout=log, stderr=log, cwd=root)
                services.children.append(child)
            finally:
                listener.close()
            proxy_env = benchmark_proxy_env()
            report['egress'] = {'transport': 'inherited lab listener, parent-network outbound sockets',
                                'uid': lab.account.pw_uid, 'host_routes_or_firewall_changed': False}
        cert, _, assets = fixtures(lab, inputs, services, snapshot_config=oss)
        node = lab.start_node("a")
        limits = resource_plan['node']
        (node.cgroup / 'memory.max').write_text(str(limits['memory_gib'] * 1024**3))
        (node.cgroup / 'cpu.max').write_text(f"{limits['cpu_cores'] * 100000} 100000")
        (node.cgroup / 'pids.max').write_text(str(limits['pids']))
        report['node_limits'] = {name: (node.cgroup / name).read_text().strip()
                                 for name in ('memory.max', 'memory.swap.max', 'cpu.max', 'pids.max')}
        from resources import effective_profile
        agents = evaluation_agents(args, computer)
        report['effective_profile'] = effective_profile(resource_plan, report['node_limits'], agents=agents)
        if benchmark or computer or oss:
            host_command(['nsenter', '-t', str(node.pid), '-n', 'ip', 'route', 'add',
                          '198.18.0.1/32', 'via', '10.241.0.1'])
        report["server"] = start_server(node, inputs, cert, assets, "dragonball", snapshot_config=oss,
                                        extra_env=server_proxy_env() if oss else None)
        report["runtime_sha256"] = {name: sha(path) for name, path in {
            "server": inputs["server"], "runner": assets["runner_path"],
            "kernel": assets["kernel_path"], "tools": assets["tools_path"]}.items()}
        profile = {**report['effective_profile'], 'harbor_revision': HARBOR_REVISION,
                   'runtime_sha256': report['runtime_sha256'], 'driver_sha256': report['driver_sha256'],
                   'task_resources': {name: row['effective'] for name, row in resource_plan['tasks'].items()},
                   'task_contracts': {name: {'config_sha256': sha(args.prepared / 'tasks' / name / 'task.toml'),
                       'timeouts_sec': {phase: task_configs[name][phase].get(key) for phase, key in (
                           ('environment', 'build_timeout_sec'), ('agent', 'timeout_sec'), ('verifier', 'timeout_sec'))}}
                       for name in selected_task_names}}
        report['effective_profile_sha256'] = hashlib.sha256(
            json.dumps(profile, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        (args.output / 'effective-profile.json').write_text(json.dumps(
            {**profile, 'sha256': report['effective_profile_sha256']}, indent=2))
        if args.continuous:
            from run_campaign import run_trials
            repository = {key: oss[key] for key in ('endpoint', 'region', 'bucket', 'prefix')}
            report.update(run_trials(args, lab, node, inputs, prepared, resource_plan, task_configs,
                report['runtime_sha256'], proxy_env, repository, model_api_key, assets['runner_path']))
            exit_code = 0 if report['passed'] else 1
        else:
            images = {}
            regctl = inputs["regctl"]
            for name, entry in prepared["images"].items():
                assert sha(entry["archive"]) == entry["archive_sha256"], "Task image archive changed"
                ref = f"10.241.0.1:5443/harbor/{name}:candidate"
                command = ["env", "REGCTL_CONFIG=" + str(node.root / "regctl.json"), regctl]
                node.run([*command, "image", "import", ref, entry["archive"]], timeout=1800 if full else 180)
                digest = node.run([*command, "image", "digest", ref]).stdout.strip()
                assert digest.startswith("sha256:")
                if full:
                    assert digest == entries[name]['source_platform_digest'], 'Fixture changed the mirrored image digest'
                images[entry.get('environment_name', name)] = {**entry, "image": ref.split(":candidate")[0] + "@" + digest}
            images_path = args.output / "images.json"
            images_path.write_text(json.dumps(images, indent=2))
            config = {"job_name": "dragonball-smoke", "jobs_dir": str(args.output / "jobs"),
                "n_concurrent_trials": resource_plan['concurrency'], "n_attempts": 1, "retry": {"max_retries": 0},
                "environment": {"import_path": "agentenv_environment:AgentENVEnvironment",
                    **resource_plan['overrides'],
                    "kwargs": {"api_url": f"http://{node.address}:8000", "images_path": str(images_path)}},
                "agents": [{"name": "oracle"}, {"name": "nop"}],
                "tasks": [{"path": str(args.prepared / "tasks" / name)} for name in prepared["task_names"]]}
            config['environment']['env'] = proxy_env
            config['verifier'] = {'import_path': 'failure_evidence:EvidenceVerifier'}
            if downloads:
                config['verifier']['kwargs'] = {'downloads_path': str(downloads_path)}
            if dependencies:
                config['verifier'] = {'import_path': 'cached_verifier:CachedVerifier',
                    'kwargs': {'dependencies_path': str(args.dependencies.resolve()), 'local_git_only': True,
                               **({'downloads_path': str(downloads_path)} if downloads else {})}}
            if full:
                phase_budget = max(cfg['environment']['build_timeout_sec'] +
                    (cfg['agent']['timeout_sec'] or 900) + cfg['verifier']['timeout_sec'] + 900
                    for cfg in task_configs.values())
                config['environment']['kwargs'].update(
                    request_timeout_sec=phase_budget, sandbox_timeout_sec=int(phase_budget + 300))
                report['task_budget_sec'] = phase_budget
                if len(task_names) == 1:
                    report['task_config'] = task_configs[task_names[0]]
            if computer:
                # The local calibration task declares no-network; only the host-side
                # model CLI uses the proxy. Do not inject it into the guest.
                config['environment']['env'] = {}
                config['agents'] = [{'import_path': 'computer_control:ComputerCalibrationControl'}, {'name': 'nop'}]
            config_path = args.output / "job.json"
            config_path.write_text(json.dumps(config, indent=2))
            for path in (args.output, images_path, config_path):
                os.chown(path, lab.account.pw_uid, lab.account.pw_gid)
            report["disk_before_job"] = shutil.disk_usage(root)._asdict()
            def transport_probe():
                probe = node.run(["env", "AENV_API_KEY=" + API_KEY,
                    str(args.harbor.parent / "python"), str(Path(__file__).with_name("probe.py")),
                    str(config_path)], timeout=probe_outer_timeout(config))
                report["transport_probe"] = json.loads(probe.stdout)

            if args.shared_templates != 'reuse':
                transport_probe()
            if args.template_start:
                assets_path = args.output / 'runtime-assets.json'
                assets_path.write_text(json.dumps(report['runtime_sha256'], indent=2))
                os.chown(assets_path, lab.account.pw_uid, lab.account.pw_gid)
                run_id = root.name
                templates_path = args.output / 'templates/receipt.json'
                shared_args = []
                if args.shared_templates:
                    repository = {key: oss[key] for key in ('endpoint', 'region', 'bucket', 'prefix')}
                    repository_path = args.output / 'template-repository.json'
                    repository_path.write_text(json.dumps(repository, indent=2))
                    os.chown(repository_path, lab.account.pw_uid, lab.account.pw_gid)
                    shared_args = ['--oss-env', str(args.oss_env), '--repository', str(repository_path),
                                   '--shared-mode', args.shared_templates]
                    config['environment']['kwargs']['template_repository'] = repository
                preparation = node.run(['env', 'AENV_API_KEY=' + API_KEY,
                    *(name + '=' + value for name, value in proxy_env.items()),
                    'PYTHONPATH=' + str(Path(__file__).resolve().parent),
                    str(args.harbor.parent / 'python'), str(Path(__file__).with_name('prepare_templates.py')),
                    '--job', str(config_path), '--assets', str(assets_path), '--run-id', run_id,
                    '--output', str(templates_path.parent), *shared_args], timeout=600 * len(task_names) + 60)
                report['template_preparation'] = json.loads(templates_path.read_text())
                assert json.loads(preparation.stdout)['passed'], 'Template preparation failed'
                config['environment']['kwargs'].update(templates_path=str(templates_path),
                    template_run_id=run_id, runtime_assets=report['runtime_sha256'])
                if args.shared_templates == 'reuse':
                    # This is the first sandbox in a new Lab. Do not cold-boot the
                    # image first: that would warm caches before proving OSS restore.
                    home = node.root / 'home'
                    report['cache_before_first_restore'] = {
                        path.relative_to(home).as_posix(): path.stat().st_size
                        for path in sorted(home.rglob('*')) if path.is_file()}
                    report['transport_probe_start_mode'] = 'template'
                    config_path.write_text(json.dumps(config, indent=2))
                    os.chown(config_path, lab.account.pw_uid, lab.account.pw_gid)
                    transport_probe()
            observed = []
            phases = [('dragonball-controls' if benchmark or computer else 'dragonball-smoke', config['agents'])]
            if args.model_only:
                phases = []
            if args.model and model_task_names:
                phases.append(('dragonball-model', [computer_agent_config(args.model) if computer else model_agent_config(args.model)]))
            report['harbor_exit_codes'] = {}
            for job_name, agents in phases:
                phase_tasks = model_task_names if job_name == 'dragonball-model' else task_names
                config.update(job_name=job_name, agents=agents,
                    tasks=[{'path': str(args.prepared / 'tasks' / name)} for name in phase_tasks])
                if job_name == 'dragonball-model':
                    config['environment']['kwargs'].update(material_manifest=args.material_manifest,
                                                            material_required=True)
                config_path = args.output / (job_name + '.json')
                config_path.write_text(json.dumps(config, indent=2))
                os.chown(config_path, lab.account.pw_uid, lab.account.pw_gid)
                command = ['env', 'AENV_API_KEY=' + API_KEY, 'HOME=' + lab.account.pw_dir,
                    'PYTHONPATH=' + str(Path(__file__).resolve().parent),
                    *(name + '=' + value for name, value in proxy_env.items()),
                    str(args.harbor), 'run', '--config', str(config_path)]
                # Only the model CLI inherits this credential, never fixtures or VM env.
                if job_name == 'dragonball-model':
                    os.environ['OPENAI_API_KEY'] = model_api_key
                try:
                    child = node.spawn(command, args.output / (job_name + '.log'))
                finally:
                    if job_name == 'dragonball-model':
                        os.environ.pop('OPENAI_API_KEY', None)
                budget = job_budget(phase_budget if full else (2400 if benchmark or computer else 600),
                                    len(phase_tasks), len(agents), resource_plan['concurrency'])
                report.setdefault('job_budget_sec', {})[job_name] = budget
                deadline = time.monotonic() + budget
                while child.poll() is None and time.monotonic() < deadline:
                    live = []
                    for pid in (node.cgroup / 'cgroup.procs').read_text().split():
                        proc = Path('/proc') / pid
                        row = observe_vmm(proc, assets['runner_path'], lab.account.pw_uid)
                        if row:
                            live.append(row['pid'])
                        if row and row['pid'] not in [item['pid'] for item in observed]:
                            observed.append(row)
                    if len(live) > report.get('max_live_vmm_count', 0):
                        report.update(max_live_vmm_count=len(live), peak_vmm_pids=live)
                    time.sleep(.1)
                if child.poll() is None:
                    raise TimeoutError('Harbor job exceeded its deadline; owned cgroup will be cleaned')
                report['harbor_exit_codes'][job_name] = child.returncode
                report['results'] = [json.loads(path.read_text()) for path in
                                    (args.output / 'jobs').glob('*/*/result.json')]
                report["vmm_observed"] = observed
                report["sandbox_receipts"] = [json.loads(path.read_text()) for path in
                    (args.output / "jobs").rglob("agentenv.json")]
                report["remaining_sandboxes"] = api(node, "/sandboxes")
                assert child.returncode == 0, 'Harbor CLI failed; see ' + job_name + '.log'
                if (benchmark or computer) and job_name == 'dragonball-controls':
                    controls = [json.loads(path.read_text()) for path in
                                (args.output / 'jobs' / job_name).glob('*/result.json')]
                    if computer:
                        validate_computer_results(controls)
                    else:
                        validate_benchmark_results(controls, task_names=task_names,
                                                   strict_oracle=not full or not args.model)
                        if full:
                            report['oracle_diagnostics'] = [row for row in controls if
                                row['agent_info']['name'] == 'oracle' and
                                row['verifier_result']['rewards']['reward'] != 1]
            results = report["results"]
            receipts = report["sandbox_receipts"]
            if computer:
                validate_computer_results(results, model=bool(args.model))
            elif benchmark:
                validate_benchmark_results(results, model=bool(args.model), task_names=task_names,
                    strict_oracle=not full or not args.model, controls=not args.model_only,
                    model_task_names=model_task_names)
            else:
                validate_results(results)
            assert observed, "No actual Dragonball VMM observed"
            assert len(receipts) == len(results) and all(r["state"] == "deleted" for r in receipts)
            validate_receipt_resources(resource_plan, prepared['images'], receipts)
            assert not report["remaining_sandboxes"], "Job left a sandbox alive"
            report["passed"] = True
            exit_code = 0
    except BaseException as error:
        report["error"] = repr(error)
    finally:
        errors = []
        if lab is not None:
            report['node_memory_events'] = {node.name: (node.cgroup / 'memory.events').read_text()
                                            for node in lab.nodes if node.cgroup.exists()}
        for action in [item.close for item in (services, lab) if item is not None]:
            try:
                action()
            except Exception as error:
                errors.append(str(error))
        for node in lab.nodes if lab is not None else []:
            try:
                cleanup_owned_ublk(node, inputs, receipt_path=node.root / "ublk-cleanup.json")
            except Exception as error:
                errors.append(str(error))
        report["cleanup"] = "passed" if not errors else errors
        report["finished"] = time.time()
        report["disk_after_job"] = shutil.disk_usage(root)._asdict()
        if errors:
            report["passed"] = False
            exit_code = 1
        write_report(args.output, report)
        print(json.dumps({"root": str(root), "passed": report["passed"],
            "cleanup": report["cleanup"], "error": report.get("error")}), flush=True)
    return exit_code


def main(argv=None):
    from dragonball_m4_host import host_lock
    with host_lock():
        return _main(argv)


if __name__ == "__main__":
    raise SystemExit(main())
