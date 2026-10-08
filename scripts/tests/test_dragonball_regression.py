#!/usr/bin/env python3
"""Behavioral tests for frozen functional-suite regression receipts (no VM required)."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("dragonball_regression.py")


class RegressionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        # A subprocess fixture at the libtest boundary: writes actual executions,
        # can fail a stage once, and reports the same summary protocol as libtest.
        self.binary = self.root / "integration"
        self.binary.write_text("""#!/usr/bin/env python3
import pathlib, sys
root = pathlib.Path(__file__).parent
stage = next(a for a in sys.argv if a.startswith('dragonball_'))
if '--list' in sys.argv:
    if not (root / 'empty').exists():
        print(stage + '::case: test')
    sys.exit(0)
with (root / 'calls').open('a') as f:
    f.write(stage + '\\n')
if (root / 'fail').exists() and stage == 'dragonball_memory::':
    print('original failure')
    sys.exit(1)
if (root / 'false-green').exists():
    print('test result: ok. 0 passed; 0 failed; 0 ignored; 1 filtered out')
else:
    print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out')
""")
        self.binary.chmod(0o755)
        self.output = self.root / "results"

    def module(self):
        self.assertTrue(SCRIPT.exists(), "resumable regression entrypoint is missing")
        spec = importlib.util.spec_from_file_location("regression", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def run_stages(self, resume=False, identity=None):
        return self.module().run_stages(
            self.binary, self.output, ["lifecycle", "memory"], identity or {"candidate": "one"},
            resume=resume, check_inputs=lambda: identity or {"candidate": "one"},
        )

    def test_functional_suites_select_disjoint_module_filters(self):
        module = self.module()
        names = [
            "dragonball_lifecycle::dragonball_memory_oracle",
            "dragonball_memory::case", "dragonball_slots::case",
            "dragonball_startup::case", "dragonball_compatibility::case",
        ]
        selected = [[name for name in names if any(f in name for f in filters)]
                    for filters in module.SUITES.values()]
        self.assertEqual(sorted(name for group in selected for name in group), sorted(names))
        self.assertEqual(module.SUITES["lifecycle"], ["dragonball_lifecycle::"])
        self.assertNotIn("compatibility", module.DEFAULT_SUITES)
        self.assertNotIn("platform", module.DEFAULT_SUITES)

    def test_resume_runs_failed_stage_only_and_retains_original_failure(self):
        (self.root / "fail").touch()
        self.assertNotEqual(self.run_stages(), 0)
        (self.root / "fail").unlink()
        self.assertEqual(self.run_stages(resume=True), 0)
        self.assertEqual((self.root / "calls").read_text().splitlines(),
                         ["dragonball_lifecycle::", "dragonball_memory::", "dragonball_memory::"])
        self.assertIn("original failure", (self.output / "memory-1.log").read_text())
        receipts = json.loads((self.output / "results.json").read_text())
        self.assertEqual([r["status"] for r in receipts["attempts"]],
                         ["passed", "failed", "passed"])

    def test_changed_input_refuses_reuse_before_any_execution(self):
        self.assertEqual(self.run_stages(), 0)
        with self.assertRaisesRegex(ValueError, "inputs changed"):
            self.run_stages(resume=True, identity={"candidate": "two"})
        self.assertEqual(len((self.root / "calls").read_text().splitlines()), 2)

    def test_zero_matching_tests_never_pass(self):
        (self.root / "empty").touch()
        with self.assertRaisesRegex(ValueError, "no tests"):
            self.run_stages()
        self.assertFalse((self.root / "calls").exists())

    def test_exit_zero_with_zero_executed_tests_is_failure(self):
        (self.root / "false-green").touch()
        self.assertNotEqual(self.run_stages(), 0)
        self.assertEqual((self.root / "calls").read_text().splitlines(), ["dragonball_lifecycle::"])

    def test_input_change_during_execution_invalidates_pass(self):
        module = self.module()
        self.assertNotEqual(module.run_stages(
            self.binary, self.output, ["lifecycle"], {"candidate": "one"},
            resume=False, check_inputs=lambda: {"candidate": "changed"},
        ), 0)
        receipt = json.loads((self.output / "results.json").read_text())
        self.assertNotEqual(receipt["attempts"][-1]["status"], "passed")

    def test_existing_output_requires_explicit_resume(self):
        self.assertEqual(self.run_stages(), 0)
        with self.assertRaisesRegex(ValueError, "resume"):
            self.run_stages()

    def test_truncated_pass_log_is_not_reused(self):
        self.assertEqual(self.run_stages(), 0)
        (self.output / "lifecycle-1.log").write_text("truncated")
        with self.assertRaisesRegex(ValueError, "evidence"):
            self.run_stages(resume=True)

    def test_incomplete_attempt_is_rerun_without_overwriting_log(self):
        self.assertEqual(self.run_stages(), 0)
        path = self.output / "results.json"
        data = json.loads(path.read_text())
        data["attempts"][-1]["status"] = "running"
        path.write_text(json.dumps(data))
        original = (self.output / "memory-1.log").read_bytes()
        self.assertEqual(self.run_stages(resume=True), 0)
        self.assertEqual((self.output / "memory-1.log").read_bytes(), original)
        self.assertTrue((self.output / "memory-2.log").exists())

    def test_missing_input_after_execution_cannot_leave_a_pass_receipt(self):
        module = self.module()
        def missing():
            raise ValueError("missing asset")
        with self.assertRaisesRegex(ValueError, "missing asset"):
            module.run_stages(self.binary, self.output, ["lifecycle"], {"candidate": "one"},
                              resume=False, check_inputs=missing)
        receipt = json.loads((self.output / "results.json").read_text())
        self.assertNotEqual(receipt["attempts"][-1]["status"], "passed")

    def test_timeout_stops_child_and_retains_attempt(self):
        self.binary.write_text(self.binary.read_text().replace(
            "with (root / 'calls').open('a') as f:",
            "import time\n(root / 'pid').write_text(str(__import__('os').getpid()))\ntime.sleep(60)\nwith (root / 'calls').open('a') as f:"))
        module = self.module()
        self.assertNotEqual(module.run_stages(
            self.binary, self.output, ["lifecycle"], {}, resume=False,
            check_inputs=lambda: {}, timeout=0.2), 0)
        pid = int((self.root / "pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        self.assertEqual(json.loads((self.output / "results.json").read_text())["attempts"][0]["status"], "interrupted")

    def test_concurrent_output_is_refused(self):
        import fcntl
        self.output.mkdir()
        with (self.output / ".lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(ValueError, "another regression"):
                self.run_stages()
        self.assertFalse((self.root / "calls").exists())

    def test_make_execution_requires_output_without_building(self):
        result = subprocess.run(["make", "test-dragonball-regression", "AENV_REGRESSION_OUTPUT="],
                                cwd=SCRIPT.parents[2], text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("AENV_REGRESSION_OUTPUT", result.stderr)
        self.assertNotIn("cargo", result.stdout)

    def test_make_run_uses_frozen_binary_without_invoking_cargo(self):
        recorder = self.root / "python-recorder"
        recorder.write_text("#!/usr/bin/env python3\nimport json,os,sys\nprint(json.dumps({'args':sys.argv[1:], 'home':os.environ['AENV_HOME_PATH'], 'state':os.environ['AENV_TEST_STATE_DIR']}))\n")
        recorder.chmod(0o755)
        cargo = self.root / "cargo-must-not-run"
        cargo.write_text("#!/bin/sh\nexit 79\n")
        cargo.chmod(0o755)
        result = subprocess.run([
            "make", "--silent", "test-dragonball-regression", f"CARGO={cargo}",
            f"REGRESSION_PYTHON={recorder}", f"AENV_TEST_STATE_DIR={self.root}/state",
            f"AENV_REGRESSION_OUTPUT={self.output}", "AENV_REGRESSION_ARGS=--resume"],
            cwd=SCRIPT.parents[2], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertEqual(data['args'], ['scripts/tests/dragonball_regression.py', '--binary',
                                       str(self.root / 'state/regression-integration'),
                                       '--output', str(self.output), '--resume'])
        self.assertEqual(data['home'], str(self.root / 'state/home'))
        self.assertEqual(data['state'], str(self.root / 'state'))

    @unittest.skipUnless(sys.platform == "linux", "requires Linux host identity and installed tools")
    def test_identity_tracks_asset_config_dependency_and_source_contents(self):
        module = self.module()
        source = self.root / "source"
        source.mkdir()
        subprocess.run(["git", "init", "-q", str(source)], check=True)
        (source / "tracked").write_text("original")
        subprocess.run(["git", "add", "."], cwd=source, check=True)
        subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                        "commit", "-qm", "fixture"], cwd=source, check=True)
        env = {key: str(self.root / key.lower()) for key in module.REQUIRED}
        env['AENV_HOME_PATH'] = str(Path(env['AENV_TEST_STATE_DIR']) / 'home')
        env['AENV_RUNTIME_PATH'] = str(Path(env['AENV_TEST_STATE_DIR']) / 'run')
        for key in ("AENV_TEST_STATE_DIR", "AENV_HOME_PATH", "AENV_RUNTIME_PATH", "AENV_DEPS_PATH"):
            Path(env[key]).mkdir()
        for key in ("AENV_DRAGONBALL_RUNTIME_RUNNER", "AENV_DRAGONBALL_FAULT_RUNNER", "AENV_UBLK_DAEMON_BINARY_PATH"):
            Path(env[key]).write_text("binary")
        Path(env["AENV_CONFIG_PATH"]).write_text("")
        for path in module.global_configs({}, self.root, env):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("{}")
        kernel, tools, lower, image = [self.root / name for name in ("kernel", "tools", "lower", "image.json")]
        for path in (kernel, tools, lower):
            path.write_text("asset")
        image.write_text(json.dumps({"lowers": [{"file": str(lower)}]}))
        Path(env["AENV_DRAGONBALL_M0_ASSETS"]).write_text(json.dumps({
            "runner_path": env["AENV_DRAGONBALL_RUNTIME_RUNNER"], "kernel_path": str(kernel),
            "tools_path": str(tools), "image_path": str(image)}))
        dependency = Path(env["AENV_DEPS_PATH"]) / "tool"
        dependency.write_text("dependency")
        original = module.collect_inputs(self.binary, env, source)
        for path in (kernel, lower, dependency, self.binary, source / "tracked",
                     module.global_configs({}, self.root, env)[0]):
            before = path.read_bytes()
            path.write_bytes(before + b" ")
            self.assertNotEqual(module.collect_inputs(self.binary, env, source), original, str(path))
            path.write_bytes(before)
        self.assertEqual(module.collect_inputs(self.binary, env, source), original)
        (source / "untracked").write_text("new source")
        self.assertNotEqual(module.collect_inputs(self.binary, env, source), original)
        (source / "untracked").unlink()
        lower.unlink()
        with self.assertRaisesRegex(ValueError, "input file"):
            module.collect_inputs(self.binary, env, source)

    def test_timeout_reaps_group_even_when_leader_exits_first(self):
        import signal
        module = self.module()
        stopped = self.root / "grandchild-stopped"
        pidfile = self.root / "grandchild-pid"
        grandchild = ("import os,signal,time,pathlib,sys; "
                      "signal.signal(signal.SIGINT, signal.SIG_IGN); "
                      f"signal.signal(signal.SIGTERM, lambda *_: (pathlib.Path({str(stopped)!r}).touch(), sys.exit(0))); "
                      f"pathlib.Path({str(pidfile)!r}).write_text(str(os.getpid())); time.sleep(60)")
        parent = f"import subprocess,sys,time; subprocess.Popen([sys.executable, '-c', {grandchild!r}]); time.sleep(60)"
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                module.execute([sys.executable, "-c", parent], self.root / "group.log", timeout=1, grace=0.2)
            self.assertTrue(stopped.exists(), "descendant survived leader exit without receiving SIGTERM")
        finally:
            if pidfile.exists():
                try:
                    os.kill(int(pidfile.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_make_resume_control_flag_is_not_a_runtime_input(self):
        module = self.module()
        env = {"AENV_HOME_PATH": "/tmp/isolated", "AENV_REGRESSION_ARGS": ""}
        original = module.environment_digest(env)
        env["AENV_REGRESSION_ARGS"] = "--resume"
        self.assertEqual(module.environment_digest(env), original)
        env["AENV_HOME_PATH"] = "/tmp/different"
        self.assertNotEqual(module.environment_digest(env), original)

    def test_preflight_missing_config_and_changed_environment(self):
        module = self.module()
        with self.assertRaisesRegex(ValueError, "AENV_CONFIG_PATH"):
            module.collect_inputs(self.binary, {}, self.root)
        with self.assertRaisesRegex(ValueError, "environment"):
            module.verify_environment("wrong", {"AENV_CONFIG_PATH": "/tmp/x"})

    def test_budget_and_socket_checks_reject_before_vm_start(self):
        module = self.module()
        with self.assertRaisesRegex(ValueError, "space"):
            module.check_capacity(self.root, 10**20, 1)
        with self.assertRaisesRegex(ValueError, "inode"):
            module.check_capacity(self.root, 0, 10**20)
        with self.assertRaisesRegex(ValueError, "socket"):
            module.check_socket_path(self.root / ("x" * 100))

    def test_missing_generated_config_is_detected(self):
        module = self.module()
        env = {"AENV_HOME_PATH": str(self.root), "AENV_RUNTIME_PATH": str(self.root / "run")}
        paths = module.global_configs({}, self.root, env)
        self.assertEqual(paths[0], self.root / "overlaybd/overlaybd-global.json")
        with self.assertRaisesRegex(ValueError, "input file"):
            module.file_digest(paths[0])



class FixtureOwnershipTests(unittest.TestCase):
    def test_fixture_validation_rejects_missing_escaped_and_symlinked_globals(self):
        import dragonball_regression as regression
        self.assertTrue(hasattr(regression, 'validate_fixtures'), 'isolated fixture preflight missing')
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory).resolve()
            home = state / 'home'
            home.mkdir()
            config = state / 'config.toml'
            config.write_text('')
            env = {'AENV_CONFIG_PATH': str(config), 'AENV_TEST_STATE_DIR': str(state),
                   'AENV_HOME_PATH': str(home), 'AENV_RUNTIME_PATH': str(state / 'run')}
            paths = regression.validate_fixtures(env, prepared=False)
            with self.assertRaisesRegex(ValueError, 'prepare-dragonball-regression'):
                regression.validate_fixtures(env)
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('{}')
                path.chmod(0o600)
            self.assertEqual(regression.validate_fixtures(env), paths)
            paths[0].unlink()
            paths[0].symlink_to(paths[1])
            with self.assertRaisesRegex(ValueError, 'owned|isolated'):
                regression.validate_fixtures(env)
            config.write_text('[ublk.overlaybd]\nglobal_config_path="/var/lib/aenv/global.json"\n')
            with self.assertRaisesRegex(ValueError, 'isolated'):
                regression.validate_fixtures(env, prepared=False)


if __name__ == "__main__":
    unittest.main(verbosity=2)
