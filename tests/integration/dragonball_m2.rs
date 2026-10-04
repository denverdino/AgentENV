use std::{path::PathBuf, time::Duration};

use agentenv::sandbox::{
    dragonball::{
        DragonballMemoryMode, DragonballRuntimePolicy, DragonballSandbox, DragonballSandboxFactory,
    },
    SandboxBackend, SandboxExecutor,
};
use anyhow::{Context, Result};

use super::dragonball_m1::{
    asset, fixture, fork_command, fork_spec, launch, posix_manager, publish_metadata,
    runtime_frame, spec, start_active_io, start_echo, ActiveTraffic,
};

// Take over only after node capture has left the source held. Exporting an
// independent full dump at that same token catches missing CPU/device/zero pages.
async fn compare_held_oracle(
    sandbox: &mut DragonballSandbox,
    memory_config: &std::path::Path,
    global_config: &std::path::Path,
) -> Result<()> {
    use agentenv_vmm_protocol::{runtime::*, *};
    use overlaybd::virtual_file::VirtualFile;
    use std::os::unix::fs::FileExt;
    use tokio::io::AsyncWriteExt;
    let proof = sandbox
        .runtime_status()
        .await?
        .capture
        .context("held capture proof")?;
    let work = sandbox.work_dir()?.to_path_buf();
    let full_path = work.join("oracle-memory.bin");
    let peer = tokio::net::UnixStream::connect(work.join("runner.sock")).await?;
    let pid = peer.peer_cred()?.pid().context("runner PID")?;
    drop(peer);
    let args = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let args = args.split(|byte| *byte == 0).collect::<Vec<_>>();
    let index = args
        .iter()
        .position(|arg| *arg == b"--instance-id")
        .context("sandbox instance flag")?;
    let instance = String::from_utf8(args[index + 1].to_vec())?;
    for (index, command) in [RuntimeCommand::ExportHeldSnapshot {
        token: proof.token.clone(),
        state_path: work.join("oracle-state.json"),
        memory_path: full_path.clone(),
    }]
    .into_iter()
    .enumerate()
    {
        let request = RuntimeRequest {
            mode: "runtime".into(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            request_id: 1_000_000 + index as u64,
            sandbox_instance_id: instance.clone(),
            budget_ms: 60_000,
            command,
        };
        let mut stream = tokio::net::UnixStream::connect(work.join("runner.sock")).await?;
        stream.write_all(&encode_runtime_request(&request)?).await?;
        let frame =
            tokio::time::timeout(Duration::from_secs(65), runtime_frame(&mut stream)).await??;
        let response = decode_runtime_response(&frame)?;
        validate_runtime_response(&request, &response)
            .with_context(|| format!("oracle request={request:?}, response={response:?}"))?;
        response
            .result
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    }
    let service = overlaybd::ImageService::from_config_path(global_config).await?;
    let image = service.create_image_file(memory_config).await?;
    let full = std::fs::File::open(full_path)?;
    let size = proof.memory_layout.image_size;
    assert_eq!(image.size().await?, size);
    assert_eq!(full.metadata()?.len(), size);
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(2 * 1024 * 1024) as usize;
        let bytes = image.read_at(offset, length).await?;
        let mut oracle = vec![0; length];
        full.read_exact_at(&mut oracle, offset)?;
        anyhow::ensure!(
            bytes.as_ref() == oracle,
            "incremental/full RAM mismatch at {offset}"
        );
        offset += length as u64;
    }
    // Node's lower RPC IDs are intentionally no longer usable; stop must still
    // reap before dropping the shared device and ancestral artifact owners.
    sandbox.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "same-held complete RAM oracle of a real incremental shared-memory guest"]
async fn dragonball_m2_incremental_matches_held_full_dump() -> Result<()> {
    incremental_case(1024, false, false).await
}

#[tokio::test]
#[ignore = "2GiB active CPU/AIO/TAP incremental full-RAM oracle"]
async fn dragonball_m2_incremental_full_oracle_2g() -> Result<()> {
    incremental_case(2048, false, false).await
}

#[tokio::test]
#[ignore = "8GiB across GPA hole; requires dedicated memory fixture capacity"]
async fn dragonball_m2_incremental_full_oracle_8g() -> Result<()> {
    incremental_case(8192, false, false).await
}

#[tokio::test]
#[ignore = "real publication rejection must not advance the restored memory base"]
async fn dragonball_m2_failed_publish_does_not_reset_dirty() -> Result<()> {
    incremental_case(512, true, false).await
}

