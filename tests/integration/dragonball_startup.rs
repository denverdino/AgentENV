use std::time::Duration;

use agentenv::sandbox::{
    dragonball::{DragonballMemoryMode, DragonballRuntimePolicy, DragonballSandboxFactory},
    SandboxBackend, SandboxBackendFactory, SandboxExecutor,
};
use anyhow::{Context, Result};

use super::dragonball_support::{asset, fixture, launch, posix_manager, publish_metadata, spec};

// Observe only this isolated runtime directory, never unrelated host VMs.
async fn mapped_memory_device(runtime: &std::path::Path) -> Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(runtime)? {
        let dir = entry?.path();
        if !dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("db-runtime-"))
        {
            continue;
        }
        let Ok(socket) = tokio::net::UnixStream::connect(dir.join("runner.sock")).await else {
            continue;
        };
        let pid = socket.peer_cred()?.pid().context("runner PID")?;
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
        for line in maps.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() >= 6 && fields[1] == "rw-p" && fields[5].starts_with("/dev/ublkb") {
                return Ok(fields[5].into());
            }
        }
    }
    anyhow::bail!("no lazy memory mapping in isolated runtime")
}

fn page_resident(path: &std::path::Path, offset: u64) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path)?;
    unsafe {
        let address = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            offset as libc::off_t,
        );
        anyhow::ensure!(
            address != libc::MAP_FAILED,
            "map cache observer: {}",
            std::io::Error::last_os_error()
        );
        let mut resident = 0;
        let result = libc::mincore(address, 4096, &mut resident);
        libc::munmap(address, 4096);
        anyhow::ensure!(result == 0, "mincore cache observer");
        Ok(resident & 1 != 0)
    }
}

async fn daemon_call(
    app: &agentenv::cfg::AppConfig,
    request: uvm_ublk_daemon::protocol::DaemonRequest,
) -> Result<uvm_ublk_daemon::protocol::DaemonResponse> {
    use uvm_ublk_daemon::protocol::{recv_message, send_message};
    let mut socket =
        tokio::net::UnixStream::connect(app.runtime_path.join("ublk-daemon.sock")).await?;
    send_message(&mut socket, &request).await?;
    recv_message(&mut socket)
        .await?
        .context("daemon closed test RPC")
}

