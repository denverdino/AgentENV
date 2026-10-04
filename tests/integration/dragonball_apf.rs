//! Explicit host-trace qualification, separate from the ordinary M2 matrix.
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    time::Duration,
};

use agentenv::sandbox::{
    dragonball::{DragonballMemoryMode, DragonballRuntimePolicy, DragonballSandboxFactory},
    SandboxBackend, SandboxExecutor,
};
use anyhow::{Context, Result};

use super::dragonball_m1::{asset, fixture, launch, spec};
use super::dragonball_m2::memory_device;

struct StoppedDaemon(OwnedFd);

impl StoppedDaemon {
    fn new(pid: i32) -> Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        anyhow::ensure!(fd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
        let owner = Self(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        anyhow::ensure!(owner.signal(libc::SIGSTOP) == 0, "stop owned test daemon");
        Ok(owner)
    }

    fn signal(&self, signal: i32) -> libc::c_long {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        }
    }
}

impl Drop for StoppedDaemon {
    fn drop(&mut self) {
        // Always reopen the I/O gate on assertion failure/cancellation; pidfd
        // cannot accidentally resume an unrelated reused process ID.
        self.signal(libc::SIGCONT);
    }
}

#[tokio::test]
#[ignore = "requires root-collected KVM APF trace; run separately with a private test daemon"]
async fn dragonball_apf_runner_death_during_blocked_memory_io() -> Result<()> {
    let trace = PathBuf::from(
        std::env::var_os("AENV_APF_TRACE_LOG").context("explicit APF trace log prerequisite")?,
    );
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(
        DragonballRuntimePolicy::from_app_config(
            &app,
            asset(&assets, "runner_path")?,
            asset(&assets, "kernel_path")?,
        )?
        .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
    )?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let access = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let output = source.run_command("/bin/sh", &["-ec", "dd if=/dev/urandom of=/dev/shm/apf-payload bs=1M count=64; sha256sum /dev/shm/apf-payload"]).await?;
    anyhow::ensure!(output.exit_code == 0, "payload: {}", output.stderr);
    let paused = source.pause(None).await?;
    let memory = PathBuf::from(
        paused.encode()?["memory_image_config_path"]
            .as_str()
            .unwrap(),
    );
    source.stop().await?;
    drop(source);
    let mut restored = factory.build_paused(id, paused.as_ref(), access)?;
    restored.start().await?;
    drop(paused);
    let device = memory_device(&restored).await?;
    let peer = tokio::net::UnixStream::connect(restored.work_dir()?.join("runner.sock")).await?;
    let runner_pid = peer.peer_cred()?.pid().context("owned runner PID")?;
    drop(peer);
    let tids = std::fs::read_dir(format!("/proc/{runner_pid}/task"))?
        .map(|entry| entry.map(|entry| format!("-{} ", entry.file_name().to_string_lossy())))
        .collect::<std::io::Result<Vec<_>>>()?;
    let daemon = tokio::net::UnixStream::connect(&app.ublk.daemon_socket_path).await?;
    let daemon_pid = daemon.peer_cred()?.pid().context("private daemon PID")?;
    let args = std::fs::read(format!("/proc/{daemon_pid}/cmdline"))?;
    anyhow::ensure!(
        String::from_utf8_lossy(&args).contains(app.ublk.daemon_socket_path.to_str().unwrap()),
        "daemon must belong to this isolated fixture"
    );
    drop(daemon);
    let start = std::fs::metadata(&trace)?.len() as usize;
    let gate = StoppedDaemon::new(daemon_pid)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut stopped = true;
            for task in std::fs::read_dir(format!("/proc/{daemon_pid}/task"))? {
                let status = std::fs::read_to_string(task?.path().join("status"))?;
                stopped &= status
                    .lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'));
            }
            if stopped {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("daemon threads did not stop")??;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) },
        0
    );
    let stopped_at = now.tv_sec as f64 + now.tv_nsec as f64 / 1_000_000_000.0;
    let observed = async {
        loop {
            let data = std::fs::read_to_string(&trace)?;
            if let Some(line) = data.get(start..).unwrap_or_default().lines().find(|line| {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                let event = fields
                    .iter()
                    .position(|field| *field == "kvm_async_pf_not_present:");
                event
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| fields[index].trim_end_matches(':').parse::<f64>().ok())
                    .is_some_and(|time| time >= stopped_at)
                    && tids.iter().any(|tid| line.contains(tid))
            }) {
                return Ok::<_, anyhow::Error>(line.to_owned());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    {
        let read = restored.run_command("/bin/sh", &["-ec", "sha256sum /dev/shm/apf-payload"]);
        tokio::pin!(read);
        let event = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(15), observed) => result.context("no actual APF while daemon I/O blocked")??,
            result = &mut read => anyhow::bail!("guest read completed before blocked APF: {result:?}"),
        };
        eprintln!("owned runner {runner_pid}, stopped daemon {daemon_pid}: {event}");
    }
    anyhow::ensure!(
        std::fs::read_to_string(format!("/proc/{daemon_pid}/status"))?
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T')),
        "I/O gate not stopped"
    );
    assert_eq!(unsafe { libc::kill(runner_pid, libc::SIGKILL) }, 0);
    {
        let stop = restored.stop();
        tokio::pin!(stop);
        let reaped = async {
            while std::path::Path::new(&format!("/proc/{runner_pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(10), reaped) => result.context("runner not reaped with APF pending")?,
            result = &mut stop => anyhow::bail!("teardown completed before blocked I/O drained: {result:?}"),
        }
        assert!(
            memory.exists(),
            "backing vanished while I/O was still blocked"
        );
        drop(gate);
        tokio::time::timeout(Duration::from_secs(20), &mut stop)
            .await
            .context("teardown after I/O drain")??;
    }
    drop(restored);
    tokio::time::timeout(Duration::from_secs(15), async {
        while memory.exists() || device.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("old memory generation not retired after APF drain")?;
    Ok(())
}