#[tokio::test]
#[ignore = "real tmpfs zero/rewrite snapshots and subsequent private restore"]
async fn dragonball_m2_zero_then_rewrite_and_fork() -> Result<()> {
    incremental_case(512, false, true).await
}

async fn incremental_case(memory_mib: u32, failed_publish: bool, zero_rewrite: bool) -> Result<()> {
    let (app, assets) = fixture().await?;
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?
    .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental);
    let factory = DragonballSandboxFactory::new(policy)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let access = launch.envd_access_token.clone();
    let mut build = spec(&assets)?;
    build.resources.memory_mib = memory_mib;
    let mut cold = factory.build_fresh(build.clone(), launch)?;
    cold.start().await?;
    let marker = start_active_io(&cold, &app, access.as_ref()).await?;
    start_echo(&cold, &marker).await?;
    command(
        &cold,
        "head -c 1048576 /dev/zero | tr '\\000' A > /dev/shm/db-m2-pages",
    )
    .await?;
    let original = cold.pause(None).await?;
    let memory_path: PathBuf = original.encode()?["memory_image_config_path"]
        .as_str()
        .context("original memory config")?
        .into();
    let initial_reports = balloon_reports(&memory_path.with_file_name("vm-state.json"))?;
    cold.stop().await?;
    drop(cold);
    let mut restored = factory.build_paused(id, original.as_ref(), access.clone())?;
    restored.start().await?;
    memory_device(&restored).await?;
    let traffic = ActiveTraffic::start(&restored).await?;
    command(&restored, "nohup /bin/sh -c 'while dd if=/var/tmp/db-m1-cut-block of=/dev/null bs=4096 count=1 iflag=direct status=none; do :; done' >/tmp/db-m2-direct-read.log 2>&1 </dev/null &").await?;
    command(
        &restored,
        "printf first | dd of=/dev/shm/db-m2-pages conv=notrunc status=none",
    )
    .await?;
    // Linux schedules page reporting two seconds after freeing large ranges.
    // Free new guest RAM after lazy restore so the private reclaimer, not just
    // the cold-start balloon, must run. Queue progress below is the proof.
    command(&restored, "dd if=/dev/zero of=/dev/shm/db-m2-report bs=1M count=64 status=none && rm /dev/shm/db-m2-report && sleep 3").await?;
    let first = restored.snapshot().await?;
    traffic.wait_after(traffic.count()).await?;
    let reports = balloon_reports(&first.manifest().vm_state.path)?;
    assert_ne!(
        reports, initial_reports,
        "no guest page reporting after lazy restore"
    );
    eprintln!(
        "{memory_mib} MiB guest balloon reporting used index: {initial_reports} -> {reports}"
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first.manifest().memory.image_config_path)?)?;
    assert!(
        config["lowers"].as_array().context("memory lowers")?.len() >= 2,
        "capture discarded the immutable base and emitted full RAM"
    );
    let publish_root = tempfile::tempdir_in(&app.runtime_path)?;
    let manager = posix_manager(publish_root.path())?;
    if failed_publish {
        let alias = agentenv::snapshot::SnapshotAlias::parse("db-m2-rejected-delta")?;
        let mut metadata = publish_metadata(id, &build, &assets, &first)?;
        metadata.alias = Some(alias.clone());
        manager.publish_captured(metadata, first).await?;
        command(
            &restored,
            "printf second | dd of=/dev/shm/db-m2-pages bs=1 seek=4096 conv=notrunc status=none",
        )
        .await?;
        let rejected = restored.snapshot().await?;
        let mut metadata = publish_metadata(id, &build, &assets, &rejected)?;
        metadata.alias = Some(alias);
        let error = manager
            .publish_captured(metadata, rejected)
            .await
            .expect_err("alias conflict");
        assert!(
            matches!(
                error,
                agentenv::snapshot::RepositoryError::AliasConflict { .. }
            ),
            "{error:#}"
        );
    } else {
        drop(first);
    }
    if zero_rewrite {
        command(
            &restored,
            "dd if=/dev/zero of=/dev/shm/db-m2-pages bs=4096 count=128 conv=notrunc status=none",
        )
        .await?;
        drop(restored.snapshot().await?);
        command(
            &restored,
            "printf rewrite | dd of=/dev/shm/db-m2-pages bs=1 seek=8192 conv=notrunc status=none",
        )
        .await?;
    }
    let final_state = restored.pause(None).await?;
    traffic.stop().await?;
    let encoded = final_state.encode()?;
    let memory: PathBuf = encoded["memory_image_config_path"]
        .as_str()
        .context("paused memory image")?
        .into();
    compare_held_oracle(
        &mut restored,
        &memory,
        &app.ublk.overlaybd.global_config_path,
    )
    .await?;
    drop(restored);
    let mut later = factory.build_paused(id, final_state.as_ref(), access)?;
    later.start().await?;
    if zero_rewrite {
        assert_eq!(
            command(
                &later,
                "dd if=/dev/shm/db-m2-pages bs=1 skip=8192 count=7 status=none"
            )
            .await?,
            "rewrite"
        );
        command(
            &later,
            "dd if=/dev/shm/db-m2-pages bs=4096 count=1 status=none | cmp -n 4096 - /dev/zero",
        )
        .await?;
        let child_spec = fork_spec()?;
        let child_access = child_spec.envd_access_token.clone();
        let mut children = later.fork(&[child_spec]).await?;
        let mut child = children.pop().context("fork child")??;
        let inherited = fork_command(
            child.as_ref(),
            child_access.as_ref(),
            "dd if=/dev/shm/db-m2-pages bs=4096 count=1 status=none | cmp -n 4096 - /dev/zero && dd if=/dev/shm/db-m2-pages bs=1 skip=8192 count=7 status=none",
        )
        .await?;
        assert_eq!(inherited.exit_code, 0);
        assert_eq!(inherited.stdout, "rewrite");
        let changed = fork_command(
            child.as_ref(),
            child_access.as_ref(),
            "printf forked! | dd of=/dev/shm/db-m2-pages bs=1 seek=8192 conv=notrunc status=none",
        )
        .await?;
        assert_eq!(changed.exit_code, 0);
        assert_eq!(
            command(
                &later,
                "dd if=/dev/shm/db-m2-pages bs=1 skip=8192 count=7 status=none"
            )
            .await?,
            "rewrite"
        );
        child.stop().await?;
    } else {
        assert_eq!(
            command(&later, "head -c 5 /dev/shm/db-m2-pages").await?,
            "first"
        );
        if failed_publish {
            assert_eq!(
                command(
                    &later,
                    "dd if=/dev/shm/db-m2-pages bs=1 skip=4096 count=6 status=none"
                )
                .await?,
                "second"
            );
        }
    }
    later.stop().await?;
    Ok(())
}

