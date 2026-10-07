//! Shared Dragonball integration fixtures; suites depend on this module, not each other.
use agentenv::{
    cfg::AppConfig,
    sandbox::{
        dragonball::DragonballSandbox, BaseSandboxNetworkPolicy, FreshSandboxBuildSpec,
        SandboxAccessTokenGenerator, SandboxBackend, SandboxExecutor, SandboxForkSpec,
        SandboxLaunchConfig, SandboxNetworkEgressPolicy, SandboxNetworkPolicy,
    },
    snapshot::CommandContext,
    types::{SandboxId, SandboxResources},
    virtualization::VirtualizationMode,
};
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::{collections::HashMap, path::PathBuf, time::Duration};

pub(super) fn posix_manager(root: &std::path::Path) -> Result<agentenv::snapshot::SnapshotManager> {
    use agentenv::snapshot::repository::backends::{PosixFsBackend, PosixFsBackendConfig};
    let (repository, resolver) = PosixFsBackend::new(PosixFsBackendConfig {
        root: root.join("repository"),
        cache_root: Some(root.join("cache")),
        runtime_cache_root: Some(root.join("runtime-cache")),
    })?
    .into_parts();
    Ok(agentenv::snapshot::SnapshotManager::from_parts(
        repository, resolver, None,
    ))
}

pub(super) fn publish_metadata(
    source: SandboxId,
    spec: &FreshSandboxBuildSpec,
    assets: &Value,
    captured: &agentenv::sandbox::CapturedSandboxSnapshot,
) -> Result<agentenv::snapshot::SnapshotPublishMetadata> {
    use agentenv::snapshot::{
        SnapshotId, SnapshotPublishMetadata, SnapshotPublishSource, SnapshotRuntimeVersions,
    };
    Ok(SnapshotPublishMetadata {
        id: SnapshotId::generate(),
        alias: None,
        source: SnapshotPublishSource::Sandbox {
            source_sandbox_id: source.to_string(),
        },
        context: spec.context.clone(),
        startup: None,
        resources: spec.resources,
        runtime_versions: SnapshotRuntimeVersions::from_dragonball(
            captured
                .manifest()
                .restore_identity
                .clone()
                .context("capture identity")?,
        )?,
        virtualization_mode: VirtualizationMode::Kvm,
        image_configs: serde_json::from_value(assets["image_configs"].clone())?,
        volume_snapshots: vec![],
        custom_extension_params: None,
    })
}

pub(super) async fn fork_command(
    sandbox: &dyn SandboxBackend,
    token: Option<&agentenv::sandbox::EnvdAccessToken>,
    script: &str,
) -> Result<agentenv::sandbox::ProcessOutput> {
    let address = format!(
        "http://{}:49983",
        sandbox.host_interaction_ip().context("child IP")?
    );
    guest_command(&address, token, script).await
}

pub(super) async fn guest_command(
    address: &str,
    token: Option<&agentenv::sandbox::EnvdAccessToken>,
    script: &str,
) -> Result<agentenv::sandbox::ProcessOutput> {
    use envd::process::{process_event, ProcessClient, ProcessConfig, StartRequest};
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut client = ProcessClient::connect(address, token.map(|token| token.expose())).await?;
        let mut stream = client
            .start(StartRequest {
                process: Some(ProcessConfig {
                    cmd: "/bin/sh".into(),
                    args: vec!["-ec".into(), script.into()],
                    envs: HashMap::new(),
                    cwd: None,
                }),
                pty: None,
                tag: None,
                stdin: Some(false),
            })
            .await?
            .into_inner();
        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some(response) = stream.message().await? {
            match response.event.and_then(|event| event.event) {
                Some(process_event::Event::Data(data)) => match data.output {
                    Some(process_event::data_event::Output::Stdout(bytes)) => {
                        stdout.push_str(std::str::from_utf8(&bytes)?)
                    }
                    Some(process_event::data_event::Output::Stderr(bytes)) => {
                        stderr.push_str(std::str::from_utf8(&bytes)?)
                    }
                    _ => {}
                },
                Some(process_event::Event::End(end)) => {
                    return Ok(agentenv::sandbox::ProcessOutput {
                        stdout,
                        stderr,
                        exit_code: end.exit_code,
                    })
                }
                _ => {}
            }
        }
        anyhow::bail!("guest process stream ended without exit")
    })
    .await?
}

pub(super) fn fork_spec() -> Result<SandboxForkSpec> {
    let sandbox_id = SandboxId::new();
    Ok(SandboxForkSpec {
        sandbox_id,
        envd_access_token: Some(
            SandboxAccessTokenGenerator::new("dragonball-m1-child")?.generate(sandbox_id),
        ),
        extra_drives: vec![],
        replace_drive_ids: vec![],
    })
}

