//! Qualification-only transport faults around the real runtime service/VMM.
//! Never installed as a runtime asset. The socket and VMM share the owned PID;
//! node peer authentication and process-memory provenance remain unchanged.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run() -> anyhow::Result<()> {
    use agentenv_dragonball_runner::{
        process::install_parent_death_guard,
        runtime_service::{restore_identity, RuntimeService},
        vmm::DragonballVmm,
    };
    use agentenv_vmm_protocol::{
        encode_runtime_response, read_runtime_request,
        runtime::{RuntimeBuildIdentity, RuntimePhase, RUNTIME_FEATURE_PROFILE},
    };
    use anyhow::{ensure, Context};
    use std::{
        fs::{self, OpenOptions},
        io::{Read, Write},
        os::fd::AsRawFd,
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        path::PathBuf,
        time::{Duration, Instant},
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.first().is_some_and(|arg| arg == "--runtime"),
        "runtime fixture only"
    );
    if args.len() == 2 && args[1] == "--version-json" {
        let identity = restore_identity(String::new(), String::new(), String::new());
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
    ensure!(args.len() == 9, "expected four runtime option/value pairs");
    let option = |name: &str| -> anyhow::Result<&std::ffi::OsStr> {
        args[1..]
            .as_chunks::<2>()
            .0
            .iter()
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_os_str())
            .with_context(|| format!("missing {name}"))
    };
    let work = PathBuf::from(option("--work-dir")?);
    let socket = PathBuf::from(option("--socket")?);
    let instance = option("--instance-id")?
        .to_str()
        .context("UTF-8 instance")?
        .to_owned();
    let parent = option("--parent-pid")?
        .to_str()
        .context("UTF-8 PID")?
        .parse()?;
    install_parent_death_guard(parent)?;
    let metadata = fs::symlink_metadata(&work)?;
    ensure!(
        work.is_absolute()
            && socket.parent() == Some(work.as_path())
            && metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o777 == 0o700,
        "private owned fixture directory required"
    );
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut nonce = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    agentenv_dragonball_runner::seccomp::install_m0_filter()?;
    let vmm = DragonballVmm::new(&instance, &work)?;
    let mut service = RuntimeService::new(instance, nonce, vmm);
    let mut injected = false;
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut size,
                )
            } == 0,
            "read fixture peer credentials"
        );
        if peer.uid != unsafe { libc::geteuid() } || u32::try_from(peer.pid).ok() != Some(parent) {
            continue;
        }
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let request = match read_runtime_request(&mut stream) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let json = serde_json::to_value(&request)?;
        let op = json["command"]["op"].as_str().context("runtime op")?;
        let budget = json["budget_ms"].as_u64().context("request budget")?;
        let response = service.handle(request);
        if let Ok(config) = fs::read(work.join("reply-fault.json")) {
            let config: serde_json::Value = serde_json::from_slice(&config)?;
            let evidence = PathBuf::from(config["evidence"].as_str().context("evidence path")?);
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(evidence.join("ops"))?;
            writeln!(log, "{op}")?;
            if op == "export_held_state" && config["barrier"] == true {
                ensure!(
                    response.result.is_ok(),
                    "export failed before qualification barrier"
                );
                fs::write(evidence.join("reached"), b"handled")?;
                let deadline = Instant::now() + Duration::from_secs(30);
                while !evidence.join("release").exists() {
                    ensure!(Instant::now() < deadline, "fault barrier timed out");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let fault = config["drop_reply"].as_str().context("fault op")?;
            if !injected && op == fault.strip_prefix("delay_").unwrap_or(fault) {
                ensure!(
                    response.result.is_ok(),
                    "operation failed before reply injection"
                );
                injected = true;
                if fault.starts_with("delay_") {
                    std::thread::sleep(Duration::from_millis(budget + 250));
                }
                continue;
            }
        }
        let written = stream.write_all(&encode_runtime_response(&response)?);
        match service.phase() {
            RuntimePhase::Stopped => {
                written?;
                return Ok(());
            }
            RuntimePhase::Failed => anyhow::bail!("runtime requires teardown"),
            _ => {
                let _ = written;
            }
        }
    }
    Ok(())
}

fn main() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Err(error) = run() {
        eprintln!("reply-fault fixture: {error:#}");
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    panic!("reply-fault fixture requires Linux x86_64");
}