fn balloon_reports(state: &std::path::Path) -> Result<u64> {
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(state)?)?;
    let balloon =
        &state["upstream_state"]["device_states"]["balloon"]["devices"][0]["transport"]["Mmio"];
    anyhow::ensure!(
        balloon["device_activated"] == true,
        "guest did not activate balloon"
    );
    balloon["queues"][2]["queue"]["next_used"]
        .as_u64()
        .context("balloon reporting used index")
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

async fn command(sandbox: &DragonballSandbox, script: &str) -> Result<String> {
    let output = sandbox.run_command("/bin/sh", &["-ec", script]).await?;
    anyhow::ensure!(
        output.exit_code == 0,
        "guest command failed: {}",
        output.stderr
    );
    Ok(output.stdout)
}

#[tokio::test]
#[ignore = "diagnostic control: identical idle guest restored from a regular RAM file"]
async fn dragonball_m2_idle_file_restore_control() -> Result<()> {
    let (app, assets) = fixture().await?;
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let launch = launch(&assets)?;
    let (id, access) = (launch.sandbox_id, launch.envd_access_token.clone());
    let mut cold = factory.build_fresh(spec(&assets)?, launch)?;
    cold.start().await?;
    command(&cold, "printf ancestor > /dev/shm/db-m2-cow").await?;
    let paused = cold.pause(None).await?;
    cold.stop().await?;
    drop(cold);
    let mut restored = factory.build_paused(id, paused.as_ref(), access)?;
    restored.start().await?;
    assert_eq!(
        command(&restored, "cat /dev/shm/db-m2-cow").await?,
        "ancestor"
    );
    restored.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real shared ublk COW guest; isolated non-root KVM fixture"]
async fn dragonball_m2_shared_memory_cow_three_children() -> Result<()> {
    let (app, assets) = fixture().await?;
    if std::env::var_os("AENV_TEST_DB_MEMORY_POOL").is_some() {
        assert!(
            app.pool.block.enabled,
            "pool-enabled regression lost its configuration"
        );
    }
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?
    .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental);
    let factory = DragonballSandboxFactory::new(policy)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let access = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    command(&source, "printf ancestor > /dev/shm/db-m2-cow").await?;
    let paused = source.pause(None).await?;
    source.stop().await?;
    drop(source);
    let ancestral_memory = paused.encode()?["memory_image_config_path"].clone();
    let mut children = Vec::new();
    let mut device = None;
    for index in 0..3 {
        let mut child = factory.build_paused(id, paused.as_ref(), access.clone())?;
        let artifacts = serde_json::to_value(child.startup_artifacts())?;
        assert!(
            artifacts["overlaybd_image_config_paths"]
                .as_array()
                .unwrap()
                .contains(&ancestral_memory),
            "runtime liveness omitted lazy memory backing before startup"
        );
        child.start_nowait().await?;
        let current = memory_device(&child).await?;
        if let Some(expected) = &device {
            assert_eq!(
                &current, expected,
                "children must share the same ancestor device"
            );
        } else {
            device = Some(current);
        }
        child.wait_for_ready().await?;
        assert_eq!(command(&child, "cat /dev/shm/db-m2-cow").await?, "ancestor");
        command(
            &child,
            &format!("printf child-{index} | dd of=/dev/shm/db-m2-cow conv=notrunc status=none"),
        )
        .await?;
        children.push(child);
    }
    for (index, child) in children.iter().enumerate() {
        assert_eq!(
            command(child, "cat /dev/shm/db-m2-cow").await?,
            format!("child-{index}r")
        );
    }
    let mut later = factory.build_paused(id, paused.as_ref(), access)?;
    later.start().await?;
    assert_eq!(memory_device(&later).await?, *device.as_ref().unwrap());
    assert_eq!(command(&later, "cat /dev/shm/db-m2-cow").await?, "ancestor");
    for mut child in children {
        child.stop().await?;
    }
    let device = device.unwrap();
    assert!(
        device.exists(),
        "last live runner lost its shared memory device"
    );
    later.stop().await?;
    drop(later);
    drop(paused);
    tokio::time::timeout(Duration::from_secs(15), async {
        while device.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("last memory lease did not delete ublk device")?;
    Ok(())
}

#[tokio::test]
#[ignore = "shared lazy RAM must retire, not recycle, even with the block warm pool enabled"]
async fn dragonball_m2_shared_memory_retires_with_pool_enabled() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("run"))?;
    let config_path = std::env::var_os("AENV_CONFIG_PATH").context("explicit test config")?;
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(config_path)?)?;
    config["pool"]["block"]["enabled"] = toml::Value::Boolean(true);
    // Leave enough idle capacity that the last RAM generation really would be
    // cached; overflow deletion must not make the negative control pass.
    config["pool"]["high_watermark"] = toml::Value::Integer(64);
    let config_path = root.path().join("pool-enabled.toml");
    std::fs::write(&config_path, toml::to_string(&config)?)?;
    let status = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "dragonball_m2::dragonball_m2_shared_memory_cow_three_children",
                "--ignored",
                "--nocapture",
            ])
            .env("AENV_CONFIG_PATH", config_path)
            .env("AENV_RUNTIME_PATH", root.path().join("run"))
            .env("AENV_TEST_DB_MEMORY_POOL", "1")
            .kill_on_drop(true)
            .status(),
    )
    .await??;
    anyhow::ensure!(
        status.success(),
        "pool-enabled memory child failed: {status}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "owned shared-memory runner SIGBUS/SIGKILL; isolated non-root KVM fixture"]
async fn dragonball_m2_shared_memory_runner_death_retains_backing_until_reap() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(
        DragonballRuntimePolicy::from_app_config(
            &app,
            asset(&assets, "runner_path")?,
            asset(&assets, "kernel_path")?,
        )?
        .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
    )?;
    for signal in [libc::SIGKILL, libc::SIGBUS] {
        let launch = launch(&assets)?;
        let id = launch.sandbox_id;
        let access = launch.envd_access_token.clone();
        let mut cold = factory.build_fresh(spec(&assets)?, launch)?;
        cold.start().await?;
        let paused = cold.pause(None).await?;
        cold.stop().await?;
        drop(cold);
        let memory: PathBuf = paused.encode()?["memory_image_config_path"]
            .as_str()
            .context("memory backing")?
            .into();
        let mut restored = factory.build_paused(id, paused.as_ref(), access)?;
        restored.start().await?;
        drop(paused);
        let device = memory_device(&restored).await?;
        let work = restored.work_dir()?.to_path_buf();
        let peer = tokio::net::UnixStream::connect(work.join("runner.sock")).await?;
        let pid = peer.peer_cred()?.pid().context("owned runner PID")?;
        drop(peer);
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        if signal == libc::SIGBUS {
            // Rust's stack-overflow handler resets SIGBUS to default and returns.
            // A synchronous fault reexecutes the instruction; a synthetic kill
            // needs a second delivery after that reset to model the fatal signal.
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
                    let caught = status
                        .lines()
                        .find_map(|line| line.strip_prefix("SigCgt:"))
                        .context("caught signals")?;
                    if u64::from_str_radix(caught.trim(), 16)? & (1 << (libc::SIGBUS - 1)) == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while restored.executor().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let process = PathBuf::from(format!("/proc/{pid}"));
        assert!(process.exists(), "runner reaped outside its owner");
        assert!(
            device.exists() && memory.exists() && work.exists(),
            "shared memory or runtime lease released before reap"
        );
        let error = restored.snapshot().await.expect_err("dead runner capture");
        assert!(
            error.is_terminal(),
            "dead runner must not recover as Running"
        );
        assert!(!process.exists(), "terminal capture did not reap runner");
        drop(restored);
        tokio::time::timeout(Duration::from_secs(15), async {
            while device.exists() || memory.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("shared memory lease leaked after reap")?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "a kernel block opener outlives runner reap; backing must survive synchronous device retirement"]
async fn dragonball_m2_memory_backing_survives_kernel_reference() -> Result<()> {
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
    // A real extra block opener pins the kernel generation after the process
    // exits, as does an APF-held mm. This test does not claim to inject an APF.
    let kernel_reference = std::fs::File::open(&device)?;
    let peer = tokio::net::UnixStream::connect(restored.work_dir()?.join("runner.sock")).await?;
    let pid = peer.peer_cred()?.pid().context("owned runner PID")?;
    drop(peer);
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    restored.stop().await?;
    drop(restored);
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "runner not reaped"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while device.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("device was recycled instead of retired")?;
    assert!(
        memory.exists(),
        "backing dropped before final kernel reference drained"
    );
    drop(kernel_reference);
    tokio::time::timeout(Duration::from_secs(10), async {
        while memory.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("backing did not release after retirement completed")?;
    Ok(())
}

#[tokio::test]
#[ignore = "truncate only this fixture's new RAM layer to inject real ublk I/O failure"]
async fn dragonball_m2_shared_memory_io_error_is_contained() -> Result<()> {
    use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
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
    let mut cold = factory.build_fresh(spec(&assets)?, launch)?;
    cold.start().await?;
    let paused = cold.pause(None).await?;
    cold.stop().await?;
    drop(cold);
    let mut restored = factory.build_paused(id, paused.as_ref(), access.clone())?;
    restored.start().await?;
    // Resolve from the owned runner's private RAM mappings, never a global ID.
    let device = memory_device(&restored).await?;
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(&device)?;
    let encoded = paused.encode()?;
    let memory = std::fs::canonicalize(
        encoded["memory_image_config_path"]
            .as_str()
            .context("memory config")?,
    )?;
    let layer = memory.with_file_name("overlaybd.commit");
    assert!(layer.starts_with(std::fs::canonicalize(&app.home_path)?));
    assert!(
        layer.is_file(),
        "fault must target only the new fixture-owned memory layer"
    );
    let error = tokio::task::spawn_blocking(move || {
        #[repr(align(4096))]
        struct Page([u8; 4096]);
        let mut page = Page([0; 4096]);
        assert_eq!(backing.read_at(&mut page.0, 16 << 20).unwrap(), 4096);
        assert!(
            page.0.iter().any(|byte| *byte != 0),
            "fault probe must read materialized RAM, not a sparse zero extent"
        );
        std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&layer)
            .unwrap()
            .set_len(0)
            .unwrap();
        backing
            .read_at(&mut page.0, 16 << 20)
            .expect_err("truncated RAM layer must fail direct reads")
    })
    .await?;
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    // Lazy restore can legitimately continue from cached pages. Exercise an
    // actual uncached RAM read via the held full oracle, not a guessed readiness
    // failure (which would require eager prefaulting every page).
    match restored.pause(None).await {
        Ok(held) => {
            let encoded = held.encode()?;
            let memory = std::path::Path::new(
                encoded["memory_image_config_path"]
                    .as_str()
                    .context("captured memory")?,
            );
            let error = compare_held_oracle(
                &mut restored,
                memory,
                &app.ublk.overlaybd.global_config_path,
            )
            .await
            .expect_err("held memory oracle silently accepted an unreadable RAM backing");
            eprintln!("injected RAM EIO surfaced through held oracle: {error:#}");
        }
        Err(error) if error.is_terminal() => {}
        Err(error) => {
            assert_eq!(
                restored.runtime_status().await?.phase,
                agentenv_vmm_protocol::runtime::RuntimePhase::Running,
                "recoverable read failure did not restore Running: {error:#}"
            );
        }
    }
    restored.stop().await?;
    drop(restored);
    drop(paused);
    tokio::time::timeout(Duration::from_secs(15), async {
        while device.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("failed memory device not released")?;
    Ok(())
}