#[tokio::test]
#[ignore = "real held restore checks valid/corrupt/missing/budget startup manifests"]
async fn dragonball_memory_prefetch_failure_keeps_snapshot_runnable() -> Result<()> {
    use sha2::{Digest, Sha256};
    let (mut app, assets) = fixture().await?;
    app.snapshot.memory_startup_pack.enabled = false;
    let factory_for = |app: &agentenv::cfg::AppConfig| -> Result<DragonballSandboxFactory> {
        DragonballSandboxFactory::new(
            DragonballRuntimePolicy::from_app_config(
                app,
                asset(&assets, "runner_path")?,
                asset(&assets, "kernel_path")?,
            )?
            .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
        )
    };
    let factory = factory_for(&app)?;
    let build = spec(&assets)?;
    let source = launch(&assets)?;
    let id = source.sandbox_id;
    let mut vm = factory.build_fresh(build.clone(), source)?;
    vm.start().await?;
    let captured = vm.snapshot().await?;
    let metadata = publish_metadata(id, &build, &assets, &captured)?;
    let root = tempfile::tempdir_in(&app.runtime_path)?;
    let manager = posix_manager(root.path())?;
    let mut record = manager.publish_captured(metadata, captured).await?;
    vm.stop().await?;
    assert!(record.committed.as_ref().unwrap().memory_startup.is_none());
    let size = u64::from(build.resources.memory_mib) << 20;
    let tail = size - 4096;
    let bytes = overlaybd::startup_manifest::encode_manifest(
        &overlaybd::startup_manifest::StartupManifest {
            mem_virtual_size: size,
            prefix_pages: vec![tail],
            ranges: vec![],
        },
    )?;
    let pack_path = root
        .path()
        .join("repository/snapshots")
        .join(record.id.to_string())
        .join("memory-startup.pack");
    record.committed.as_mut().unwrap().memory_startup =
        Some(agentenv::snapshot::MemoryStartupPackInfo {
            pack_size: bytes.len() as u64,
            mem_virtual_size: size,
            index_sha256: format!("{:x}", Sha256::digest(&bytes)),
        });
    // A tail page has no replayed queue state. LoadHeld does not execute guest
    // instructions, so residency proves prefetch, not normal guest demand.
    for (case, consume, budget, expected) in [
        ("valid", false, 4096, false),
        ("valid", true, 4096, true),
        ("budget", true, 0, false),
        ("corrupt", true, 4096, false),
        ("missing", true, 4096, false),
    ] {
        std::fs::write(&pack_path, &bytes)?;
        if case == "corrupt" {
            std::fs::write(&pack_path, vec![0; bytes.len()])?;
        }
        if case == "missing" {
            std::fs::remove_file(&pack_path)?;
        }
        app.snapshot.memory_startup_pack.consume_enabled = consume;
        app.snapshot.memory_startup_pack.max_pack_bytes = budget;
        let runnable = manager.resolve_runnable(record.clone()).await?;
        let mut restored = factory_for(&app)?.build_from_snapshot(&runnable, launch(&assets)?)?;
        restored.start_nowait().await?;
        let device = mapped_memory_device(&app.runtime_path).await?;
        let until = tokio::time::Instant::now() + Duration::from_secs(2);
        while expected && !page_resident(&device, tail)? && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !expected {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let resident = page_resident(&device, tail)?;
        let ready = restored.wait_for_ready().await;
        let stopped = restored.stop().await;
        ready?;
        stopped?;
        // Wait for async shared-device deletion before the next restore: no
        // cached positive from an earlier subscriber may mask the controls.
        tokio::time::timeout(Duration::from_secs(5), async {
            while device.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("memory device did not drain")?;
        assert_eq!(
            resident, expected,
            "{case}, consume={consume}, budget={budget}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "cancel real POSIX publication while the dedicated recorder owns the capture"]
async fn dragonball_memory_recording_outlives_publication_cancellation() -> Result<()> {
    cancellation_case(false).await
}

#[tokio::test]
#[ignore = "isolated child process exercises permanent node shutdown admission and drain"]
async fn dragonball_memory_shutdown_drains_recording() -> Result<()> {
    if std::env::var("AENV_TEST_DB_SHUTDOWN_CHILD").as_deref() != Ok("1") {
        let runtime = tempfile::tempdir()?;
        let status = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "dragonball_startup::dragonball_memory_shutdown_drains_recording",
                    "--ignored",
                    "--nocapture",
                ])
                .env("AENV_TEST_DB_SHUTDOWN_CHILD", "1")
                .env("AENV_RUNTIME_PATH", runtime.path())
                .kill_on_drop(true)
                .status(),
        )
        .await??;
        anyhow::ensure!(status.success(), "shutdown child failed: {status}");
        return Ok(());
    }
    cancellation_case(true).await
}

async fn cancellation_case(shutdown: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    let (mut app, assets) = fixture().await?;
    app.snapshot.memory_startup_pack.enabled = true;
    app.snapshot.memory_startup_pack.record_min_window_ms = 3000;
    app.snapshot.memory_startup_pack.record_max_window_ms = 4000;
    app.snapshot.memory_startup_pack.record_quiet_ms = 3000;
    let factory = DragonballSandboxFactory::new(
        DragonballRuntimePolicy::from_app_config(
            &app,
            asset(&assets, "runner_path")?,
            asset(&assets, "kernel_path")?,
        )?
        .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
    )?;
    let source = launch(&assets)?;
    let id = source.sandbox_id;
    let build = spec(&assets)?;
    let mut vm = factory.build_fresh(build.clone(), source)?;
    vm.start().await?;
    let captured = vm.snapshot().await?;
    let capture_dir = captured
        .manifest()
        .memory
        .image_config_path
        .parent()
        .unwrap()
        .to_path_buf();
    let mut metadata = publish_metadata(id, &build, &assets, &captured)?;
    metadata.alias = Some(agentenv::snapshot::SnapshotAlias::parse(
        "recording-cancel",
    )?);
    let root = tempfile::tempdir_in(&app.runtime_path)?;
    let manager = posix_manager(root.path())?;
    let aliases = root.path().join("repository/catalog/aliases");
    std::fs::create_dir_all(&aliases)?;
    let lock = std::fs::File::create(aliases.join("recording-cancel.lock"))?;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "hold test publication lock"
    );
    let task_manager = manager.clone();
    let publish =
        tokio::spawn(async move { task_manager.publish_captured(metadata, captured).await });
    let device = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(device) = mapped_memory_device(&app.runtime_path).await {
                return device;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("recorder never acquired dedicated RAM device")?;
    assert!(
        !publish.is_finished(),
        "publication lock did not delay commit"
    );
    publish.abort();
    assert!(publish.await.unwrap_err().is_cancelled());
    drop(lock); // allow the already-owned blocking commit to finish
    vm.stop().await?;
    assert!(
        capture_dir.exists(),
        "publication cancellation dropped recorder backing"
    );
    assert!(
        device.exists(),
        "recording device disappeared before its window ended"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        capture_dir.exists(),
        "capture did not survive delayed recorder"
    );
    if shutdown {
        agentenv::snapshot::drain_startup_manifest_tasks(Duration::from_secs(10)).await;
        assert!(
            !device.exists() && !capture_dir.exists(),
            "shutdown missed the recorder after publication cancellation"
        );
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        while device.exists() || capture_dir.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("cancelled publication recorder did not drain device/capture")?;
    Ok(())
}

#[tokio::test]
#[ignore = "real Dragonball first-touch recording with startup-pack enabled"]
async fn dragonball_memory_recording_uses_dedicated_device() -> Result<()> {
    for (record, consume, failure) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (true, true, true),
    ] {
        recording_case(record, consume, failure).await?;
    }
    Ok(())
}

async fn recording_case(record: bool, consume: bool, failure: bool) -> Result<()> {
    let (mut app, assets) = fixture().await?;
    anyhow::ensure!(
        app.snapshot.memory_startup_pack.enabled,
        "enable startup recording"
    );
    app.snapshot.memory_startup_pack.enabled = record;
    app.snapshot.memory_startup_pack.consume_enabled = consume;
    app.snapshot.memory_startup_pack.record_min_window_ms = 1500;
    app.snapshot.memory_startup_pack.record_max_window_ms = 3000;
    if failure {
        app.snapshot.memory_startup_pack.record_budget_secs = 0;
    }
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?
    .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental);
    let factory = DragonballSandboxFactory::new(policy)?;
    let source = launch(&assets)?;
    let id = source.sandbox_id;
    let build = spec(&assets)?;
    let mut vm = factory.build_fresh(build.clone(), source)?;
    vm.start().await?;
    let root = tempfile::tempdir_in(&app.runtime_path)?;
    let manager = posix_manager(root.path())?;
    let captured = vm.snapshot().await?;
    let capture_dir = captured
        .manifest()
        .memory
        .image_config_path
        .parent()
        .unwrap()
        .to_path_buf();
    // Pre-read the exact captured layers through another block device and keep
    // its FD/cache alive. Recording must still see positive first-touch reads
    // on a different device, even though every lower file is already warm.
    let warm = if record && !failure {
        use uvm_ublk_daemon::protocol::{DaemonRequest, DaemonResponse};
        let response = daemon_call(
            &app,
            DaemonRequest::CreateOverlaybd {
                image_config: captured.manifest().memory.image_config_path.clone(),
                global_config: app.memory_snapshot.overlaybd_global_config_path.clone(),
            },
        )
        .await?;
        let DaemonResponse::DeviceCreated {
            dev_id,
            device_path,
        } = response
        else {
            anyhow::bail!("warm fixture: {response:?}");
        };
        let path = device_path.clone();
        let fd = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let mut fd = std::fs::File::open(path)?;
            std::io::copy(&mut fd, &mut std::io::sink())?;
            Ok(fd)
        })
        .await??;
        Some((dev_id, device_path, fd))
    } else {
        None
    };
    let metadata = publish_metadata(id, &build, &assets, &captured)?;
    let observe = async {
        if let Some((_, path, _)) = &warm {
            let recording_device = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(device) = mapped_memory_device(&app.runtime_path).await {
                        return device;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("no dedicated recording mapping")?;
            anyhow::ensure!(
                &recording_device != path,
                "recorder reused a warm memory block device"
            );
        }
        Ok::<_, anyhow::Error>(())
    };
    // Publication can outlast the recording window. Observe concurrently, not
    // after publication has already allowed the throwaway VM to be reaped.
    let (published, observed) = tokio::join!(manager.publish_captured(metadata, captured), observe);
    let record = published?;
    vm.stop().await?;
    observed?;
    tokio::time::timeout(Duration::from_secs(20), async {
        while capture_dir.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("recorder/publication capture lease did not drain")?;
    if let Some((dev_id, _, fd)) = warm {
        drop(fd);
        let deleted = daemon_call(
            &app,
            uvm_ublk_daemon::protocol::DaemonRequest::Delete { dev_id },
        )
        .await?;
        assert!(
            matches!(deleted, uvm_ublk_daemon::protocol::DaemonResponse::Deleted),
            "{deleted:?}"
        );
    }
    let recorded = manager
        .get(&record.id.to_string())
        .await?
        .context("published snapshot missing")?;
    assert_eq!(
        recorded
            .committed
            .as_ref()
            .unwrap()
            .memory_startup
            .is_some(),
        app.snapshot.memory_startup_pack.enabled && !failure
    );
    let runnable = manager.resolve_runnable(recorded).await?;
    if let Some(pack) = &runnable.manifest().memory_startup_pack {
        let agentenv::snapshot::ResolvedStartupPackSource::LocalPath(path) = &pack.source else {
            anyhow::bail!("expected POSIX startup manifest");
        };
        let manifest = overlaybd::startup_manifest::decode_manifest(&std::fs::read(path)?)?;
        anyhow::ensure!(
            !manifest.prefix_pages.is_empty() || !manifest.ranges.is_empty(),
            "no first-touch pages recorded"
        );
    }
    let mut restored = factory.build_from_snapshot(&runnable, launch(&assets)?)?;
    let started = restored.start().await;
    let stopped = restored.stop().await;
    started?;
    stopped?;
    Ok(())
}

#[tokio::test]
#[ignore = "real 1/8GiB guests with the dedicated DAMON_RECLAIM qualification kernel"]
async fn dragonball_memory_damon_uses_actual_regions() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    for (memory_mib, end) in [(1024, 1073741824u64), (8192, 9663676416)] {
        let mut build = spec(&assets)?;
        build.resources.memory_mib = memory_mib;
        build.context.user = Some("root".into());
        let mut vm = factory.build_fresh(build, launch(&assets)?)?;
        vm.start().await?;
        let output = vm.run_command("/bin/sh", &["-ec",
            "p=/sys/module/damon_reclaim/parameters; cat $p/enabled $p/monitor_region_start $p/monitor_region_end",
        ]).await?;
        let hole = if memory_mib == 8192 {
            Some(
                vm.run_command(
                    "/bin/sh",
                    &[
                        "-ec",
                        r#"
p=/sys/module/damon_reclaim/parameters
t=/sys/kernel/tracing
echo N > $p/enabled
mount -t tracefs tracefs $t
echo 'p:db_damon_page damon_get_page pfn=%di:u64' > $t/kprobe_events
echo 'r:db_damon_ret damon_get_page page=$retval:x64' >> $t/kprobe_events
echo 1 > $t/events/kprobes/enable
echo 3221225472 > $p/monitor_region_start
echo 4294967296 > $p/monitor_region_end
echo 1000 > $p/wmarks_high
echo 1000 > $p/wmarks_mid
echo 0 > $p/wmarks_low
echo Y > $p/enabled
sleep 1
echo N > $p/enabled
echo 0 > $t/events/kprobes/enable
cat $t/trace
"#,
                    ],
                )
                .await?,
            )
        } else {
            None
        };
        vm.stop().await?;
        assert_eq!(
            output.exit_code, 0,
            "DAMON_RECLAIM kernel prerequisite: {}",
            output.stderr
        );
        assert_eq!(
            output.stdout.lines().collect::<Vec<_>>(),
            vec!["Y".to_string(), "4096".to_string(), end.to_string()],
            "DAMON must monitor actual RAM span, including high RAM"
        );
        if let Some(hole) = hole {
            assert_eq!(hole.exit_code, 0, "{}", hole.stderr);
            let mut entries = 0;
            let mut returns = 0;
            for line in hole.stdout.lines() {
                if let Some((_, pfn)) = line.split_once("pfn=") {
                    let pfn: u64 = pfn.trim().parse()?;
                    assert!(
                        (786432..1048576).contains(&pfn),
                        "probe sampled outside MMIO hole: {line}"
                    );
                    entries += 1;
                }
                if let Some((_, page)) = line.split_once("page=") {
                    assert_eq!(
                        u64::from_str_radix(page.trim().trim_start_matches("0x"), 16)?,
                        0,
                        "non-RAM PFN accepted: {line}"
                    );
                    returns += 1;
                }
            }
            assert!(
                entries > 10 && returns > 10,
                "no actual DAMON invalid-PFN observations: {}",
                hole.stdout
            );
            eprintln!("DAMON MMIO-hole sampling: {entries} calls, {returns} NULL returns");
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "real daemon observes rootfs and restored memory readiness notifications"]
async fn dragonball_memory_ready_releases_rootfs_and_memory_downloads() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(
        DragonballRuntimePolicy::from_app_config(
            &app,
            asset(&assets, "runner_path")?,
            asset(&assets, "kernel_path")?,
        )?
        .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
    )?;
    let log = app
        .ublk
        .daemon_log_path
        .clone()
        .unwrap_or_else(|| app.home_path.join("logs/ublk-daemon.log"));
    let build = spec(&assets)?;
    let source = launch(&assets)?;
    let id = source.sandbox_id;
    let mut vm = factory.build_fresh(build.clone(), source)?;
    vm.start().await?;
    let runtime = vm.work_dir()?.canonicalize()?;
    let captured = vm.snapshot().await?;
    let metadata = publish_metadata(id, &build, &assets, &captured)?;
    let root = tempfile::tempdir_in(&app.runtime_path)?;
    let manager = posix_manager(root.path())?;
    let record = manager.publish_captured(metadata, captured).await?;
    vm.stop().await?;
    let runnable = manager.resolve_runnable(record.clone()).await?;
    let memory = runnable
        .manifest()
        .memory
        .image_config_path
        .canonicalize()?;
    let mut restored = factory.build_from_snapshot(&runnable, launch(&assets)?)?;
    restored.start().await?;
    restored.stop().await?;
    let lines = std::fs::read_to_string(&log)?;
    let ready: Vec<_> = lines
        .lines()
        .filter(|line| line.contains("sandbox reported envd-ready; releasing held downloads"))
        .collect();
    assert!(
        ready
            .iter()
            .any(|line| line.contains(&runtime.join("overlaybd").to_string_lossy().to_string())),
        "fresh rootfs download gate was not released: {}",
        log.display()
    );
    assert!(
        ready
            .iter()
            .any(|line| line.contains(&memory.to_string_lossy().to_string())),
        "restored memory download gate was not released: {}",
        log.display()
    );
    drop(restored);
    drop(runnable);
    manager.delete(record.id.to_string()).await?;
    Ok(())
}
