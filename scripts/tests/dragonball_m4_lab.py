#!/usr/bin/env python3
"""Disposable single-host M4 lab. Root orchestrates; workloads run as aenv.

No host routes or firewall changes. Call inside an unshared network/mount
namespace. State and logs survive close; only owned processes/cgroups go away.
"""
import json
import os
from pathlib import Path
import pwd
import signal
import subprocess
import sys
import time
import urllib.request


def command(argv, **kwargs):
    return subprocess.run(argv, check=True, text=True, capture_output=True,
                          timeout=kwargs.pop("timeout", 30), **kwargs)


def require_controllers(controllers):
    missing = {"cpu", "memory", "pids"} - set(controllers.split())
    if missing:
        raise RuntimeError("missing cgroup controllers: " + ", ".join(sorted(missing)))


def wait_http(url, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                return response.read()
        except OSError as error:
            last = error
            time.sleep(0.1)
    raise TimeoutError(f"HTTP readiness failed: {url}: {last}")


def empty_cgroup(group):
    if not group.exists():
        return
    (group / "cgroup.kill").write_text("1")
    deadline = time.monotonic() + 15
    while "populated 1" in (group / "cgroup.events").read_text():
        if time.monotonic() > deadline:
            raise TimeoutError(f"cgroup remained populated: {group}")
        time.sleep(0.05)


def delete_link(name):
    try:
        command(["ip", "link", "delete", name])
    except subprocess.CalledProcessError:
        # Kernel namespace teardown can remove both veth ends concurrently.
        if subprocess.run(["ip", "link", "show", name], capture_output=True).returncode == 0:
            raise


class Lab:
    def __init__(self, root: Path, run_user: str):
        parent = os.environ.get("AENV_M4_PARENT_NET")
        if not parent or not Path("/proc/self/ns/net").exists() or parent == os.readlink("/proc/self/ns/net"):
            raise RuntimeError("lab requires a private network namespace")
        if os.geteuid() != 0:
            raise RuntimeError("root orchestration required")
        account = pwd.getpwnam(run_user)
        if account.pw_uid == 0:
            raise ValueError("workloads must be non-root")
        self.root = root.resolve(strict=True)
        if self.root.parent != Path("/var/tmp") or not self.root.name.startswith("db-m4.") or any(self.root.iterdir()):
            raise ValueError("use an empty mkdtemp /var/tmp/db-m4.* root")
        self.run_user = run_user
        self.account = account
        self.nodes = []
        self.closed = False
        self.bridge_created = False
        base = Path("/sys/fs/cgroup")
        controllers = (base / "cgroup.subtree_control").read_text()
        require_controllers(controllers)
        self.cgroup = base / self.root.name
        self.cgroup.mkdir()
        try:
            (self.cgroup / "cgroup.subtree_control").write_text(
                "+cpu +memory +pids" + (" +io" if "io" in controllers.split() else ""))
            self.root.chmod(0o755)
            for name in ("a", "b", "repo"):
                directory = self.root / name
                directory.mkdir()
                os.chown(directory, account.pw_uid, account.pw_gid)
            command(["ip", "link", "set", "lo", "up"])
            command(["ip", "link", "add", "m4br", "type", "bridge"])
            self.bridge_created = True
            command(["ip", "addr", "add", "10.241.0.1/24", "dev", "m4br"])
            command(["ip", "link", "set", "m4br", "up"])
        except BaseException:
            self.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def start_node(self, name):
        if name not in ("a", "b") or any(n.name == name for n in self.nodes):
            raise ValueError("node must be a unique a or b")
        node = Node(self, name)
        self.nodes.append(node)
        node.start()
        return node

    def close(self):
        if self.closed:
            return
        errors = []
        for node in reversed(self.nodes):
            try:
                node.close()
            except Exception as error:
                errors.append(str(error))
        try:
            empty_cgroup(self.cgroup)
            self.cgroup.rmdir()
        except Exception as error:
            errors.append(str(error))
        if self.bridge_created:
            try:
                command(["ip", "link", "delete", "m4br"])
            except Exception as error:
                errors.append(str(error))
        if errors:
            raise RuntimeError("lab cleanup: " + "; ".join(errors))
        self.closed = True


class Node:
    def __init__(self, lab, name):
        self.lab, self.name = lab, name
        self.root = lab.root / name
        self.address = "10.241.0." + ("11" if name == "a" else "12")
        self.cgroup = lab.cgroup / name
        self.processes = []
        self.init = None
        self.pid = None
        self.link_created = False
        self.host_link = "m4" + name

    def _join_cgroup(self):
        (self.cgroup / "cgroup.procs").write_text(str(os.getpid()))

    def start(self):
        self.cgroup.mkdir()
        for key, value in {"memory.max": str(12 * 1024**3), "memory.swap.max": "0",
                           "cpu.max": "400000 100000", "pids.max": "2048"}.items():
            (self.cgroup / key).write_text(value)
        with (self.lab.root / (self.name + ".init.log")).open("w") as log:
            self.init = subprocess.Popen([
                "unshare", "--mount", "--net", "--pid", "--fork", "--kill-child=KILL",
                "--mount-proc", "--propagation", "private", sys.executable,
                str(Path(__file__).resolve()), "--node-init", str(self.lab.root), self.name,
            ], stdout=log, stderr=subprocess.STDOUT, preexec_fn=self._join_cgroup)
        deadline = time.monotonic() + 15
        while not (self.root / "ready").exists():
            if self.init.poll() is not None:
                raise RuntimeError((self.lab.root / (self.name + ".init.log")).read_text())
            if time.monotonic() > deadline:
                raise TimeoutError("node namespace readiness")
            time.sleep(0.05)
        children = Path(f"/proc/{self.init.pid}/task/{self.init.pid}/children").read_text().split()
        if len(children) != 1:
            raise RuntimeError("expected one namespace init")
        self.pid = int(children[0])
        command(["ip", "link", "add", self.host_link, "type", "veth", "peer", "name", "m4peer"])
        self.link_created = True
        command(["ip", "link", "set", "m4peer", "netns", str(self.pid)])
        command(["ip", "link", "set", self.host_link, "master", "m4br"])
        command(["ip", "link", "set", self.host_link, "up"])
        for args in (["link", "set", "m4peer", "name", "eth0"],
                     ["addr", "add", self.address + "/24", "dev", "eth0"],
                     ["link", "set", "eth0", "up"]):
            command(["nsenter", "-t", str(self.pid), "-n", "ip", *args])

    def _argv(self, argv):
        wrapper = Path(__file__).resolve().parents[1] / "run-with-capabilities.sh"
        return ["nsenter", "-t", str(self.pid), "-m", "-n", "-p", "--",
                "env", "AENV_RUN_USER=" + self.lab.run_user,
                "AENV_HOME_PATH=" + str(self.root / "home"),
                "AENV_RUNTIME_PATH=" + str(self.root / "run"),
                "AENV_DEPS_PATH=" + str(self.root / "deps"),
                "TMPDIR=" + str(self.root / "tmp"),
                "bash", str(wrapper), *argv]

    def spawn(self, argv, log):
        with Path(log).open("w") as output:
            child = subprocess.Popen(self._argv(argv), stdout=output, stderr=subprocess.STDOUT,
                                     preexec_fn=self._join_cgroup, start_new_session=True)
        self.processes.append(child)
        return child

    def run(self, argv, timeout=30):
        child = subprocess.Popen(self._argv(argv), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, preexec_fn=self._join_cgroup, start_new_session=True)
        self.processes.append(child)
        try:
            stdout, stderr = child.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Parent-side PGID includes nsenter's child and normal grandchildren.
            # Node close additionally kills the entire cgroup, including setsid children.
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate(timeout=5)
            raise
        result = subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)
        result.check_returncode()
        return result

    def disconnect(self):
        command(["ip", "link", "set", self.host_link, "down"])

    def reconnect(self):
        command(["ip", "link", "set", self.host_link, "up"])

    def close(self):
        empty_cgroup(self.cgroup)
        for child in self.processes + ([self.init] if self.init else []):
            child.wait(timeout=10)
        if self.cgroup.exists():
            self.cgroup.rmdir()
        # Deleting the peer namespace normally deletes both ends of the veth.
        if self.link_created:
            delete_link(self.host_link)