pub(super) async fn start_active_io(
    sandbox: &agentenv::sandbox::dragonball::DragonballSandbox,
    app: &AppConfig,
    token: Option<&agentenv::sandbox::EnvdAccessToken>,
) -> Result<String> {
    let build = tempfile::tempdir()?;
    let source = build.path().join("active-io.c");
    let binary = build.path().join("active-io");
    std::fs::write(
        &source,
        include_bytes!("../fixtures/dragonball-active-io.c"),
    )?;
    let compiled = tokio::process::Command::new("cc")
        .args(["-static", "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .await?;
    anyhow::ensure!(
        compiled.status.success(),
        "compile workload: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let address = format!(
        "http://{}:{}/files",
        sandbox.host_interaction_ip().context("guest IP")?,
        app.tools.control_plane_port
    );
    let body = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(std::fs::read(binary)?).file_name("active-io"),
    );
    let mut upload = reqwest::Client::new()
        .post(address)
        .query(&[("path", "/tmp/db-m1-active-io")])
        .multipart(body);
    if let Some(token) = token {
        upload = upload.header("X-Access-Token", token.expose());
    }
    let response = upload.send().await?;
    let status = response.status();
    let mut detail = response.text().await?;
    if let Some(token) = token {
        detail = detail.replace(token.expose(), "<redacted>");
    }
    anyhow::ensure!(
        status.is_success(),
        "workload upload refused {status}: {detail}"
    );
    let marker = format!("DBM1-RAM-CUT:::{}", uuid::Uuid::new_v4().simple());
    assert_eq!(marker.len(), 47);
    let command = format!("chmod +x /tmp/db-m1-active-io; nohup /tmp/db-m1-active-io write '{marker}' >/tmp/db-m1-active-io.log 2>&1 </dev/null &");
    let output = sandbox.run_command("/bin/sh", &["-ec", &command]).await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    for _ in 0..100 {
        let output = sandbox
            .run_command("/tmp/db-m1-active-io", &["read", &marker])
            .await?;
        if output.exit_code == 0
            && output
                .stdout
                .split_whitespace()
                .last()
                .is_some_and(|value| value.parse::<u64>().is_ok_and(|n| n > 10))
        {
            return Ok(marker);
        }
    }
    anyhow::bail!("active writer did not establish real fdatasync progress")
}

pub(super) async fn runtime_frame(stream: &mut tokio::net::UnixStream) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    let size = u32::from_be_bytes(header) as usize;
    anyhow::ensure!(size <= 1024 * 1024, "test proxy frame too large");
    let mut frame = vec![0; size + 4];
    frame[..4].copy_from_slice(&header);
    stream.read_exact(&mut frame[4..]).await?;
    Ok(frame)
}

