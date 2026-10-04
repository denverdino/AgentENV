use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use tokio::process::Child;

/// Shares the actual unreaped child, never just a reusable numeric PID.
#[derive(Clone)]
pub(super) struct OwnedRunnerChild(Arc<Mutex<Child>>);

impl From<Child> for OwnedRunnerChild {
    fn from(child: Child) -> Self {
        Self(Arc::new(Mutex::new(child)))
    }
}

impl OwnedRunnerChild {
    pub(super) fn id(&self) -> Option<u32> {
        self.0.lock().ok().and_then(|child| child.id())
    }

    pub(super) fn try_wait(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0
            .try_lock()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?
            .try_wait()
    }

    pub(super) fn read_exact_remote(
        &self,
        requested_pid: u32,
        address: u64,
        dst: &mut [u8],
    ) -> Result<()> {
        let mut child = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("runner owner poisoned"))?;
        anyhow::ensure!(
            child.id() == Some(requested_pid),
            "memory reader PID does not own this runner"
        );
        anyhow::ensure!(
            child.try_wait()?.is_none(),
            "memory reader runner has exited"
        );
        // Holding the actual Child across the syscall prevents reap/PID reuse.
        crate::sandbox::process_vm_reader::ProcessVmReader::new(nix::unistd::Pid::from_raw(
            i32::try_from(requested_pid)?,
        ))
        .read_exact_remote(address, dst)
    }

    fn terminate_step(&self) -> Result<bool> {
        let mut child = match self.0.try_lock() {
            Ok(child) => child,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => anyhow::bail!("runner owner poisoned; retaining leases"),
        };
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        child.start_kill()?;
        Ok(false)
    }

    pub(super) async fn terminate_and_wait(&self, deadline: Duration) -> Result<()> {
        let until = std::time::Instant::now() + deadline;
        loop {
            if self.terminate_step()? {
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < until,
                "runner termination not confirmed; retaining leases"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

pub(super) async fn spawn_runner(
    mut command: tokio::process::Command,
    namespace: Option<std::fs::File>,
) -> Result<RunnerProcessGuard> {
    use std::os::fd::AsRawFd;
    command.kill_on_drop(true);
    // A short-lived launcher thread can trigger the runner's PDEATHSIG.
    // Spawn from the owning runtime thread; its child hook does ONLY syscalls.
    // linux-cap explicitly guarantees allocation-free pre_exec suitability.
    unsafe {
        command.pre_exec(move || {
            if let Some(namespace) = &namespace {
                linux_cap::configure_current_process_capabilities(&[
                    crate::privileges::CAP_SYS_ADMIN,
                ])?;
                if libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            linux_cap::configure_current_process_capabilities(&[])
        });
    }
    let child = command.spawn()?;
    Ok(RunnerProcessGuard {
        child: child.into(),
        #[cfg(test)]
        fail_blocking_reap: false,
    })
}

pub(super) struct RunnerProcessGuard {
    pub(super) child: OwnedRunnerChild,
    #[cfg(test)]
    pub(super) fail_blocking_reap: bool,
}

impl RunnerProcessGuard {
    pub(super) async fn terminate_and_wait(&mut self, deadline: Duration) -> Result<()> {
        self.child.terminate_and_wait(deadline).await
    }

    // Drop cannot await; retain all resource leases until this confirms exit.
    pub(super) fn terminate_blocking(&mut self, deadline: Duration) -> Result<()> {
        #[cfg(test)]
        anyhow::ensure!(!self.fail_blocking_reap, "injected unconfirmed runner exit");
        let until = std::time::Instant::now() + deadline;
        loop {
            if self.child.terminate_step()? {
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < until,
                "runner termination not confirmed; retaining leases"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn m2_process_vm_readv_rejects_wrong_process() {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owner = OwnedRunnerChild::from(child);
        let bytes = b"not-the-owned-runner";
        let mut output = vec![0; bytes.len()];
        assert!(
            owner
                .read_exact_remote(std::process::id(), bytes.as_ptr() as u64, &mut output)
                .is_err(),
            "an address in another readable process must not bypass the runner owner"
        );
    }

    #[tokio::test]
    async fn m2_process_owner_cannot_reap_during_read_guard() {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owner = OwnedRunnerChild::from(child);
        let reader = owner.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let reading = tokio::task::spawn_blocking(move || {
            let _child = reader.0.lock().unwrap();
            entered.send(()).unwrap();
            let _ = proceed.recv();
        });
        ready.await.unwrap();
        assert!(owner
            .terminate_and_wait(Duration::from_millis(20))
            .await
            .is_err());
        assert_eq!(
            owner.try_wait().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        release.send(()).unwrap();
        reading.await.unwrap();
        owner
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(owner.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn m2_process_vm_readv_reads_owned_child_without_ptrace_capability() {
        use tokio::io::AsyncBufReadExt;
        assert_ne!(
            unsafe { libc::geteuid() },
            0,
            "run build/tests as a non-root user"
        );
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let caps = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:\t"))
            .unwrap();
        assert_eq!(u64::from_str_radix(caps, 16).unwrap() & (1 << 19), 0);
        let mut child = tokio::process::Command::new("python3").args(["-c",
            "import ctypes,time; b=ctypes.create_string_buffer(b'owned-runner-memory'); print(ctypes.addressof(b),flush=True); time.sleep(60)"])
            .stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut address = String::new();
        tokio::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .await
            .unwrap();
        let address: u64 = address.trim().parse().unwrap();
        let pid = child.id().unwrap();
        let owner = OwnedRunnerChild::from(child);
        let reader = owner.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let mut bytes = [0; 19];
            reader.read_exact_remote(pid, address, &mut bytes).unwrap();
            bytes
        })
        .await
        .unwrap();
        assert_eq!(&bytes, b"owned-runner-memory");
        owner
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(owner.read_exact_remote(pid, address, &mut [0]).is_err());
    }

    #[tokio::test]
    async fn runner_parent_thread_remains_alive_until_exit() {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("60");
        // Install before exec to deterministically expose a short-lived launcher.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut guard = spawn_runner(command, None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let prematurely_exited = guard.child.try_wait().unwrap().is_some();
        guard
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            !prematurely_exited,
            "launcher thread exit killed the runner before its owner stopped it"
        );
    }

    #[tokio::test]
    async fn failed_spawn_releases_resources_after_exit() {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut guard = RunnerProcessGuard {
            child: child.into(),
            fail_blocking_reap: false,
        };
        guard
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            guard.child.try_wait().unwrap().is_some(),
            "must confirm exit before releasing leases"
        );
    }
}