def node_init(root, name):
    temporary = root / name / "tmp"
    temporary.mkdir(mode=0o1777)
    temporary.chmod(0o1777)
    command(["mount", "--bind", str(temporary), "/tmp"])
    for peer in ("a", "b", "repo"):
        if peer != name:
            command(["mount", "-t", "tmpfs", "-o", "size=4k,mode=000", "tmpfs", str(root / peer)])
    command(["mount", "-t", "sysfs", "-o", "ro,nosuid,nodev,noexec", "sysfs", "/sys"])
    command(["ip", "link", "set", "lo", "up"])
    Path("/proc/sys/net/ipv4/ip_forward").write_text("1")
    (root / name / "ready").write_text(json.dumps({n: os.readlink("/proc/self/ns/" + n)
                                                for n in ("net", "mnt", "pid")}))
    while True:
        try:
            os.waitpid(-1, 0)
        except ChildProcessError:
            time.sleep(0.1)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--node-init":
        node_init(Path(sys.argv[2]), sys.argv[3])
    elif sys.argv[1:] == ["--self-test"]:
        raise SystemExit(subprocess.call([sys.executable, str(Path(__file__).with_name("test_dragonball_m4_lab.py")), "--linux"]))
    else:
        raise SystemExit("usage: dragonball_m4_lab.py --self-test")