pub(super) struct ActiveTraffic {
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for ActiveTraffic {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl ActiveTraffic {
    pub(super) async fn start(sandbox: &dyn SandboxBackend) -> Result<Self> {
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        };
        let address = (
            sandbox.host_interaction_ip().context("traffic guest IP")?,
            50001,
        );
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(address).await?;
        let progress = Arc::new(AtomicU64::new(0));
        let observed = progress.clone();
        let task = tokio::spawn(async move {
            let mut response = [0u8; 4096];
            for sequence in 1u64.. {
                let mut packet = [0u8; 4096];
                for (index, word) in packet.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                    word.copy_from_slice(
                        &(sequence ^ (index as u64).wrapping_mul(0x9e3779b97f4a7c15)).to_le_bytes(),
                    );
                }
                tokio::time::timeout(Duration::from_secs(60), async {
                    loop {
                        anyhow::ensure!(
                            socket.send(&packet).await? == packet.len(),
                            "short UDP send"
                        );
                        match tokio::time::timeout(
                            Duration::from_secs(1),
                            socket.recv(&mut response),
                        )
                        .await
                        {
                            Err(_) => continue, // UDP may drop a datagram; capture must still resume progress.
                            Ok(length) => {
                                anyhow::ensure!(length? == response.len(), "short UDP echo")
                            }
                        }
                        let echoed = u64::from_le_bytes(response[..8].try_into()?);
                        for (index, word) in response.as_chunks::<8>().0.iter().enumerate() {
                            anyhow::ensure!(
                                u64::from_le_bytes(*word)
                                    == echoed ^ (index as u64).wrapping_mul(0x9e3779b97f4a7c15),
                                "corrupt echoed word {index}"
                            );
                        }
                        anyhow::ensure!(echoed <= sequence, "echo from a future request");
                        if echoed == sequence {
                            break;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("network queue did not resume")??;
                observed.fetch_add(1, Ordering::Release);
            }
            Ok(())
        });
        let traffic = Self {
            task: Some(task),
            progress,
        };
        traffic.wait_after(10).await?;
        Ok(traffic)
    }

    pub(super) fn count(&self) -> u64 {
        self.progress.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) async fn wait_after(&self, before: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.count() <= before {
                anyhow::ensure!(
                    !self.task.as_ref().unwrap().is_finished(),
                    "traffic worker terminated"
                );
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("no verified RX/TX progress")??;
        Ok(())
    }

    pub(super) async fn stop(mut self) -> Result<()> {
        let task = self.task.take().unwrap();
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error.into()),
            Ok(result) => result,
        }
    }
}

pub(super) async fn start_echo(
    source: &agentenv::sandbox::dragonball::DragonballSandbox,
    marker: &str,
) -> Result<()> {
    let command = format!(
        "nohup /tmp/db-m1-active-io echo '{marker}' >/tmp/db-m1-echo.log 2>&1 </dev/null &"
    );
    let output = source.run_command("/bin/sh", &["-ec", &command]).await?;
    anyhow::ensure!(output.exit_code == 0, "echo startup: {}", output.stderr);
    for _ in 0..100 {
        if source
            .run_command("/bin/sh", &["-ec", "test -f /dev/shm/db-m1-echo-ready"])
            .await?
            .exit_code
            == 0
        {
            return Ok(());
        }
    }
    anyhow::bail!("UDP workload did not bind its guest socket")
}

pub(super) async fn fixture() -> Result<(AppConfig, Value)> {
    anyhow::ensure!(
        std::env::var("AENV_DRAGONBALL_M0_ISOLATED").as_deref() == Ok("1"),
        "use the isolated capability runner"
    );
    anyhow::ensure!(
        std::fs::read_link("/proc/thread-self/ns/net")?.to_string_lossy()
            != std::env::var("AENV_DRAGONBALL_M0_PARENT_NETNS")?,
        "test namespace not isolated"
    );
    anyhow::ensure!(
        unsafe { libc::geteuid() } != 0,
        "non-root build/test required"
    );
    let app = super::common::setup_runtime_only().await.clone();
    let assets = serde_json::from_slice(&std::fs::read(
        std::env::var_os("AENV_DRAGONBALL_M0_ASSETS").context("prepared guest assets")?,
    )?)?;
    Ok((app, assets))
}

pub(super) fn asset(assets: &Value, key: &str) -> Result<PathBuf> {
    Ok(assets[key]
        .as_str()
        .with_context(|| format!("missing {key}"))?
        .into())
}

pub(super) fn spec(assets: &Value) -> Result<FreshSandboxBuildSpec> {
    Ok(FreshSandboxBuildSpec {
        image_config_path: asset(assets, "image_path")?,
        resources: SandboxResources {
            cpu_count: 1,
            memory_mib: 512,
            disk_size_mib: 0,
        },
        context: CommandContext {
            env_vars: HashMap::from([("DB_M1_ENV".into(), "guest-contract".into())]),
            workdir: "/tmp".into(),
            user: Some("1000:1000".into()),
            exposed_ports: vec![],
            entrypoint: None,
            cmd: None,
            volumes: vec![],
            labels: HashMap::new(),
        },
        extra_drives: vec![],
        extra_boot_args: None,
    })
}

pub(super) fn launch(assets: &Value) -> Result<SandboxLaunchConfig> {
    let sandbox_id = SandboxId::new();
    let mut extra = Map::new();
    extra.insert("imageConfigs".into(), assets["image_configs"].clone());
    Ok(SandboxLaunchConfig {
        sandbox_id,
        snapshot_id: "dragonball-m1-existing-guest".into(),
        network: Some(SandboxNetworkPolicy::new(
            true,
            BaseSandboxNetworkPolicy::Deny,
            SandboxNetworkEgressPolicy::default(),
        )),
        extra_mmds: extra,
        envd_access_token: Some(
            SandboxAccessTokenGenerator::new("dragonball-m1-qualification")?.generate(sandbox_id),
        ),
        ..Default::default()
    })
}

pub(super) async fn memory_device(sandbox: &DragonballSandbox) -> Result<PathBuf> {
    let socket = tokio::net::UnixStream::connect(sandbox.work_dir()?.join("runner.sock")).await?;
    let pid = socket.peer_cred()?.pid().context("owned runner peer PID")?;
    drop(socket);
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let devices = maps
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() >= 6 && fields[1] == "rw-p" && fields[5].starts_with("/dev/ublkb"))
                .then(|| PathBuf::from(fields[5]))
        })
        .collect::<std::collections::HashSet<_>>();
    anyhow::ensure!(
        devices.len() == 1,
        "expected one private ublk RAM mapping, got {devices:?}"
    );
    anyhow::ensure!(
        !sandbox.work_dir()?.join("restore-memory.bin").exists(),
        "restore copied full RAM"
    );
    Ok(devices.into_iter().next().unwrap())
}
