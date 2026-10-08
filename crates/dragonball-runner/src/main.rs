fn main() {
    if let Err(error) = run() {
        eprintln!("aenv-dragonball: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn reserve_fd_table() -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // The common block/net/balloon profile exceeds Linux's initial 64 slots.
    // Expand while single-threaded: expansion after CLONE_FILES can wait for
    // an RCU grace period in a device's otherwise cheap eventfd allocation.
    // This is a capacity hint, not an FD limit or a permanently held FD.
    let source = std::fs::File::open("/dev/null")?;
    let fd = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 127) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new descriptor owned exclusively by this call.
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
    Ok(())
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version-json" {
        println!(
            "{}",
            serde_json::to_string(&agentenv_dragonball_runner::build_identity())
                .expect("serialize build identity")
        );
        return Ok(());
    }
    let runtime = args.first().is_some_and(|arg| arg == "--runtime");
    if runtime {
        args.remove(0);
    }
    if runtime && args.len() == 1 && args[0] == "--version-json" {
        use agentenv_vmm_protocol::runtime::{RuntimeBuildIdentity, RUNTIME_FEATURE_PROFILE};
        let identity = agentenv_dragonball_runner::runtime_service::restore_identity(
            String::new(),
            String::new(),
            String::new(),
        );
        println!(
            "{}",
            serde_json::to_string(&RuntimeBuildIdentity {
                runner_build_id: identity.runner_build_id,
                dragonball_revision: identity.dragonball_revision,
                snapshot_format_epoch: identity.snapshot_format_epoch,
                restore_abi_id: identity.restore_abi_id,
                feature_profile: RUNTIME_FEATURE_PROFILE.into(),
            })?
        );
        return Ok(());
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use agentenv_dragonball_runner::{
            process::{install_parent_death_guard, PrivateListener},
            service::RunnerService,
            vmm::DragonballVmm,
        };
        use std::path::PathBuf;
        let expected_parent = unsafe { libc::getppid() } as u32;
        let mut socket = None;
        let mut work_dir = None;
        let mut instance_id = None;
        let mut parent_pid = None;
        anyhow::ensure!(args.len() % 2 == 0, "expected option/value pairs");
        for pair in args.as_chunks::<2>().0 {
            match pair[0].to_str() {
                Some("--socket") if socket.is_none() => socket = Some(PathBuf::from(&pair[1])),
                Some("--work-dir") if work_dir.is_none() => {
                    work_dir = Some(PathBuf::from(&pair[1]))
                }
                Some("--instance-id") if instance_id.is_none() => {
                    instance_id = Some(
                        pair[1]
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("instance ID must be UTF-8"))?
                            .to_string(),
                    )
                }
                Some("--parent-pid") if parent_pid.is_none() => {
                    parent_pid = Some(
                        pair[1]
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("parent PID must be UTF-8"))?
                            .parse::<u32>()?,
                    )
                }
                _ => anyhow::bail!("unknown or duplicate runner option"),
            }
        }
        let instance_id = instance_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("--instance-id is required"))?;
        let socket = socket.ok_or_else(|| anyhow::anyhow!("--socket is required"))?;
        let work_dir = work_dir.ok_or_else(|| anyhow::anyhow!("--work-dir is required"))?;
        install_parent_death_guard(parent_pid.unwrap_or(expected_parent))?;
        let listener = PrivateListener::bind(&socket, &work_dir)?;
        let runner_instance_id = if runtime {
            use std::io::Read;
            let mut nonce = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        } else {
            String::new()
        };
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init()
            .map_err(|error| anyhow::anyhow!("initialize runner diagnostics: {error}"))?;
        if let Err(error) = reserve_fd_table() {
            // A low RLIMIT_NOFILE must not make this optimization a new
            // startup requirement; ordinary allocation retains its behavior.
            tracing::debug!(%error, "runner FD table reservation unavailable");
        }
        agentenv_dragonball_runner::seccomp::install_m0_filter()?;
        let vmm = DragonballVmm::new(&instance_id, &work_dir)?;
        if runtime {
            let mut service = agentenv_dragonball_runner::runtime_service::RuntimeService::new(
                instance_id,
                runner_instance_id,
                vmm,
            );
            listener.serve_runtime(&mut service)?;
        } else {
            let mut service = RunnerService::new(instance_id, vmm);
            listener.serve(&mut service)?;
        }
        Ok(())
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    anyhow::bail!("M0 runner requires Linux x86_64/KVM; only --version-json is portable")
}
