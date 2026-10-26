use agentenv::volume::VolumeCatalog;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use super::dragonball_support::{
    asset, fixture, fork_command, fork_spec, guest_command, launch, posix_manager,
    publish_metadata, runtime_frame, spec, start_active_io, start_echo, ActiveTraffic,
};
use agentenv::{
    cfg::AppConfig,
    sandbox::{
        dragonball::{DragonballRuntimePolicy, DragonballSandboxFactory},
        ExtraDrive, SandboxBackend, SandboxBackendFactory, SandboxExecutor,
    },
    types::SandboxId,
    virtualization::VirtualizationMode,
};
use agentenv_vmm_protocol::runtime::RuntimePhase;
use anyhow::{Context, Result};
use serde_json::Value;

#[tokio::test]
#[ignore = "real POSIX VM snapshot with managed volumes, attached disk and deleted source catalogs"]
async fn dragonball_lifecycle_posix_volume_snapshot_rebinds_logical_devices() -> Result<()> {
    use agentenv::snapshot::{
        backends::{PosixFsBackend, PosixFsBackendConfig},
        SnapshotManager, SnapshotVolume,
    };
    use agentenv::volume::{VolumeManager, VolumeMode, VolumeRecord};
    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (repository, resolver) = PosixFsBackend::new(PosixFsBackendConfig {
        root: root.path().join("repository"),
        cache_root: Some(root.path().join("cache")),
        runtime_cache_root: Some(root.path().join("runtime-cache")),
    })?
    .into_parts();
    let snapshots = SnapshotManager::from_parts(repository.clone(), resolver, None);
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository.clone())
            .await?;
    let mut build = spec(&assets)?;
    build.context.user = Some("root".into());
    let mut originals = vec![];
    for (name, mount_path, read_only, volume) in [
        ("db-m1-published-data", "/mnt/data", false, true),
        ("db-m1-published-data2", "/mnt/data2", false, true),
        ("db-m1-published-ro", "/mnt/ro", true, true),
        ("db-m1-published-attached", "/mnt/attached", false, false),
    ] {
        let seed = volumes
            .create(
                format!("{name}-seed"),
                if read_only {
                    VolumeMode::ReadOnly
                } else {
                    VolumeMode::Exclusive
                },
                None,
                None,
                128,
            )
            .await?;
        // Clone committed logical layers and materialize through the normal
        // resolver, rather than use an undescribed arbitrary local lower as an attached image.
        let record = volumes
            .create(name.into(), seed.mode, Some(seed.id.clone()), None, 128)
            .await?;
        let record = volumes.materialize_backing(&record.id).await?;
        volumes.delete(&seed.id).await?;
        let image = record
            .backing_image_config
            .clone()
            .context("volume backing")?;
        let output = image.parent().context("volume directory")?.to_path_buf();
        build.extra_drives.push(ExtraDrive::Overlaybd {
            drive_id: record.id.clone(),
            image_config_path: image,
            read_only,
            mount_path: mount_path.into(),
            virtual_size: Some(128 << 20),
            sub_path: None,
            snapshot_output_dir: (volume && !read_only).then_some(output),
            volume,
        });
        originals.push(record);
    }
    let source_launch = launch(&assets)?;
    let id = source_launch.sandbox_id;
    let owner = id.to_string();
    let volume_ids = originals[..3]
        .iter()
        .map(|record| record.id.clone())
        .collect::<Vec<_>>();
    for volume_id in &volume_ids {
        volumes.reserve(volume_id, &owner).await?;
    }
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(build.clone(), source_launch)?;
    source.start().await?;
    let output = source.run_command("/bin/sh", &["-ec",
        "printf published-root >/var/tmp/db-m1-published; printf published-ram >/dev/shm/db-m1-published; printf published-volume >/mnt/data/db-m1-published; printf published-volume2 >/mnt/data2/db-m1-published; printf published-attached >/mnt/attached/db-m1-published; ! touch /mnt/ro/forbidden; sync"
    ]).await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let captured = source.snapshot().await?;
    assert_eq!(
        captured.manifest().attached_drives.len(),
        1,
        "managed volumes must not be republished as attached disks"
    );
    assert!(
        captured
            .manifest()
            .volume_slots
            .iter()
            .any(
                |slot| slot.mount_path.as_deref() == Some(Path::new("/mnt/ro"))
                    && slot.guest_read_only
            ),
        "fresh bound RO volume must negotiate guest RO"
    );
    // Keep A alive while a later capture B advances the source's volume catalog.
    // Publishing A must consume A's leased generation, not the latest image.json.
    let later = source.run_command("/bin/sh", &["-ec",
        "printf later-root >/var/tmp/db-m1-published; printf later-ram >/dev/shm/db-m1-published; printf later-volume >/mnt/data/db-m1-published; printf later-volume2 >/mnt/data2/db-m1-published; sync"
    ]).await?;
    assert_eq!(later.exit_code, 0, "{}", later.stderr);
    let later_capture = source.snapshot().await?;
    volumes
        .recover_and_publish_backings(&owner, &volume_ids)
        .await?;
    let mut metadata = publish_metadata(id, &build, &assets, &captured)?;
    for (index, mount_path) in ["/mnt/data", "/mnt/data2", "/mnt/ro"].iter().enumerate() {
        let record = volumes.get(&volume_ids[index]).await?;
        metadata.volume_snapshots.push(SnapshotVolume {
            mount_path: (*mount_path).into(),
            mode: record.mode,
            size_mb: record.size_mb,
            layers: record.backing_layers,
        });
    }
    let record = agentenv::sandbox::publish_captured(&snapshots, metadata, captured).await?;
    drop(later_capture);
    source.stop().await?;
    drop(source);
    volumes.replace_owner_for(&owner, None, &volume_ids).await?;
    for original in &originals {
        volumes.delete(&original.id).await?;
    }
    let runnable = snapshots
        .load_runnable(record.id.to_string())
        .await?
        .context("published VM")?;
    let mut target_launch = launch(&assets)?;
    target_launch.snapshot_id = record.id.to_string();
    target_launch.extra_drives_in_snapshot = true;
    let target_owner = target_launch.sandbox_id.to_string();
    let mut replacements = vec![];
    for saved in &runnable.committed().volume_snapshots {
        // Exercise the public repository's logical catalog creation, then let
        // VolumeManager own backing materialization/reservations, as in API restore.
        let child: VolumeRecord = serde_json::from_value(serde_json::json!({
            "id": format!("vol_{}", uuid::Uuid::now_v7().simple()),
            "name": format!("db-m1-restore-{}", uuid::Uuid::now_v7().simple()),
            "mode": saved.mode, "sizeMb": saved.size_mb, "status": "ready",
            "reservedBySandboxId": null, "backingLayers": saved.layers, "readOnlyMounts": [],
        }))?;
        repository.create_volume(child.clone()).await?;
        let child = volumes.materialize_backing(&child.id).await?;
        volumes.reserve(&child.id, &target_owner).await?;
        replacements.push(child.id.clone());
        target_launch.extra_drives.push(ExtraDrive::Overlaybd {
            drive_id: child.id,
            image_config_path: child.backing_image_config.context("restored backing")?,
            read_only: saved.mode == VolumeMode::ReadOnly,
            mount_path: saved.mount_path.clone().into(),
            virtual_size: Some(saved.size_mb << 20),
            sub_path: None,
            snapshot_output_dir: None,
            volume: true,
        });
    }
    target_launch.extra_drives.reverse(); // Binding must not depend on catalog/launch order.
    let before = std::fs::read_dir(&app.runtime_path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    for defect in ["missing", "duplicate_mount", "mode", "capacity", "subpath"] {
        let mut invalid = target_launch.clone();
        let duplicate_mount = invalid.extra_drives[1].mount_path().to_path_buf();
        let ExtraDrive::Overlaybd {
            mount_path,
            read_only,
            virtual_size,
            sub_path,
            ..
        } = &mut invalid.extra_drives[0];
        match defect {
            "duplicate_mount" => *mount_path = duplicate_mount,
            "mode" => *read_only = !*read_only,
            "capacity" => *virtual_size = Some(64 << 20),
            "subpath" => *sub_path = Some("unexpected".into()),
            "missing" => {
                invalid.extra_drives.pop();
            }
            _ => unreachable!(),
        }
        assert!(
            factory.build_from_snapshot(&runnable, invalid).is_err(),
            "accepted {defect}"
        );
    }
    let after = std::fs::read_dir(&app.runtime_path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    assert_eq!(
        before, after,
        "invalid volume binding allocated runtime leases"
    );
    let token = target_launch.envd_access_token.clone();
    let mut target = factory.build_from_snapshot(&runnable, target_launch)?;
    target.start_nowait().await?;
    let works = std::fs::read_dir(&app.runtime_path)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| !before.contains(path) && path.join("runtime-config.json").is_file())
        .collect::<Vec<_>>();
    assert_eq!(works.len(), 1, "identify the held target's owned runtime");
    let machine: agentenv_vmm_protocol::runtime::RuntimeVmConfig =
        serde_json::from_slice(&std::fs::read(works[0].join("runtime-config.json"))?)?;
    let captured_id = &runnable
        .manifest()
        .volume_devices
        .iter()
        .find(|drive| drive.mount_path == Path::new("/mnt/data"))
        .context("captured volume identity")?
        .drive_id;
    let disk = machine
        .block_devices
        .iter()
        .find(|disk| &disk.drive_id == captured_id)
        .context("held volume device")?;
    let physical = tokio::process::Command::new("debugfs")
        .args(["-D", "-R", "cat /db-m1-published"])
        .arg(&disk.path_on_host)
        .output()
        .await?;
    assert!(
        physical.status.success(),
        "{}",
        String::from_utf8_lossy(&physical.stderr)
    );
    assert_eq!(
        physical.stdout, b"published-volume",
        "delayed publication mixed volume generations before guest resume"
    );
    target.wait_for_ready().await?;
    let output = fork_command(target.as_ref(), token.as_ref(),
        "echo 3 >/proc/sys/vm/drop_caches; test \"$(cat /var/tmp/db-m1-published)\" = published-root; test \"$(cat /dev/shm/db-m1-published)\" = published-ram; test \"$(cat /mnt/data/db-m1-published)\" = published-volume; test \"$(cat /mnt/data2/db-m1-published)\" = published-volume2; test \"$(cat /mnt/attached/db-m1-published)\" = published-attached; ! touch /mnt/ro/forbidden; printf child-volume >/mnt/data/db-m1-published; sync"
    ).await?;
    assert_eq!(
        output.exit_code, 0,
        "logical volume/attached rebind: {}",
        output.stderr
    );
    target.stop().await?;
    volumes
        .replace_owner_for(&target_owner, None, &replacements)
        .await?;
    for replacement in replacements {
        volumes.delete(&replacement).await?;
    }
    snapshots.delete(record.id.to_string()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real Linux KVM/ublk POSIX publication and committed full-RAM restore"]
async fn dragonball_lifecycle_posix_publication_restores_after_source_stop() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let spec = spec(&assets)?;
    let source_launch = launch(&assets)?;
    let (id, token) = (
        source_launch.sandbox_id,
        source_launch.envd_access_token.clone(),
    );
    let mut source = factory.build_fresh(spec.clone(), source_launch)?;
    source.start().await?;
    assert_eq!(source.run_command("/bin/sh", &["-ec", "printf published-disk >/var/tmp/db-m1-published; printf published-ram >/dev/shm/db-m1-published; sync"]).await?.exit_code, 0);
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    let before = ram_progress(&source, &marker).await?;
    let captured = source.snapshot().await?;
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    let capture_root = captured
        .manifest()
        .vm_state
        .path
        .parent()
        .context("capture root")?
        .to_path_buf();
    let metadata = publish_metadata(id, &spec, &assets, &captured)?;
    let root = tempfile::tempdir()?;
    let manager = posix_manager(root.path())?;
    let record = agentenv::sandbox::publish_captured(&manager, metadata, captured).await?;
    assert!(ram_progress(&source, &marker).await? > before);
    source.stop().await?;
    drop(source);
    // Publication may still own this capture through its asynchronous startup
    // recorder. Source stop does not synchronously drain that independent VM.
    tokio::time::timeout(Duration::from_secs(30), async {
        while capture_root.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("publication/recording retained the node-local capture owner")?;
    let runnable = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("committed runnable")?;
    let mut launch = launch(&assets)?;
    launch.snapshot_id = record.id.to_string();
    launch.env_vars = Some(HashMap::from([(
        "DB_M1_PUBLISHED_ENV".into(),
        "current-launch".into(),
    )]));
    let (child_id, child_token) = (launch.sandbox_id, launch.envd_access_token.clone());
    let mut child = factory.build_from_snapshot(&runnable, launch)?;
    child.start().await?;
    let output = fork_command(child.as_ref(), child_token.as_ref(), &format!(
        "test \"$(cat /var/tmp/db-m1-published)\" = published-disk; test \"$(cat /dev/shm/db-m1-published)\" = published-ram; test \"$E2B_SANDBOX_ID\" = '{child_id}'; test \"$DB_M1_PUBLISHED_ENV\" = current-launch; test \"$(id -u)\" = 1000; test \"$PWD\" = /tmp; /tmp/db-m1-active-io read '{marker}'"
    )).await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(
        output
            .stdout
            .split_whitespace()
            .last()
            .context("restored RAM progress")?
            .parse::<u64>()?
            > before
    );
    child.stop().await?;
    manager.delete(record.id.to_string()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real Linux KVM/ublk POSIX commit rejection with a live source"]
async fn m1_publish_failure_does_not_stop_source() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let spec = spec(&assets)?;
    let launch = launch(&assets)?;
    let (id, token) = (launch.sandbox_id, launch.envd_access_token.clone());
    let mut source = factory.build_fresh(spec.clone(), launch)?;
    source.start().await?;
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    let root = tempfile::tempdir()?;
    let manager = posix_manager(root.path())?;
    let alias = agentenv::snapshot::SnapshotAlias::parse("db-m1-publish-failure")?;
    let captured = source.snapshot().await?;
    let mut metadata = publish_metadata(id, &spec, &assets, &captured)?;
    metadata.alias = Some(alias.clone());
    let record = agentenv::sandbox::publish_captured(&manager, metadata, captured).await?;
    let before = ram_progress(&source, &marker).await?;
    let captured = source.snapshot().await?;
    let mut metadata = publish_metadata(id, &spec, &assets, &captured)?;
    metadata.alias = Some(alias);
    let rejected = metadata.id.to_string();
    let error = agentenv::sandbox::publish_captured(&manager, metadata, captured)
        .await
        .expect_err("real alias-conflicting repository commit");
    assert!(
        matches!(
            error,
            agentenv::repository::RepositoryError::AliasConflict { .. }
        ),
        "{error:#}"
    );
    assert!(manager.get(rejected).await?.is_none());
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert!(ram_progress(&source, &marker).await? > before);
    source.stop().await?;
    manager.delete(record.id.to_string()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real Linux KVM/ublk captured record mismatch preflight"]
async fn m1_record_mismatch_allocates_nothing() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let spec = spec(&assets)?;
    let launch = launch(&assets)?;
    let mut source = factory.build_fresh(spec.clone(), launch.clone())?;
    source.start().await?;
    let captured = source.snapshot().await?;
    let root = tempfile::tempdir()?;
    let manager = posix_manager(root.path())?;
    let record = agentenv::sandbox::publish_captured(
        &manager,
        publish_metadata(launch.sandbox_id, &spec, &assets, &captured)?,
        captured,
    )
    .await?;
    let mut changed = record.clone();
    changed
        .committed
        .as_mut()
        .context("committed")?
        .runtime_versions
        .backend = "firecracker".into();
    let record_path = root
        .path()
        .join("repository/catalog/records")
        .join(format!("{}.json", record.id));
    std::fs::write(record_path, serde_json::to_vec(&changed)?)?;
    let mismatched = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("tampered runnable")?;
    let before = std::fs::read_dir(&app.runtime_path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    let error = match factory.build_from_snapshot(&mismatched, launch) {
        Err(error) => error,
        Ok(_) => anyhow::bail!("mismatched committed identity was accepted"),
    };
    assert!(
        format!("{error:#}").contains("identity conflict"),
        "wrong preflight failure: {error:#}"
    );
    let after = std::fs::read_dir(&app.runtime_path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    assert_eq!(before, after, "record mismatch allocated runtime leases");
    source.stop().await?;
    manager.delete(record.id.to_string()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real owned runner SIGKILL with retained disk/network/capture leases"]
async fn m1_runner_death_retains_leases_until_reap() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let captured = source.snapshot().await?;
    let capture_root = captured
        .manifest()
        .rootfs
        .image_config_path
        .parent()
        .context("captured disk root")?
        .to_path_buf();
    drop(captured);
    let work = source.work_dir()?.to_path_buf();
    let machine: agentenv_vmm_protocol::runtime::RuntimeVmConfig =
        serde_json::from_slice(&std::fs::read(work.join("runtime-config.json"))?)?;
    let tap = &machine.network.tap_name;
    let socket = tokio::net::UnixStream::connect(work.join("runner.sock")).await?;
    let credential = socket.peer_cred()?;
    assert_eq!(credential.uid(), unsafe { libc::geteuid() });
    let pid = credential.pid().context("owned runner PID")?;
    drop(socket);
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let device_paths = machine
        .block_devices
        .iter()
        .filter_map(|disk| {
            std::fs::metadata(&disk.path_on_host)
                .ok()
                .filter(|metadata| metadata.file_type().is_block_device())
                .map(|_| disk.path_on_host.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        device_paths.len(),
        1,
        "fresh profile has one owned user-root ublk device; tools is a shared regular file"
    );
    let ns_metadata = std::fs::metadata(format!("/proc/{pid}/ns/net"))?;
    let namespace = std::fs::read_dir(app.runtime_path.join("netns"))?
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            let metadata = std::fs::metadata(entry.path()).ok()?;
            (metadata.dev() == ns_metadata.dev() && metadata.ino() == ns_metadata.ino())
                .then(|| entry.path())
        })
        .context("owned runner's bound network namespace")?;
    let ns_arg = format!("--net={}", namespace.display());
    let tap_before = tokio::process::Command::new("nsenter")
        .args([&ns_arg, "--", "ip", "link", "show", "dev", tap])
        .output()
        .await?;
    assert!(
        tap_before.status.success(),
        "owned TAP missing before fault: {}",
        String::from_utf8_lossy(&tap_before.stderr)
    );
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    tokio::time::timeout(Duration::from_secs(2), async {
        while source.executor().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "runner was reaped outside its owner"
    );
    assert!(
        work.is_dir() && capture_root.is_dir(),
        "live leases released before owner reap"
    );
    for path in &device_paths {
        assert!(path.exists(), "disk lease released before reap");
    }
    let tap_after = tokio::process::Command::new("nsenter")
        .args([&ns_arg, "--", "ip", "link", "show", "dev", tap])
        .output()
        .await?;
    assert!(
        tap_after.status.success(),
        "network lease released before reap: {}",
        String::from_utf8_lossy(&tap_after.stderr)
    );
    source.stop().await?;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "runner not reaped"
    );
    assert!(
        !work.exists() && !capture_root.exists(),
        "owned directories retained after reap"
    );
    for path in &device_paths {
        assert!(
            !path.exists(),
            "owned disk lease leaked after reap: {}",
            path.display()
        );
    }
    assert!(
        machine.block_devices[0].path_on_host.is_file(),
        "immutable tools asset was deleted during cleanup"
    );
    if namespace.exists() {
        // Other warm slots can precede the released slot. Keep acquisitions
        // live so the pool cannot repeatedly return an unrelated namespace.
        let slots = std::fs::read_dir(app.runtime_path.join("netns"))?.count();
        let mut replacements = Vec::new();
        let mut reused = false;
        for _ in 0..=slots {
            if !namespace.exists() {
                break; // Background maintenance may destroy the idle slot.
            }
            let mut next = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
            next.start().await?;
            let socket =
                tokio::net::UnixStream::connect(next.work_dir()?.join("runner.sock")).await?;
            let next_pid = socket
                .peer_cred()?
                .pid()
                .context("replacement runner PID")?;
            let next_ns = std::fs::metadata(format!("/proc/{next_pid}/ns/net"))?;
            reused = (next_ns.dev(), next_ns.ino()) == (ns_metadata.dev(), ns_metadata.ino());
            replacements.push(next);
            if reused {
                break;
            }
        }
        for next in &mut replacements {
            next.stop().await?;
        }
        assert!(
            reused || !namespace.exists(),
            "released namespace was neither reusable nor destroyed"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separate real server processes and disposable KVM/ublk/network fixture"]
async fn dragonball_lifecycle_server_restart_loads_same_paused_id() -> Result<()> {
    use agentenv::orchestrator::{
        CreateSandboxRequest, FileBackedSandboxPersister, InMemoryMetadataStore, NewTimeout,
        Orchestrator, ProxyLookupResult, SandboxLaunchSource, SandboxState, SandboxTimeoutAction,
    };
    let phase = std::env::var("AENV_DB_M1_RESTART_PHASE").ok();
    if phase.is_none() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let extension_url = format!("http://{}", listener.local_addr()?);
        let (hooks_tx, mut hooks_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => return Err::<(), anyhow::Error>(error.into()),
                };
                let mut header = vec![];
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await?);
                    anyhow::ensure!(header.len() < 8192, "hook header limit");
                }
                let header = std::str::from_utf8(&header)?;
                let path = header
                    .lines()
                    .next()
                    .context("hook request")?
                    .split_whitespace()
                    .nth(1)
                    .context("hook path")?
                    .to_owned();
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().to_owned())
                    })
                    .context("hook body length")?
                    .parse()?;
                anyhow::ensure!(length < 65536, "hook body limit");
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await?;
                hooks_tx.send((path, serde_json::from_slice::<Value>(&body)?))?;
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
            }
        });
        let root = tempfile::tempdir()?;
        for phase in ["create", "restore"] {
            if phase == "restore" {
                if let Some(binary) = std::env::var_os("AENV_DB_M1_LEGACY_SERVER") {
                    // Run an unchanged old FC server between the two DB processes.
                    let runtime = root.path().join("runtime-legacy");
                    std::fs::create_dir(&runtime)?;
                    let log_path = root.path().join("legacy-server.log");
                    let log = std::fs::File::create(&log_path)?;
                    let mut old = tokio::process::Command::new(binary)
                        .env("AENV_HOME_PATH", root.path().join("legacy-home"))
                        .env("AENV_RUNTIME_PATH", runtime)
                        .env(
                            "AENV_PERSISTED_SANDBOX_STORE_PATH",
                            root.path().join("paused"),
                        )
                        .env("API_ADDR", "127.0.0.1:0")
                        .stdout(log.try_clone()?)
                        .stderr(log)
                        .kill_on_drop(true)
                        .spawn()?;
                    let ready = tokio::time::timeout(Duration::from_secs(60), async {
                        loop {
                            if std::fs::read_to_string(&log_path)?.contains("API server listening")
                            {
                                return Ok::<_, anyhow::Error>(());
                            }
                            anyhow::ensure!(old.try_wait()?.is_none(), "legacy server exited");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    })
                    .await;
                    if !matches!(ready, Ok(Ok(()))) {
                        if old.try_wait()?.is_none() {
                            old.kill().await?;
                        }
                        let _ = old.wait().await?;
                        anyhow::bail!(
                            "legacy server failed before readiness: {}",
                            std::fs::read_to_string(&log_path)?
                        );
                    }
                    let pid = old.id().context("legacy server PID")?;
                    anyhow::ensure!(
                        unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0,
                        "signal owned legacy server"
                    );
                    let status =
                        tokio::time::timeout(Duration::from_secs(30), old.wait()).await??;
                    anyhow::ensure!(
                        status.success(),
                        "legacy server shutdown failed: {}",
                        std::fs::read_to_string(&log_path)?
                    );
                    anyhow::ensure!(
                        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                        "legacy server still alive"
                    );
                    let legacy_log = std::fs::read_to_string(&log_path)?;
                    assert!(legacy_log.contains(root.path().join("paused").to_str().unwrap()));
                    assert!(legacy_log.contains("loaded paused sandbox records"));
                    assert!(root.path().join("paused/dragonball/records.db").is_dir());
                    eprintln!("legacy FC server loaded the isolated paused root, reached API readiness and exited; DB record retained");
                }
            }
            std::fs::create_dir(root.path().join(format!("runtime-{phase}")))?;
            let output = tokio::time::timeout(
                Duration::from_secs(180),
                tokio::process::Command::new(std::env::current_exe()?)
                    .args([
                        "--ignored",
                        "--exact",
                        "dragonball_lifecycle::dragonball_lifecycle_server_restart_loads_same_paused_id",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("AENV_DB_M1_RESTART_PHASE", phase)
                    .env("AENV_DB_M1_RESTART_ROOT", root.path())
                    .env(
                        "AENV_RUNTIME_PATH",
                        root.path().join(format!("runtime-{phase}")),
                    )
                    .env("AENV_CUSTOM_EXTENSION_URL", &extension_url)
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            anyhow::ensure!(
                output.status.success(),
                "{phase} server process failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if phase == "create" {
                let pids: Vec<u32> =
                    serde_json::from_slice(&std::fs::read(root.path().join("old-pids.json"))?)?;
                for pid in pids {
                    anyhow::ensure!(
                        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                        "old process {pid} still alive"
                    );
                }
            }
        }
        let mut hooks = vec![];
        for _ in 0..4 {
            hooks.push(
                tokio::time::timeout(Duration::from_secs(5), hooks_rx.recv())
                    .await?
                    .context("paired hook")?,
            );
        }
        assert!(hooks[0].0.ends_with("/start-fresh"));
        assert!(hooks[1].0.ends_with("/stop"));
        assert!(hooks[2].0.ends_with("/start-resume"));
        assert!(hooks[3].0.ends_with("/stop"));
        let params = serde_json::json!({"dbRestart": {"opaque": ["keep", 9], "flag": false}});
        assert_eq!(hooks[0].1["customExtensionParams"], params);
        assert_eq!(hooks[2].1["customExtensionParams"], params);
        assert_eq!(hooks[0].1["sandboxId"], hooks[2].1["sandboxId"]);
        assert_ne!(
            hooks[0].1["sandboxInstanceId"],
            hooks[2].1["sandboxInstanceId"]
        );
        for (start, stop) in [(0, 1), (2, 3)] {
            assert_eq!(
                hooks[start].1["sandboxInstanceId"],
                hooks[stop].1["sandboxInstanceId"]
            );
            assert_eq!(hooks[start].1["sandboxId"], hooks[stop].1["sandboxId"]);
        }
        server.abort();
        return Ok(());
    }
    // Initialize config before common's explicit test seed, exercising the real managed seed file.
    std::env::remove_var("AENV_SANDBOX_ACCESS_TOKEN_HASH_SEED");
    assert!(agentenv::cfg::ConfigManager::init_global()?
        .config()
        .sandbox
        .access_token_hash_seed
        .is_none());
    let (app, assets) = fixture().await?;
    let root = PathBuf::from(std::env::var_os("AENV_DB_M1_RESTART_ROOT").context("restart root")?);
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let orchestrator = Orchestrator::new(
        InMemoryMetadataStore::new(),
        factory,
        FileBackedSandboxPersister::new(root.join("paused"), VirtualizationMode::Kvm),
    )
    .await?;
    let metadata = if phase.as_deref() == Some("create") {
        let spec = spec(&assets)?;
        let launch = launch(&assets)?;
        let metadata = orchestrator
            .create_sandbox(CreateSandboxRequest {
                source: SandboxLaunchSource::Image {
                    image_ref: launch.snapshot_id,
                    overlaybd_config_path: spec.image_config_path,
                    context: Box::new(spec.context),
                    resources: Some(spec.resources),
                    extra_drives: spec.extra_drives,
                    extra_boot_args: None,
                    image_configs: Box::new(serde_json::from_value(
                        assets["image_configs"].clone(),
                    )?),
                },
                extra_drives: vec![],
                extra_drives_in_snapshot: false,
                timeout: Some(Duration::from_secs(600)),
                timeout_action: SandboxTimeoutAction::Pause,
                auto_resume: false,
                user_metadata: None,
                env_vars: Some(HashMap::from([(
                    "DB_RESTART_CONTEXT".into(),
                    "survives".into(),
                )])),
                network_policy: launch.network.unwrap(),
                secure: true,
                custom_extension_params: Some(serde_json::from_value(
                    serde_json::json!({"dbRestart": {"opaque": ["keep", 9], "flag": false}}),
                )?),
                volume_mounts: HashMap::new(),
            })
            .await?;
        assert_eq!(metadata.runtime_versions.backend, "dragonball");
        std::fs::write(root.join("sandbox-id"), metadata.id.to_string())?;
        let mut pids = vec![std::process::id()];
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if std::fs::read_link(entry.path().join("exe")).ok().as_ref()
                == Some(&asset(&assets, "runner_path")?)
            {
                let status = std::fs::read_to_string(entry.path().join("status"))?;
                if status
                    .lines()
                    .any(|line| line == format!("PPid:\t{}", std::process::id()))
                {
                    pids.push(pid);
                }
            }
        }
        assert_eq!(pids.len(), 2, "one actual runner belongs to this server");
        std::fs::write(root.join("old-pids.json"), serde_json::to_vec(&pids)?)?;
        metadata
    } else {
        let id = SandboxId::parse_str(std::fs::read_to_string(root.join("sandbox-id"))?.trim())?;
        let paused = orchestrator
            .get_sandbox(&id)
            .await?
            .context("same paused sandbox exists")?;
        assert_eq!(paused.state, SandboxState::Paused);
        assert_eq!(paused.runtime_versions.backend, "dragonball");
        assert!(paused.paused_state.is_some());
        orchestrator
            .resume_sandbox(id, NewTimeout::UseExisting)
            .await?
    };
    let ProxyLookupResult::Ready(target) = orchestrator.proxy_lookup_for(&metadata.id).await?
    else {
        anyhow::bail!("resumed server has no ready route");
    };
    let token = orchestrator
        .get_envd_access_token(&metadata)
        .context("secure token")?;
    let script = if phase.as_deref() == Some("create") {
        "printf restart-disk >/var/tmp/db-m1-restart; printf restart-ram >/dev/shm/db-m1-restart; sync; test \"$DB_RESTART_CONTEXT\" = survives"
    } else {
        "test \"$(cat /var/tmp/db-m1-restart)\" = restart-disk; test \"$(cat /dev/shm/db-m1-restart)\" = restart-ram; test \"$DB_RESTART_CONTEXT\" = survives; test \"$E2B_SANDBOX_ID\" = \"$EXPECTED_SANDBOX\""
    };
    // The persisted context restores UID/workdir/env; current metadata restores the same ID.
    let script = format!(
        "test \"$(id -u)\" = 1000; test \"$PWD\" = /tmp; {}",
        script.replace("$EXPECTED_SANDBOX", &metadata.id.to_string())
    );
    let output = guest_command(
        &format!("http://{}:49983", target.ip),
        Some(&token),
        &script,
    )
    .await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    if phase.as_deref() == Some("create") {
        let blocker = root.join("paused/dragonball/records.db");
        std::fs::create_dir_all(blocker.parent().unwrap())?;
        std::fs::write(&blocker, b"test-only RocksDB open failure")?;
        assert!(orchestrator.pause_sandbox(metadata.id).await.is_err());
        assert_eq!(
            orchestrator.get_sandbox(&metadata.id).await?.unwrap().state,
            SandboxState::Running
        );
        fn has_sealed_root(path: &std::path::Path) -> Result<bool> {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    if has_sealed_root(&entry.path())? {
                        return Ok(true);
                    }
                } else if entry.file_name() == "snapshot.commit"
                    && path.file_name().is_some_and(|name| name == "rootfs")
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        assert!(
            has_sealed_root(
                &root
                    .join("paused/dragonball/artifacts")
                    .join(metadata.id.to_string())
            )?,
            "persistence failure removed a sealed lower still owned by the Running source"
        );
        let output = guest_command(&format!("http://{}:49983", target.ip), Some(&token),
            "test \"$(cat /var/tmp/db-m1-restart)\" = restart-disk; test \"$(cat /dev/shm/db-m1-restart)\" = restart-ram; sync").await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        std::fs::rename(&blocker, root.join("rocksdb-open-failure.evidence"))?;
        orchestrator.shutdown().await?;
        let paused = orchestrator
            .get_sandbox(&metadata.id)
            .await?
            .context("shutdown preserves paused metadata")?;
        assert_eq!(paused.state, SandboxState::Paused);
        let encoded = serde_json::to_string(
            &paused
                .paused_state
                .as_ref()
                .context("encoded state")?
                .encode()?,
        )?;
        assert!(
            !encoded.contains(token.expose()),
            "runtime token leaked into persisted state"
        );
        assert!(
            orchestrator
                .resume_sandbox(metadata.id, NewTimeout::UseExisting)
                .await
                .is_err(),
            "shutdown is permanent"
        );
        assert!(root.join("paused/dragonball/records.db").is_dir());
        assert!(root.join("paused/dragonball/artifacts").is_dir());
    } else {
        assert_eq!(metadata.state, SandboxState::Running);
        orchestrator.delete_sandbox(metadata.id).await?;
        orchestrator.shutdown().await?;
    }
    Ok(())
}

async fn fork_isolation(invalid_child: bool, volume: bool) -> Result<()> {
    use agentenv::volume::{VolumeManager, VolumeMode};
    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, _, repository) = crate::common::snapshot_test_parts(root.path());
    let manager =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let data = manager
        .create("db-m1-data".into(), VolumeMode::Exclusive, None, None, 1024)
        .await?;
    let image = data.backing_image_config.clone().context("volume image")?;
    let output = image.parent().context("volume directory")?.to_path_buf();
    let drive = ExtraDrive::Overlaybd {
        drive_id: data.id.clone(),
        image_config_path: image,
        read_only: false,
        mount_path: "/mnt/data".into(),
        virtual_size: Some(1024 * 1024 * 1024),
        sub_path: None,
        snapshot_output_dir: volume.then_some(output.clone()),
        volume,
    };
    let mut source_spec = spec(&assets)?;
    source_spec.context.user = Some("root".into());
    source_spec.extra_drives.push(drive);
    let ro = if volume {
        let record = manager
            .create("db-m1-ro".into(), VolumeMode::ReadOnly, None, None, 1024)
            .await?;
        source_spec.extra_drives.push(ExtraDrive::Overlaybd {
            drive_id: record.id.clone(),
            image_config_path: record.backing_image_config.clone().context("ro backing")?,
            read_only: true,
            mount_path: "/mnt/ro".into(),
            virtual_size: Some(1024 * 1024 * 1024),
            sub_path: None,
            snapshot_output_dir: None,
            volume: true,
        });
        Some(record)
    } else {
        None
    };
    let source_launch = launch(&assets)?;
    let source_id = source_launch.sandbox_id;
    let source_token = source_launch.envd_access_token.clone();
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(source_spec, source_launch)?;
    source.start().await?;
    let initial = source.run_command("/bin/sh", &["-ec",
        "printf seed >/var/tmp/db-m1-fork; printf seed >/dev/shm/db-m1-fork; printf seed >/mnt/data/db-m1-fork; sync"
    ]).await?;
    assert_eq!(initial.exit_code, 0, "{}", initial.stderr);
    let mut child_specs = vec![fork_spec()?, fork_spec()?];
    if volume {
        source.snapshot_volumes().await?;
        for (index, child) in child_specs.iter_mut().enumerate() {
            let record = manager
                .create(
                    format!("db-m1-child-{index}"),
                    VolumeMode::Exclusive,
                    None,
                    Some(output.join("image.json")),
                    1024,
                )
                .await?;
            manager
                .reserve(&record.id, &child.sandbox_id.to_string())
                .await?;
            child
                .replace_drive_ids
                .push((data.id.clone(), record.id.clone()));
            child.extra_drives.push(ExtraDrive::Overlaybd {
                drive_id: record.id,
                image_config_path: record.backing_image_config.context("child backing")?,
                read_only: false,
                mount_path: "/mnt/data".into(),
                virtual_size: Some(1024 * 1024 * 1024),
                sub_path: None,
                snapshot_output_dir: None,
                volume: true,
            });
            let ro = ro.as_ref().unwrap();
            manager
                .reserve(&ro.id, &child.sandbox_id.to_string())
                .await?;
            child.extra_drives.push(ExtraDrive::Overlaybd {
                drive_id: ro.id.clone(),
                image_config_path: ro.backing_image_config.clone().context("ro backing")?,
                read_only: true,
                mount_path: "/mnt/ro".into(),
                virtual_size: Some(1024 * 1024 * 1024),
                sub_path: None,
                snapshot_output_dir: None,
                volume: true,
            });
        }
        manager.reserve(&data.id, &source_id.to_string()).await?;
        assert!(
            manager.delete(&data.id).await.is_err(),
            "mounted exclusive volume deleted"
        );
        assert_eq!(
            manager
                .get(&ro.as_ref().unwrap().id)
                .await?
                .read_only_mounts
                .len(),
            2
        );
    }
    if invalid_child {
        let mut bad = fork_spec()?;
        if volume {
            let record = manager
                .create(
                    "db-m1-bad".into(),
                    VolumeMode::Exclusive,
                    None,
                    Some(output.join("image.json")),
                    1024,
                )
                .await?;
            manager
                .reserve(&record.id, &bad.sandbox_id.to_string())
                .await?;
            bad.replace_drive_ids
                .push((data.id.clone(), record.id.clone()));
            bad.extra_drives.push(ExtraDrive::Overlaybd {
                drive_id: record.id,
                image_config_path: root.path().join("deliberately-missing-image.json"),
                read_only: false,
                mount_path: "/mnt/data".into(),
                virtual_size: Some(1024 * 1024 * 1024),
                sub_path: None,
                snapshot_output_dir: None,
                volume: true,
            });
        } else {
            bad.replace_drive_ids
                .push(("not-a-captured-drive".into(), "not-a-child-drive".into()));
        }
        child_specs.insert(1, bad);
    }
    // Child catalog allocation precedes the actual fork capture. Writes in this
    // interval must be included in both RAM and the child's volume generation.
    if volume {
        let output = source.run_command("/bin/sh", &["-ec",
            "printf cut >/var/tmp/db-m1-late-cut; printf cut >/dev/shm/db-m1-late-cut; printf cut >/mnt/data/db-m1-late-cut; sync"
        ]).await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
    }
    // Observe immutable capture roots without replacing the authenticated runner
    // socket. A fork creates one root even when one child later fails.
    use std::os::fd::{AsRawFd, FromRawFd};
    let captures = app
        .firecracker
        .work_dir
        .clone()
        .context("fixture work dir")?
        .join("managed-snapshots/dragonball");
    std::fs::create_dir_all(&captures)?;
    let watch_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    anyhow::ensure!(
        watch_fd >= 0,
        "capture watcher: {}",
        std::io::Error::last_os_error()
    );
    let watcher = unsafe { std::fs::File::from_raw_fd(watch_fd) };
    let capture_path = std::ffi::CString::new(captures.as_os_str().as_encoded_bytes())?;
    anyhow::ensure!(
        unsafe {
            libc::inotify_add_watch(
                watcher.as_raw_fd(),
                capture_path.as_ptr(),
                libc::IN_CREATE | libc::IN_ONLYDIR,
            )
        } >= 0,
        "watch capture roots"
    );
    let result = source.fork(&child_specs).await;
    if result.is_err() {
        source.stop().await?;
    }
    let mut children = result?;
    assert_eq!(children.len(), child_specs.len());
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    let mut events = [0u8; 4096];
    let length = unsafe {
        libc::read(
            watcher.as_raw_fd(),
            events.as_mut_ptr().cast(),
            events.len(),
        )
    };
    anyhow::ensure!(length > 0, "no capture-root creation observed");
    let mut offset = 0;
    let mut created = 0;
    while offset < length as usize {
        let event = unsafe {
            std::ptr::read_unaligned(events.as_ptr().add(offset).cast::<libc::inotify_event>())
        };
        assert_eq!(
            event.mask & libc::IN_Q_OVERFLOW,
            0,
            "capture watcher overflow"
        );
        if event.mask & (libc::IN_CREATE | libc::IN_ISDIR) == libc::IN_CREATE | libc::IN_ISDIR {
            created += 1;
        }
        offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
    }
    assert_eq!(created, 1, "fork must take exactly one immutable capture");
    let mut good = vec![];
    for (index, child) in children.drain(..).enumerate() {
        if invalid_child && index == 1 {
            assert!(child.is_err(), "invalid child unexpectedly started");
            if volume {
                let child_spec = &child_specs[index];
                let ids = child_spec
                    .extra_drives
                    .iter()
                    .map(|drive| drive.drive_id().to_owned())
                    .collect::<Vec<_>>();
                manager
                    .replace_owner_for(&child_spec.sandbox_id.to_string(), None, &ids)
                    .await?;
                for id in ids {
                    manager.delete(&id).await?;
                    assert!(
                        manager.get(&id).await.is_err(),
                        "failed child allocation remained"
                    );
                }
            }
        } else {
            good.push((index, child?));
        }
    }
    let result: Result<()> = async {
        for (ordinal, (index, child)) in good.iter().enumerate() {
            let request = &child_specs[*index];
            let value = format!("child-{ordinal}");
            if volume {
                let cut = fork_command(child.as_ref(), request.envd_access_token.as_ref(),
                    "echo 3 >/proc/sys/vm/drop_caches; cat /var/tmp/db-m1-late-cut /dev/shm/db-m1-late-cut /mnt/data/db-m1-late-cut"
                ).await?;
                assert_eq!(cut.exit_code, 0, "fork volume predates captured RAM: {}", cut.stderr);
                assert_eq!(cut.stdout, "cutcutcut", "fork volume/RAM cut mismatch");
            }
            let output = fork_command(child.as_ref(), request.envd_access_token.as_ref(),
                &format!("test \"$(cat /var/tmp/db-m1-fork)\" = seed; test \"$(cat /dev/shm/db-m1-fork)\" = seed; test \"$(cat /mnt/data/db-m1-fork)\" = seed; test \"$E2B_SANDBOX_ID\" = {}; printf {value} >/var/tmp/db-m1-fork; printf {value} >/dev/shm/db-m1-fork; printf {value} >/mnt/data/db-m1-fork; sync", request.sandbox_id)
            ).await?;
            assert_eq!(output.exit_code, 0, "child {ordinal}: {}", output.stderr);
            if volume {
                let output = fork_command(child.as_ref(), request.envd_access_token.as_ref(),
                    "! touch /mnt/ro/forbidden; test -z \"$(ls -A /mnt/ro | sed /lost+found/d)\"").await?;
                assert_eq!(output.exit_code, 0, "RO volume writable: {}", output.stderr);
            }
            let stale = fork_command(child.as_ref(), source_token.as_ref(), "true").await;
            assert!(stale.is_err(), "parent credential accepted by child");
            // This envd returns JSON HTTP401 on gRPC routes; tonic reports a framing
            // error. Check native HTTP auth instead of assuming a gRPC status code.
            let endpoint = format!("http://{}:49983/files", child.host_interaction_ip().unwrap());
            let http = envd::reqwest::Client::new();
            let rejected = http.get(&endpoint)
                .query(&[("path", "/var/tmp/db-m1-fork")])
                .header("x-access-token", source_token.as_ref().unwrap().expose())
                .send().await?;
            assert_eq!(
                rejected.status(), envd::reqwest::StatusCode::UNAUTHORIZED,
                "parent credential was not refused by HTTP authentication"
            );
            let accepted = http.get(&endpoint)
                .query(&[("path", "/var/tmp/db-m1-fork")])
                .header("x-access-token", request.envd_access_token.as_ref().unwrap().expose())
                .send().await?;
            assert_eq!(accepted.status(), envd::reqwest::StatusCode::OK);
        }
        let output = source.run_command("/bin/sh", &["-ec",
            "test \"$(cat /var/tmp/db-m1-fork)\" = seed; test \"$(cat /dev/shm/db-m1-fork)\" = seed; test \"$(cat /mnt/data/db-m1-fork)\" = seed; printf parent >/var/tmp/db-m1-fork; printf parent >/dev/shm/db-m1-fork; printf parent >/mnt/data/db-m1-fork; sync"
        ]).await?;
        assert_eq!(output.exit_code, 0, "source: {}", output.stderr);
        for (ordinal, (index, child)) in good.iter().enumerate() {
            let output = fork_command(child.as_ref(), child_specs[*index].envd_access_token.as_ref(),
                &format!("cat /var/tmp/db-m1-fork /dev/shm/db-m1-fork /mnt/data/db-m1-fork; test \"$(cat /mnt/data/db-m1-fork)\" = child-{ordinal}")
            ).await?;
            assert_eq!(output.exit_code, 0, "sibling contaminated: {}", output.stderr);
            assert_eq!(output.stdout, format!("child-{ordinal}child-{ordinal}child-{ordinal}"));
        }
        if volume {
            source.stop().await?;
            manager.replace_owner_for(&source_id.to_string(), None, std::slice::from_ref(&data.id)).await?;
            manager.delete(&data.id).await?;
            assert!(manager.get(&data.id).await.is_err());
            let mut cold_spec = spec(&assets)?;
            cold_spec.context.user = Some("root".into());
            cold_spec.extra_drives.push(child_specs[0].extra_drives[0].clone());
            let mut restored_volume = factory.build_fresh(cold_spec, launch(&assets)?)?;
            restored_volume.start().await?;
            let restored = restored_volume.run_command("/bin/sh", &["-ec",
                "test \"$(cat /mnt/data/db-m1-fork)\" = seed && test \"$(cat /mnt/data/db-m1-late-cut)\" = cut && printf restored >/mnt/data/db-m1-fork && sync"
            ]).await?;
            restored_volume.stop().await?;
            assert_eq!(restored.exit_code, 0, "source deletion invalidated published volume: {}", restored.stderr);
            let output = fork_command(good[0].1.as_ref(), child_specs[0].envd_access_token.as_ref(),
                "cat /mnt/data/db-m1-fork").await?;
            assert_eq!(output.stdout, "child-0", "cold volume restore shared the live child's upper");
        }
        Ok(())
    }.await;
    for (_, child) in &mut good {
        child.stop().await?;
    }
    source.stop().await?;
    result
}

#[tokio::test]
#[ignore = "requires real forked guests in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_fork_has_independent_writable_disks() -> Result<()> {
    fork_isolation(false, false).await
}

#[tokio::test]
#[ignore = "requires real forked guests in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_failed_child_keeps_source_and_sibling_running() -> Result<()> {
    fork_isolation(true, false).await?;
    fork_isolation(true, true).await
}

#[tokio::test]
#[ignore = "requires real volume-backed forked guests in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_exclusive_volumes_fork_and_ro_share() -> Result<()> {
    fork_isolation(false, true).await
}

#[tokio::test]
#[ignore = "requires real volume-backed pause and cold reopen in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_pause_preserves_dirty_volume_after_ram_is_deleted() -> Result<()> {
    use agentenv::volume::{VolumeManager, VolumeMode};

    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, _, repository) = crate::common::snapshot_test_parts(root.path());
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let volume = volumes
        .create(
            "pause-dirty-volume".into(),
            VolumeMode::Exclusive,
            None,
            None,
            128,
        )
        .await?;
    let config = volume.backing_image_config.context("volume backing")?;
    let mut build = spec(&assets)?;
    build.context.user = Some("root".into());
    build.extra_drives.push(ExtraDrive::Overlaybd {
        drive_id: volume.id.clone(),
        snapshot_output_dir: Some(config.parent().context("backing parent")?.to_path_buf()),
        image_config_path: config,
        read_only: false,
        mount_path: "/mnt/data".into(),
        virtual_size: Some(128 << 20),
        sub_path: None,
        volume: true,
    });
    let source_launch = launch(&assets)?;
    let owner = source_launch.sandbox_id.to_string();
    volumes.reserve(&volume.id, &owner).await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(build.clone(), source_launch)?;
    source.start().await?;
    let result: Result<()> = async {
        // Keep the overwrite dirty until pause explicitly syncs it. Reuse the
        // allocated file so the oracle is its bytes, not journal replay timing.
        let output = source
            .run_command(
                "/bin/sh",
                &[
                    "-ec",
                    r#"
            mount -o remount,commit=600 /mnt/data
            echo 0 >/proc/sys/vm/dirty_writeback_centisecs
            echo 60000 >/proc/sys/vm/dirty_expire_centisecs
            printf before >/mnt/data/marker
            sync
            printf 'after!' | dd of=/mnt/data/marker conv=notrunc status=none
            test "$(cat /mnt/data/marker)" = 'after!'
        "#,
                ],
            )
            .await?;
        assert_eq!(output.exit_code, 0, "dirty volume setup: {}", output.stderr);
        let paused = source.pause(None).await?;
        // Paused deletion publishes the captured backing without another guest
        // sync, then discards RAM. A cold guest must see the completed write.
        volumes
            .publish_backings(&owner, std::slice::from_ref(&volume.id))
            .await?;
        source.stop().await?;
        drop(paused);
        volumes
            .replace_owner_for(&owner, None, std::slice::from_ref(&volume.id))
            .await?;
        let mut reopened = factory.build_fresh(build, launch(&assets)?)?;
        reopened.start().await?;
        let output = reopened
            .run_command("/bin/sh", &["-ec", "cat /mnt/data/marker"])
            .await;
        reopened.stop().await?;
        let output = output?;
        assert_eq!(output.exit_code, 0, "cold volume read: {}", output.stderr);
        assert_eq!(
            output.stdout, "after!",
            "pause lost dirty guest volume pages when RAM was deleted"
        );
        Ok(())
    }
    .await;
    source.stop().await?;
    result
}

#[tokio::test]
#[ignore = "requires real guest volume sync refusal in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_pause_volume_sync_failure_preserves_running() -> Result<()> {
    use agentenv::volume::{VolumeManager, VolumeMode};

    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, _, repository) = crate::common::snapshot_test_parts(root.path());
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let volume = volumes
        .create(
            "pause-sync-refusal".into(),
            VolumeMode::Exclusive,
            None,
            None,
            128,
        )
        .await?;
    let config = volume.backing_image_config.context("volume backing")?;
    let mut build = spec(&assets)?;
    build.context.user = Some("root".into());
    build.extra_drives.push(ExtraDrive::Overlaybd {
        drive_id: volume.id,
        snapshot_output_dir: Some(config.parent().context("backing parent")?.to_path_buf()),
        image_config_path: config,
        read_only: false,
        mount_path: "/mnt/data".into(),
        virtual_size: Some(128 << 20),
        sub_path: None,
        volume: true,
    });
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(build, launch(&assets)?)?;
    source.start().await?;
    let result: Result<()> = async {
        // Refuse only sync in the real guest; all other maintenance applets
        // retain their actual behavior and the runner remains unmodified.
        let output = source
            .run_command(
                "/bin/sh",
                &[
                    "-ec",
                    r#"
            mkdir -p /tmp/db-m1-real
            cp /agentenv/bin/busybox /tmp/db-m1-real/busybox
            cat >/tmp/db-m1-refuse-sync <<'EOF'
#!/bin/sh
if [ "$1" = sync ]; then
    echo deliberate-volume-sync-refusal >&2
    exit 42
fi
exec /tmp/db-m1-real/busybox "$@"
EOF
            chmod 755 /tmp/db-m1-refuse-sync
            mount --bind /tmp/db-m1-refuse-sync /agentenv/bin/busybox
            printf before >/mnt/data/marker
        "#,
                ],
            )
            .await?;
        assert_eq!(output.exit_code, 0, "sync refusal setup: {}", output.stderr);
        let error = source
            .pause(None)
            .await
            .err()
            .context("pause succeeded despite guest volume sync refusal")?;
        assert!(
            !error.is_terminal(),
            "known sync refusal must remain recoverable: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("deliberate-volume-sync-refusal"),
            "{error:#}"
        );
        assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
        let output = source
            .run_command(
                "/bin/sh",
                &[
                    "-ec",
                    r#"
            test "$(cat /mnt/data/marker)" = before
            printf recovered >/mnt/data/marker
            umount /agentenv/bin/busybox
        "#,
                ],
            )
            .await?;
        assert_eq!(
            output.exit_code, 0,
            "source unusable after sync refusal: {}",
            output.stderr
        );
        let paused = source.pause(None).await?;
        source.resume().await?;
        drop(paused);
        let output = source
            .run_command("/bin/sh", &["-ec", "cat /mnt/data/marker"])
            .await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "recovered");
        Ok(())
    }
    .await;
    source.stop().await?;
    result
}

#[tokio::test]
#[ignore = "requires real volume freeze/thaw in isolated KVM/ublk fixture"]
async fn dragonball_lifecycle_volume_freeze_failure_thaws_prior_mount() -> Result<()> {
    use agentenv::volume::{VolumeManager, VolumeMode};
    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, _, repository) = crate::common::snapshot_test_parts(root.path());
    let manager =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let mut source_spec = spec(&assets)?;
    source_spec.context.user = Some("root".into());
    for (index, mount) in ["/data", "/archive"].into_iter().enumerate() {
        let record = manager
            .create(
                format!("freeze-{index}"),
                VolumeMode::Exclusive,
                None,
                None,
                1024,
            )
            .await?;
        let config = record.backing_image_config.context("volume backing")?;
        source_spec.extra_drives.push(ExtraDrive::Overlaybd {
            drive_id: record.id,
            snapshot_output_dir: Some(config.parent().unwrap().to_path_buf()),
            image_config_path: config,
            mount_path: mount.into(),
            read_only: false,
            virtual_size: Some(1024 * 1024 * 1024),
            sub_path: None,
            volume: true,
        });
    }
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(source_spec, launch(&assets)?)?;
    source.start().await?;
    let result: Result<()> = async {
        assert_eq!(source.run_command("/bin/sh", &["-ec", "printf first >/data/marker; printf first >/archive/marker; sync; umount /archive"]).await?.exit_code, 0);
        let error = source.freeze_and_snapshot_volumes().await.unwrap_err();
        assert!(!error.is_terminal(), "known freeze refusal must thaw earlier mount: {error:#}");
        let output = source.run_command("/bin/sh", &["-ec", "printf thawed >/data/marker; mount /dev/vdd /archive; sync"]).await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        source.freeze_and_snapshot_volumes().await?;
        assert_eq!(source.run_command("/bin/sh", &["-ec", "mount --bind /archive /data"]).await?.exit_code, 0);
        assert!(source.thaw_volumes().await.is_err(), "changed filesystem must not be thawed");
        assert_eq!(source.run_command("/bin/sh", &["-ec", "umount /data"]).await?.exit_code, 0);
        source.thaw_volumes().await?;
        source.thaw_volumes().await?;
        let output = source.run_command("/bin/sh", &["-ec", "printf again >/data/marker; printf again >/archive/marker; sync"]).await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        Ok(())
    }.await;
    source.stop().await?;
    result
}

#[tokio::test]
#[ignore = "requires real concurrent child startup and cancellation in disposable namespace"]
async fn dragonball_lifecycle_fork_cancellation_reaps_unclaimed_children() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut app, assets) = fixture().await?;
    let prior_works = std::fs::read_dir(&app.runtime_path)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect::<std::collections::HashSet<_>>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    app.custom_extension.url = Some(format!("http://{}", listener.local_addr()?));
    app.custom_extension.timeout_ms = 30000;
    let (reached_tx, mut reached_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => return Err::<(), anyhow::Error>(error.into()),
            };
            let reached = reached_tx.clone();
            let mut release = release_rx.clone();
            tokio::spawn(async move {
                let mut bytes = vec![];
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await?);
                    anyhow::ensure!(bytes.len() < 8192, "oversized hook header");
                }
                let header = std::str::from_utf8(&bytes)?;
                let resume = header
                    .lines()
                    .next()
                    .context("hook request")?
                    .contains("/start-resume");
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().to_owned())
                    })
                    .context("hook content length")?
                    .parse()?;
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await?;
                if resume {
                    reached.send(
                        serde_json::from_slice::<Value>(&body)?["sandboxId"]
                            .as_str()
                            .context("child ID")?
                            .to_owned(),
                    )?;
                    release.wait_for(|released| *released).await?;
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
                Ok::<_, anyhow::Error>(())
            });
        }
    });
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let source_work = source.work_dir()?.to_path_buf();
    let specs = vec![fork_spec()?, fork_spec()?];
    let mut future = Box::pin(source.fork(&specs));
    let result: Result<()> = async {
        let mut ids = vec![];
        for _ in 0..2 {
            let id = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::select! {
                    result = &mut future => anyhow::bail!("fork ended before both child barriers: {}", result.is_ok()),
                    id = reached_rx.recv() => id.context("child barrier disappeared"),
                }
            }).await??;
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1], "child startup was not independent/concurrent");
        Ok(())
    }.await;
    drop(future);
    if let Err(error) = result {
        let _ = release_tx.send(true);
        source.stop().await?;
        server.abort();
        return Err(error);
    }
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    release_tx.send(true)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let works = std::fs::read_dir(&app.runtime_path)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("db-runtime-")
                })
                .map(|entry| entry.path())
                .filter(|path| !prior_works.contains(path))
                .collect::<Vec<_>>();
            if works == [source_work.clone()] {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(
        source
            .run_command("/bin/sh", &["-ec", "true"])
            .await?
            .exit_code,
        0
    );
    source.stop().await?;
    server.abort();
    Ok(())
}

async fn disk_counter(path: &std::path::Path) -> Result<u64> {
    // e2fsprogs opens read-only without -w. Direct reads avoid a cached view.
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("debugfs")
            .args(["-D", "-R", "cat /var/tmp/db-m1-counter"])
            .arg(path)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "debugfs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::str::from_utf8(&output.stdout)?
        .trim()
        .parse()
        .context("counter must exist on the actual user disk")
}

#[tokio::test]
#[ignore = "requires real captured guest and disposable KVM/ublk/network fixture"]
async fn dragonball_lifecycle_load_held_does_not_run_before_identity() -> Result<()> {
    let (app, assets) = fixture().await?;
    let mut limited = app.clone();
    limited.machine.disk_rate_limit.enabled = true;
    limited.machine.disk_rate_limit.bandwidth_bytes_per_sec = 128 * 1024 * 1024;
    limited.machine.disk_rate_limit.iops = 10000;
    let policy = DragonballRuntimePolicy::from_app_config(
        &limited,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let token = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let configured: Value = serde_json::from_slice(&std::fs::read(
        source.work_dir()?.join("runtime-config.json"),
    )?)?;
    assert_eq!(
        configured["block_devices"][1]["rate_limiter"]["ops"]["size"],
        10000
    );
    let initial = source.run_command("/bin/sh", &["-ec", "findmnt -T /var/tmp -n -o SOURCE; printf '%020d\\n' 0 > /var/tmp/db-m1-counter; sync; nohup /bin/sh -c 'n=0; while :; do n=$((n+1)); printf \"%020d\\n\" \"$n\" | dd of=/var/tmp/db-m1-counter bs=21 count=1 conv=notrunc,fdatasync status=none; done' >/tmp/db-m1-counter.log 2>&1 </dev/null &"]).await?;
    assert_eq!(initial.exit_code, 0, "{}", initial.stderr);
    assert!(
        initial.stdout.contains("vdb"),
        "counter is not on the user disk: {}",
        initial.stdout
    );
    let paused = source.pause(None).await?;
    assert!(source.executor().is_err(), "held source exposed executor");
    assert!(
        source.pause(None).await.unwrap_err().is_terminal(),
        "held source must not return Recoverable while it is not Running"
    );
    source.stop().await?;
    let current = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let current = DragonballSandboxFactory::new(current)?;
    let mut restored = current.build_paused(id, paused.as_ref(), token)?;
    restored.start_nowait().await?;
    let status = restored.runtime_status().await?;
    assert_eq!(status.phase, RuntimePhase::LoadedHeld);
    assert!(status.capture.as_ref().is_some_and(|proof| proof
        .worker_acks
        .iter()
        .all(|ack| ack.pending_io == 0 && ack.memory_writers == 0)));
    assert!(restored.executor().is_err(), "held target exposed executor");
    let machine: Value = serde_json::from_slice(&std::fs::read(
        restored.work_dir()?.join("runtime-config.json"),
    )?)?;
    assert!(
        machine["block_devices"][1].get("rate_limiter").is_none(),
        "disabled policy inherited old limits"
    );
    let path: PathBuf = machine["block_devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|disk| disk["drive_id"] == "user_rootfs")
        .unwrap()["path_on_host"]
        .as_str()
        .unwrap()
        .into();
    let before = disk_counter(&path).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        disk_counter(&path).await?,
        before,
        "held guest performed disk writes before explicit release"
    );
    assert_eq!(
        restored.runtime_status().await?.phase,
        RuntimePhase::LoadedHeld
    );
    restored.wait_for_ready().await?;
    let first = restored
        .run_command(
            "/bin/sh",
            &[
                "-ec",
                "cat /var/tmp/db-m1-counter; printf '%s\\n' \"$E2B_SANDBOX_ID\"",
            ],
        )
        .await?;
    let mut progressed = false;
    for _ in 0..100 {
        let next = restored
            .run_command("/bin/sh", &["-ec", "cat /var/tmp/db-m1-counter"])
            .await?;
        if next.stdout.lines().next() != first.stdout.lines().next() && !next.stdout.is_empty() {
            progressed = true;
            break;
        }
    }
    restored.stop().await?;
    assert!(first.stdout.contains(&id.to_string()), "{}", first.stdout);
    assert!(progressed, "counter did not resume after held release");
    Ok(())
}

async fn ram_progress(
    sandbox: &agentenv::sandbox::dragonball::DragonballSandbox,
    marker: &str,
) -> Result<u64> {
    let output = sandbox
        .run_command("/tmp/db-m1-active-io", &["read", marker])
        .await?;
    anyhow::ensure!(
        output.exit_code == 0,
        "RAM workload reader failed: {}",
        output.stderr
    );
    output
        .stdout
        .split_whitespace()
        .last()
        .context("RAM sequence")?
        .parse()
        .context("RAM sequence integer")
}

async fn fault_fixture() -> Result<(AppConfig, Value)> {
    let (app, mut assets) = fixture().await?;
    let runner = std::env::var("AENV_DRAGONBALL_FAULT_RUNNER")
        .context("build runner example reply-fault and set AENV_DRAGONBALL_FAULT_RUNNER")?;
    anyhow::ensure!(
        std::path::Path::new(&runner).is_absolute(),
        "absolute fault runner path"
    );
    assets["runner_path"] = Value::String(runner);
    Ok((app, assets))
}

struct RuntimeReplyFault {
    task: tokio::task::JoinHandle<()>,
    evidence: tempfile::TempDir,
}

impl RuntimeReplyFault {
    fn operations(&self) -> Result<Vec<String>> {
        Ok(std::fs::read_to_string(self.evidence.path().join("ops"))?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

impl Drop for RuntimeReplyFault {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn install_reply_fault(
    work: &std::path::Path,
    drop_reply: &'static str,
    barrier: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<RuntimeReplyFault> {
    let evidence = tempfile::tempdir()?;
    let path = evidence.path().to_owned();
    std::fs::write(
        work.join("reply-fault.json"),
        serde_json::to_vec(&serde_json::json!({
            "drop_reply": drop_reply,
            "barrier": barrier.is_some(),
            "evidence": path,
        }))?,
    )?;
    let task = tokio::spawn(async move {
        if let Some((reached, release)) = barrier {
            while !path.join("reached").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = reached.send(());
            let _ = release.await;
            let _ = std::fs::write(path.join("release"), b"continue");
        }
    });
    Ok(RuntimeReplyFault { task, evidence })
}

#[tokio::test]
#[ignore = "real runner completes Export but response is lost; source must actually recover"]
async fn dragonball_lifecycle_capture_export_reply_loss_recovers_running() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let proxy = install_reply_fault(source.work_dir()?, "export_held_state", None)?;
    let output = tempfile::tempdir()?;
    let error = source.capture_to_dir(output.path()).await.unwrap_err();
    assert!(!error.is_terminal(), "{error:#}");
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert_eq!(source.run_command("/bin/true", &[]).await?.exit_code, 0);
    let ops = proxy.operations()?;
    assert_eq!(
        ops.iter().filter(|op| *op == "export_held_state").count(),
        1,
        "unknown export was replayed"
    );
    assert!(ops.iter().any(|op| op == "end_capture"), "{ops:?}");
    assert!(!output.path().join("vm-state.json").exists());
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real runner holds but BeginCapture response is lost; Terminal requires teardown"]
async fn dragonball_lifecycle_capture_begin_reply_loss_tears_down() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let work = source.work_dir()?.to_owned();
    let proxy = install_reply_fault(&work, "begin_capture", None)?;
    let output = tempfile::tempdir()?;
    let error = source.capture_to_dir(output.path()).await.unwrap_err();
    assert!(error.is_terminal(), "{error:#}");
    assert!(source.executor().is_err());
    assert!(
        !work.exists(),
        "terminal capture returned with unreaped runner resources"
    );
    let ops = proxy.operations()?;
    assert_eq!(ops.iter().filter(|op| *op == "begin_capture").count(), 1);
    assert!(ops.iter().any(|op| op == "stop"), "{ops:?}");
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "actual production Export deadline expires after real export, same-token release only"]
async fn dragonball_lifecycle_capture_export_timeout_recovers_running() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let proxy = install_reply_fault(source.work_dir()?, "delay_export_held_state", None)?;
    let output = tempfile::tempdir()?;
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(80),
        source.capture_to_dir(output.path()),
    )
    .await?
    .unwrap_err();
    assert!(!error.is_terminal(), "{error:#}");
    assert!(
        format!("{error:#}").contains("deadline exceeded"),
        "{error:#}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(60),
        "did not exercise actual production deadline"
    );
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert_eq!(source.run_command("/bin/true", &[]).await?.exit_code, 0);
    let ops = proxy.operations()?;
    assert_eq!(
        ops.iter().filter(|op| *op == "export_held_state").count(),
        1
    );
    assert!(ops.iter().any(|op| op == "end_capture"));
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "actual BeginCapture deadline expires; unknown ack requires confirmed process reap"]
async fn dragonball_lifecycle_capture_begin_timeout_tears_down() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let work = source.work_dir()?.to_owned();
    let proxy = install_reply_fault(&work, "delay_begin_capture", None)?;
    let output = tempfile::tempdir()?;
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(50),
        source.capture_to_dir(output.path()),
    )
    .await?
    .unwrap_err();
    assert!(error.is_terminal(), "{error:#}");
    assert!(
        format!("{error:#}").contains("deadline exceeded"),
        "{error:#}"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert!(
        !work.exists(),
        "unknown ack returned Terminal without process reap"
    );
    assert!(source.executor().is_err());
    let ops = proxy.operations()?;
    assert_eq!(ops.iter().filter(|op| *op == "begin_capture").count(), 1);
    assert!(ops.iter().any(|op| op == "stop"));
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real Export barrier cancels caller while owned capture retains live leases"]
async fn dragonball_lifecycle_capture_cancellation_keeps_owner_until_reap() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let work = source.work_dir()?.to_owned();
    let (reached, reached_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let proxy = install_reply_fault(&work, "never", Some((reached, release_rx)))?;
    let mut capture = Box::pin(source.snapshot());
    tokio::select! {
        result = &mut capture => anyhow::bail!("capture escaped actual export barrier: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), reached_rx) => { result??; }
    }
    drop(capture);
    assert!(
        work.exists(),
        "caller cancellation released runner/disk directory before owner"
    );
    assert!(proxy
        .operations()?
        .iter()
        .any(|op| op == "export_held_state"));
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("owned capture already abandoned"))?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while work.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert!(source.executor().is_err());
    assert_eq!(
        proxy
            .operations()?
            .iter()
            .filter(|op| *op == "export_held_state")
            .count(),
        1
    );
    source.stop().await?;
    Ok(())
}

async fn held_io_cut(
    target: &mut agentenv::sandbox::dragonball::DragonballSandbox,
    marker: &str,
) -> Result<(u64, u64)> {
    use std::io::Read;
    assert_eq!(
        target.runtime_status().await?.phase,
        RuntimePhase::LoadedHeld
    );
    let envelope = agentenv::sandbox::dragonball::snapshot::read_envelope(
        &target.work_dir()?.join("restore-state.json"),
    )?;
    let memory_path = target.work_dir()?.join("restore-memory.bin");
    assert_eq!(
        std::fs::metadata(&memory_path)?.len(),
        envelope.memory_layout.image_size
    );
    let mut memory = std::fs::File::open(memory_path)?;
    let mut chunk = vec![0u8; 1024 * 1024];
    let mut ram = vec![];
    let mut offset = 0u64;
    while offset < envelope.memory_layout.image_size {
        let length =
            usize::try_from((envelope.memory_layout.image_size - offset).min(chunk.len() as u64))?;
        memory.read_exact(&mut chunk[..length])?;
        for (index, page) in chunk[..length].as_chunks::<4096>().0.iter().enumerate() {
            if page.starts_with(marker.as_bytes()) && page[47] == 0 {
                let issued = u64::from_le_bytes(page[48..56].try_into()?);
                let completed = u64::from_le_bytes(page[56..64].try_into()?);
                if completed > 10 && issued >= completed && issued - completed <= 1 {
                    let image_offset = offset + index as u64 * 4096;
                    let region = envelope
                        .memory_layout
                        .regions
                        .iter()
                        .find(|region| {
                            image_offset >= region.image_offset
                                && image_offset < region.image_offset + region.length
                        })
                        .context("RAM marker outside declared layout")?;
                    let gpa = region.guest_base + image_offset - region.image_offset;
                    ram.push((
                        issued,
                        completed,
                        gpa,
                        u64::from_le_bytes(page[64..72].try_into()?),
                        u64::from_le_bytes(page[72..80].try_into()?),
                    ));
                }
            }
        }
        offset += length as u64;
    }
    assert_eq!(
        ram.len(),
        1,
        "expected one actual shared physical RAM page, got {ram:?}"
    );
    let (issued, completed, gpa, received, transmitted) = ram[0];
    if envelope.machine.memory_mib > 3072 {
        assert_eq!(envelope.memory_layout.regions.len(), 2);
        assert_eq!(envelope.memory_layout.regions[1].guest_base, 1 << 32);
        assert_eq!(envelope.memory_layout.regions[1].image_offset, 3 << 30);
        assert!(
            gpa >= 1 << 32,
            "no actual active RAM page above the 4GiB GPA hole: {gpa:#x}"
        );
    }
    let machine: agentenv_vmm_protocol::runtime::RuntimeVmConfig = serde_json::from_slice(
        &std::fs::read(target.work_dir()?.join("runtime-config.json"))?,
    )?;
    let device = machine
        .block_devices
        .iter()
        .find(|disk| disk.drive_id == "user_rootfs")
        .context("user disk")?;
    let disk = tokio::process::Command::new("debugfs")
        .args(["-D", "-R", "cat /var/tmp/db-m1-cut-block"])
        .arg(&device.path_on_host)
        .output()
        .await?;
    assert!(
        disk.status.success(),
        "{}",
        String::from_utf8_lossy(&disk.stderr)
    );
    assert_eq!(disk.stdout.len(), 4096, "missing/torn persistent block");
    let sequence = u64::from_le_bytes(disk.stdout[..8].try_into()?);
    for (i, word) in disk.stdout.as_chunks::<8>().0.iter().enumerate() {
        assert_eq!(
            u64::from_le_bytes(*word),
            sequence ^ (i as u64).wrapping_mul(0x9e3779b97f4a7c15),
            "torn word {i}"
        );
    }
    assert!(
        sequence >= completed && sequence <= issued,
        "disk{sequence} outside RAM issued/completed {issued}/{completed}"
    );
    println!("held full RAM {}MiB GPA={gpa:#x} issued={issued} completed={completed} disk={sequence} RX={received} TX={transmitted}", envelope.machine.memory_mib);
    Ok((completed, received.min(transmitted)))
}

#[tokio::test]
#[ignore = "real Linux KVM/ublk active-I/O capture and held restore"]
async fn dragonball_lifecycle_capture_under_active_io_is_one_cut() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let launch = launch(&assets)?;
    let (id, token) = (launch.sandbox_id, launch.envd_access_token.clone());
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    let paused = source.pause(None).await?;
    source.stop().await?;
    let mut target = factory.build_paused(id, paused.as_ref(), token)?;
    target.start_nowait().await?;
    let (completed, _) = held_io_cut(&mut target, &marker).await?;
    target.wait_for_ready().await?;
    assert!(
        ram_progress(&target, &marker).await? > completed,
        "RAM writer did not resume"
    );
    target.stop().await?;
    Ok(())
}

async fn active_io_memory_case(memory_mib: u32) -> Result<()> {
    let (mut app, assets) = fixture().await?;
    // Keep full ordinary-file RAM copies off a small /tmp tmpfs. This directory
    // belongs only to this case; the node's configured state and assets are unchanged.
    let runtime = tempfile::Builder::new()
        .prefix("db-m1-memory-")
        .tempdir_in("/var/tmp")?;
    // Preserve evidence and possibly live leases on failure; only remove this
    // exact task-owned root after all runners have positively stopped.
    let runtime_path = runtime.keep();
    println!("large-memory runtime evidence: {}", runtime_path.display());
    app.runtime_path = runtime_path.clone();
    let scratch_mount = if memory_mib > 3072 {
        // Use private tmpfs to keep full-file export within the 60s budget
        // independently of host block-storage throughput. The 32GiB scratch
        // and 40GiB available RAM requirements account for full copies and VMs.
        // This checks full-file correctness, not lazy/COW restore or performance.
        let available_kib: u64 = std::fs::read_to_string("/proc/meminfo")?
            .lines()
            .find_map(|line| {
                line.strip_prefix("MemAvailable:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
            .context("host available RAM")?;
        anyhow::ensure!(
            available_kib >= 40 * 1024 * 1024,
            "8GiB full-file qualification requires 40GiB available host RAM"
        );
        let path = runtime_path.join("ram");
        std::fs::create_dir(&path)?;
        let target = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        let tmpfs = std::ffi::CString::new("tmpfs")?;
        let options = std::ffi::CString::new(format!(
            "size=32g,uid={},gid={},mode=0700",
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() }
        ))?;
        anyhow::ensure!(
            unsafe {
                libc::mount(
                    tmpfs.as_ptr(),
                    target.as_ptr(),
                    tmpfs.as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    options.as_ptr().cast(),
                )
            } == 0,
            "mount private full-RAM scratch: {}",
            std::io::Error::last_os_error()
        );
        app.runtime_path = path;
        // Errors retain the mount until its isolated mount namespace ends;
        // any still-running owned runner keeps that namespace and its files.
        Some(target)
    } else {
        None
    };
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut build = spec(&assets)?;
    build.resources.memory_mib = memory_mib;
    let current_launch = launch(&assets)?;
    let (id, token) = (
        current_launch.sandbox_id,
        current_launch.envd_access_token.clone(),
    );
    let mut source = factory.build_fresh(build.clone(), current_launch)?;
    source.start().await?;
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    start_echo(&source, &marker).await?;
    let traffic = ActiveTraffic::start(&source).await?;
    let before = traffic.count();
    let before_ram = ram_progress(&source, &marker).await?;
    let captured = source.snapshot().await?;
    assert_eq!(
        captured.manifest().memory.virtual_size,
        u64::from(memory_mib) << 20
    );
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    drop(captured);
    traffic.wait_after(before).await?;
    assert!(ram_progress(&source, &marker).await? > before_ram);
    let paused = source.pause(None).await?;
    traffic.stop().await?;
    source.stop().await?;
    drop(source);
    let mut target = factory.build_paused(id, paused.as_ref(), token.clone())?;
    drop(paused);
    target.start_nowait().await?;
    let (mut completed, network) = held_io_cut(&mut target, &marker).await?;
    assert!(network > 10, "RX/TX counters did not survive capture");
    target.wait_for_ready().await?;
    assert!(ram_progress(&target, &marker).await? > completed);
    let traffic = ActiveTraffic::start(&target).await?;
    let paused = target.pause(None).await?;
    traffic.stop().await?;
    target.stop().await?;
    drop(target);
    let mut target = factory.build_paused(id, paused.as_ref(), token)?;
    drop(paused);
    target.start_nowait().await?;
    let cut = held_io_cut(&mut target, &marker).await?;
    assert!(
        cut.0 > completed && cut.1 > network,
        "repeated capture stalled block or network progress"
    );
    completed = cut.0;
    target.wait_for_ready().await?;
    assert!(ram_progress(&target, &marker).await? > completed);
    let traffic = ActiveTraffic::start(&target).await?;
    traffic.wait_after(traffic.count()).await?;
    traffic.stop().await?;
    target.stop().await?;
    drop(target);

    // Separate lifetime from repeated pause so this full-memory oracle never
    // needs more than two live child RAM files plus one shared capture.
    let parent_launch = launch(&assets)?;
    let parent_token = parent_launch.envd_access_token.clone();
    let mut parent = factory.build_fresh(build, parent_launch)?;
    parent.start().await?;
    let marker = start_active_io(&parent, &app, parent_token.as_ref()).await?;
    start_echo(&parent, &marker).await?;
    let traffic = ActiveTraffic::start(&parent).await?;
    let requests = [fork_spec()?, fork_spec()?];
    let results = parent.fork(&requests).await?;
    let mut children = results.into_iter().collect::<Result<Vec<_>>>()?;
    assert_eq!(children.len(), 2);
    assert_eq!(parent.runtime_status().await?.phase, RuntimePhase::Running);
    traffic.wait_after(traffic.count()).await?;
    let mut child_traffic = vec![];
    for (index, child) in children.iter().enumerate() {
        child_traffic.push(ActiveTraffic::start(child.as_ref()).await?);
        let output = fork_command(child.as_ref(), requests[index].envd_access_token.as_ref(), &format!(
            "/tmp/db-m1-active-io read '{marker}'; /tmp/db-m1-active-io network-read '{marker}'; test \"$E2B_SANDBOX_ID\" = {}; printf child-{index} >/var/tmp/db-m1-isolated; printf child-{index} >/dev/shm/db-m1-isolated; sync", requests[index].sandbox_id
        )).await?;
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        assert!(
            output
                .stdout
                .split_whitespace()
                .all(|word| word.parse::<u64>().is_ok_and(|n| n > 10)),
            "child block/RAM/RX/TX not live: {}",
            output.stdout
        );
    }
    for (index, child) in children.iter().enumerate() {
        child_traffic[index]
            .wait_after(child_traffic[index].count())
            .await?;
        let output = fork_command(child.as_ref(), requests[index].envd_access_token.as_ref(), &format!(
            "test \"$(cat /var/tmp/db-m1-isolated)\" = child-{index}; test \"$(cat /dev/shm/db-m1-isolated)\" = child-{index}"
        )).await?;
        assert_eq!(
            output.exit_code, 0,
            "child memory/root contaminated: {}",
            output.stderr
        );
    }
    assert_eq!(
        parent
            .run_command(
                "/bin/sh",
                &[
                    "-ec",
                    "test ! -e /var/tmp/db-m1-isolated; test ! -e /dev/shm/db-m1-isolated"
                ]
            )
            .await?
            .exit_code,
        0
    );
    for traffic in child_traffic {
        traffic.stop().await?;
    }
    for child in &mut children {
        child.stop().await?;
    }
    drop(children);
    traffic.stop().await?;
    parent.stop().await?;
    drop(parent);
    if let Some(target) = scratch_mount {
        anyhow::ensure!(
            unsafe { libc::umount2(target.as_ptr(), 0) } == 0,
            "unmount positively stopped full-RAM fixture: {}",
            std::io::Error::last_os_error()
        );
    }
    std::fs::remove_dir_all(runtime_path)?;
    println!("active I/O repeat-capture and two concurrent children qualified at {memory_mib}MiB");
    Ok(())
}

#[tokio::test]
#[ignore = "real 1GiB full RAM, active block/RX/TX, repeated pause and concurrent children"]
async fn dragonball_lifecycle_active_io_memory_1g() -> Result<()> {
    active_io_memory_case(1024).await
}

#[tokio::test]
#[ignore = "real 2GiB full RAM, active block/RX/TX, repeated pause and concurrent children"]
async fn dragonball_lifecycle_active_io_memory_2g() -> Result<()> {
    active_io_memory_case(2048).await
}

#[tokio::test]
#[ignore = "real 8GiB full RAM with above-4GiB GPA marker and active block/RX/TX"]
async fn dragonball_lifecycle_active_io_memory_8g() -> Result<()> {
    active_io_memory_case(8192).await
}

#[tokio::test]
#[ignore = "real rootfs restack succeeds then staging fails; source must teardown"]
async fn dragonball_lifecycle_capture_post_restack_failure_is_terminal() -> Result<()> {
    let (app, assets) = fault_fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let work = source.work_dir()?.to_owned();
    let (reached, reached_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let _proxy = install_reply_fault(&work, "never", Some((reached, release_rx)))?;
    let output = tempfile::tempdir()?;
    let mut capture = Box::pin(source.capture_to_dir(output.path()));
    tokio::select! {
        result = &mut capture => anyhow::bail!("capture escaped staging fault barrier: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), reached_rx) => { result??; }
    }
    // The preflight was empty. Fail only the final image-config adoption,
    // after the real ublk daemon has sealed/restacked the writable root.
    std::fs::create_dir_all(output.path().join("rootfs/image.json"))?;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("capture abandoned before restack fault"))?;
    let error = capture.await.unwrap_err();
    assert!(error.is_terminal(), "{error:#}");
    assert!(
        std::fs::metadata(output.path().join("rootfs/snapshot.commit"))?.len() > 0,
        "did not actually seal the writable root before staging failure"
    );
    assert!(
        !work.exists(),
        "post-mutation Terminal returned without teardown"
    );
    assert!(source.executor().is_err());
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux KVM/ublk; released capture RAM must not stay pinned by source disks"]
async fn dragonball_lifecycle_released_capture_reclaims_memory_while_source_runs() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    for _ in 0..3 {
        let capture = source.snapshot().await?;
        let memory = capture.manifest().memory.image_config_path.clone();
        let state = capture.manifest().vm_state.path.clone();
        let disk = capture.manifest().rootfs.image_config_path.clone();
        assert!(memory.is_file() && state.is_file() && disk.is_file());
        drop(capture);
        assert!(
            !memory.exists() && !state.exists(),
            "source retained capture-only files"
        );
        assert!(disk.is_file(), "source still uses sealed disk lowers");
        assert_eq!(
            source
                .run_command("/agentenv/bin/busybox", &["true"])
                .await?
                .exit_code,
            0
        );
    }
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux KVM/ublk; API split readiness failure must retain bounded diagnostics"]
async fn dragonball_lifecycle_split_readiness_failure_preserves_archive() -> Result<()> {
    let (mut app, assets) = fixture().await?;
    let local = tempfile::tempdir()?;
    app.runtime_path = local.path().join("runtime");
    // The real envd readiness loop rejects a zero budget before probing health.
    app.envd.init_timeout_secs = 0;
    let evidence = app.runtime_path.join("dragonball-evidence/failures-v1");
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut sandbox = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    sandbox.start_nowait().await?;
    let work = sandbox.work_dir()?.to_owned();
    // Exercise the API's split start/wait/stop path, not start()'s rollback.
    let result = sandbox.wait_for_ready().await;
    sandbox.stop().await?;
    sandbox.stop().await?;
    let error = result.expect_err("zero readiness budget unexpectedly succeeded");
    assert!(
        format!("{error:#}").contains("timed out waiting for envd"),
        "unexpected readiness failure: {error:#}"
    );
    assert!(!work.exists(), "failed runtime work was not released");
    assert!(runtime_work_is_released(&app.runtime_path)?);
    assert!(
        evidence.is_dir(),
        "split readiness failure lost its diagnostic archive"
    );
    let archives = std::fs::read_dir(&evidence)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(archives.len(), 1, "one failure must produce one archive");
    let archive = &archives[0];
    for name in [
        "serial.log",
        "runner-error.log",
        "runtime-directory.txt",
        "metadata-requests.json",
    ] {
        let metadata = std::fs::metadata(archive.join(name))?;
        assert!(metadata.is_file(), "missing diagnostic file {name}");
        assert!(metadata.len() <= 64 * 1024, "unbounded diagnostic {name}");
    }
    assert_eq!(
        std::fs::read_to_string(archive.join("runtime-directory.txt"))?,
        work.display().to_string()
    );
    let requests: Vec<Value> =
        serde_json::from_slice(&std::fs::read(archive.join("metadata-requests.json"))?)?;
    assert!(requests.len() <= 64, "unbounded metadata request history");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux KVM/ublk; normal Held shutdown must preserve persisted RAM without failure evidence"]
async fn dragonball_lifecycle_pause_stop_preserves_committed_memory_without_failure_archive(
) -> Result<()> {
    let (mut app, assets) = fixture().await?;
    let local = tempfile::tempdir()?;
    app.runtime_path = local.path().join("runtime");
    let evidence = app.runtime_path.join("dragonball-evidence/failures-v1");
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let token = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let pause_root = local.path().join("paused");
    let paused = source.pause(Some(&pause_root)).await?;
    let encoded = paused.encode()?;
    let original_disk = PathBuf::from(
        encoded["spec"]["image_config_path"]
            .as_str()
            .context("rootfs path")?,
    );
    let memory = PathBuf::from(
        encoded["memory_image_config_path"]
            .as_str()
            .context("memory path")?,
    );
    let state = PathBuf::from(encoded["vm_state_path"].as_str().context("state path")?);
    paused.persistence_committed();
    source.stop().await?;
    drop(source);
    drop(paused);
    assert!(
        memory.is_file() && state.is_file(),
        "persisted pause lost memory artifacts"
    );
    assert!(!evidence.exists(), "normal Held stop archived a failure");
    let decoded = factory.decode_paused_state(pause_root, encoded)?;
    let mut restored = factory.build_paused(id, decoded.as_ref(), token)?;
    restored.start().await?;
    let capture = restored.snapshot().await?;
    let disk: overlaybd::config::ImageConfig = serde_json::from_slice(&std::fs::read(
        &capture.manifest().rootfs.image_config_path,
    )?)?;
    assert!(
        disk.lowers
            .iter()
            .all(|lower| !Path::new(&lower.file).starts_with(original_disk.parent().unwrap())),
        "capture retained disk paths in the previous custom pause directory"
    );
    restored.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real full-RAM lower ENOSPC in isolated mount namespace; source must be Running"]
async fn dragonball_lifecycle_capture_enospc_is_recoverable_running() -> Result<()> {
    use std::ffi::CString;
    let (app, assets) = fixture().await?;
    let output = tempfile::tempdir()?;
    let target = CString::new(output.path().as_os_str().as_encoded_bytes())?;
    let tmpfs = CString::new("tmpfs")?;
    let options = CString::new(format!(
        "size=64k,uid={},gid={},mode=0700",
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() }
    ))?;
    anyhow::ensure!(
        unsafe {
            libc::mount(
                tmpfs.as_ptr(),
                target.as_ptr(),
                tmpfs.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                options.as_ptr().cast(),
            )
        } == 0,
        "mount isolated ENOSPC fixture: {}",
        std::io::Error::last_os_error()
    );
    struct Mounted(CString);
    impl Drop for Mounted {
        fn drop(&mut self) {
            let result = unsafe { libc::umount2(self.0.as_ptr(), 0) };
            assert_eq!(
                result,
                0,
                "unmount own ENOSPC fixture: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    let _mounted = Mounted(target);
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let error = source.capture_to_dir(output.path()).await.unwrap_err();
    assert!(!error.is_terminal(), "{error:#}");
    let cause = match &error {
        agentenv::sandbox::SandboxCaptureError::Recoverable(cause) => cause,
        agentenv::sandbox::SandboxCaptureError::Terminal(_) => unreachable!("checked above"),
    };
    assert!(
        cause.chain().any(|cause| cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.raw_os_error() == Some(libc::ENOSPC))),
        "not an actual ENOSPC: {cause:#}"
    );
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert_eq!(source.run_command("/bin/true", &[]).await?.exit_code, 0);
    assert!(!output.path().join("memory-image.json").exists());
    assert_eq!(
        std::fs::read_dir(output.path())?.count(),
        1,
        "partial/final RAM lower survived failure"
    );
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real snapshot must return with source Running and payload owning artifacts"]
async fn dragonball_lifecycle_snapshot_source_continues() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let launch = launch(&assets)?;
    let token = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    let before = ram_progress(&source, &marker).await?;
    let captured = source.snapshot().await?;
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert!(ram_progress(&source, &marker).await? > before);
    let state = captured.manifest().vm_state.path.clone();
    source.stop().await?;
    assert!(
        state.is_file(),
        "capture owner died with source before publication"
    );
    drop(captured);
    assert!(
        !state.exists(),
        "unpublished capture leaked after final owner"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "M2 same-held process_vm_readv/full-export oracle; real KVM/ublk/guest"]
async fn dragonball_memory_leased_map_matches_held_full_ram_oracle() -> Result<()> {
    use agentenv_vmm_protocol::{runtime::*, runtime_memory::MemoryMapAssembler, *};
    use std::{io::IoSliceMut, os::unix::fs::FileExt};
    use tokio::io::AsyncWriteExt;
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut source = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    source.start().await?;
    let work = source.work_dir()?.to_path_buf();
    let socket = work.join("runner.sock");
    let peer = tokio::net::UnixStream::connect(&socket)
        .await?
        .peer_cred()?;
    assert_eq!(peer.uid(), unsafe { libc::geteuid() });
    let pid = peer.pid().context("runner peer PID")?;
    let args = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let args: Vec<_> = args.split(|byte| *byte == 0).collect();
    let index = args
        .iter()
        .position(|arg| *arg == b"--instance-id")
        .context("runner instance flag")?;
    let instance = String::from_utf8(args[index + 1].to_vec())?;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let status = std::fs::read_to_string(task?.path().join("status"))?;
        assert!(status.contains("Seccomp:\t2") && status.contains("NoNewPrivs:\t1"));
        assert!(status.contains("CapEff:\t0000000000000000"));
    }
    // This test takes over control at a disjoint monotonic ID range. It never
    // returns this runner to the sandbox client; normal teardown reaps it.
    let mut request_id = 1_000_000u64;
    let mut call = |command| {
        request_id += 1;
        let request = RuntimeRequest {
            mode: "runtime".into(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            request_id,
            sandbox_instance_id: instance.clone(),
            budget_ms: 60_000,
            command,
        };
        let socket = socket.clone();
        async move {
            let mut stream = tokio::net::UnixStream::connect(socket).await?;
            stream.write_all(&encode_runtime_request(&request)?).await?;
            let frame =
                tokio::time::timeout(Duration::from_secs(65), runtime_frame(&mut stream)).await??;
            let response = decode_runtime_response(&frame)?;
            validate_runtime_response(&request, &response)?;
            Ok::<_, anyhow::Error>(response.result)
        }
    };
    let RuntimeReply::Held(report) = call(RuntimeCommand::BeginCapture)
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
    else {
        anyhow::bail!("missing held report")
    };
    assert_eq!(report.worker_acks[0].device_id, "balloon:memory");
    let mut assembler = MemoryMapAssembler::new(
        report.token.clone(),
        "packed-ram-4k-v2".into(),
        report.memory_layout.clone(),
    )
    .map_err(anyhow::Error::msg)?;
    let mut cursor = 0;
    let map = loop {
        let RuntimeReply::MemoryMap(chunk) = call(RuntimeCommand::ExportMemoryMap {
            token: report.token.clone(),
            cursor,
        })
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        else {
            anyhow::bail!("missing map page")
        };
        let next = chunk.next_cursor;
        if let Some(map) = assembler.push(chunk).map_err(anyhow::Error::msg)? {
            break map;
        }
        cursor = next.context("next page")?;
    };
    let RuntimeReply::Status(status) = call(RuntimeCommand::Status)
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
    else {
        anyhow::bail!("missing status")
    };
    assert!(status.memory_map_pinned);
    assert_eq!(
        call(RuntimeCommand::EndCapture {
            token: report.token.clone()
        })
        .await?
        .unwrap_err()
        .code,
        "memory_map_pinned"
    );
    let state = work.join("oracle-state.json");
    let memory = work.join("oracle-memory.bin");
    call(RuntimeCommand::ExportHeldSnapshot {
        token: report.token.clone(),
        state_path: state,
        memory_path: memory.clone(),
    })
    .await?
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    call(RuntimeCommand::ExportHeldState {
        token: report.token.clone(),
        state_path: work.join("state-only.json"),
    })
    .await?
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let full = std::fs::File::open(memory)?;
    assert_eq!(full.metadata()?.len(), report.memory_layout.image_size);
    for region in &map.regions {
        let mut offset = 0;
        while offset < region.length {
            let length = (region.length - offset).min(2 * 1024 * 1024) as usize;
            let mut actual = vec![0; length];
            let mut read = 0;
            while read < length {
                let n = nix::sys::uio::process_vm_readv(
                    nix::unistd::Pid::from_raw(pid),
                    &mut [IoSliceMut::new(&mut actual[read..])],
                    &[nix::sys::uio::RemoteIoVec {
                        base: (region.host_base + offset + read as u64) as usize,
                        len: length - read,
                    }],
                )?;
                anyhow::ensure!(n > 0, "short remote read");
                read += n;
            }
            let mut expected = vec![0; length];
            full.read_exact_at(&mut expected, region.image_offset + offset)?;
            assert!(
                actual == expected,
                "RAM mismatch at packed offset {}",
                region.image_offset + offset
            );
            offset += length as u64;
        }
    }
    call(RuntimeCommand::ReleaseMemoryMap {
        token: report.token.clone(),
    })
    .await?
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    call(RuntimeCommand::EndCapture {
        token: report.token,
    })
    .await?
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    call(RuntimeCommand::Stop)
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real same-ID in-place pause/resume retains RAM and idempotency"]
async fn dragonball_lifecycle_pause_resume_keeps_id_and_ram() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id;
    let token = launch.envd_access_token.clone();
    let mut source = factory.build_fresh(spec(&assets)?, launch)?;
    source.start().await?;
    let marker = start_active_io(&source, &app, token.as_ref()).await?;
    let before = ram_progress(&source, &marker).await?;
    let paused = source.pause(None).await?;
    assert!(source.executor().is_err());
    source.resume().await?;
    source.resume().await?;
    assert_eq!(source.runtime_status().await?.phase, RuntimePhase::Running);
    assert!(ram_progress(&source, &marker).await? > before);
    let identity = source
        .run_command("/bin/sh", &["-ec", "printf '%s' \"$E2B_SANDBOX_ID\""])
        .await?;
    assert_eq!(identity.stdout, id.to_string());
    source.stop().await?;
    let mut restored = factory.build_paused(id, paused.as_ref(), token)?;
    restored.start().await?;
    assert!(ram_progress(&restored, &marker).await? > before);
    restored.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires two actual guests in disposable namespaces"]
async fn dragonball_lifecycle_auth_metadata_neighbor_deny_all_and_runner_exit() -> Result<()> {
    let (app, assets) = fixture().await?;
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let one = launch(&assets)?;
    let one_id = one.sandbox_id.to_string();
    let two = launch(&assets)?;
    let two_id = two.sandbox_id.to_string();
    let mut first = factory.build_fresh(spec(&assets)?, one)?;
    let mut second = factory.build_fresh(spec(&assets)?, two)?;
    first.start().await?;
    second.start().await?;
    let token = first.run_command("/bin/sh", &["-ec", "curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token"]).await?;
    assert_eq!(token.exit_code, 0);
    let opts = agentenv::sandbox::ProcessOpts::new().with_envs(HashMap::from([(
        "OTHER_NAMESPACE_TOKEN".into(),
        token.stdout,
    )]));
    let output = second.run_command_with_opts("/bin/sh", &["-ec", "curl --silent -o /tmp/m1-foreign -w '%{http_code}' -H \"X-metadata-token: $OTHER_NAMESPACE_TOKEN\" http://169.254.169.254/; printf '\\n'; cat /tmp/m1-foreign; printf '\\n'; t=$(curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token); curl --fail --silent -H \"X-metadata-token: $t\" http://169.254.169.254/instanceID"], &opts).await?;
    let address = format!(
        "http://{}:{}",
        first.host_interaction_ip().context("guest IP")?,
        app.tools.control_plane_port
    );
    let mut wrong = envd::process::ProcessClient::connect(&address, Some("wrong-token")).await?;
    assert!(
        wrong.list(envd::process::ListRequest {}).await.is_err(),
        "wrong process token accepted"
    );
    let mut wrong_http = envd::http_client::apis::configuration::Configuration::new();
    wrong_http.base_path = address;
    wrong_http.api_key = Some(envd::http_client::apis::configuration::ApiKey {
        prefix: None,
        key: "wrong-token".into(),
    });
    let Err(envd::http_client::apis::Error::ResponseError(response)) =
        envd::http_client::apis::default_api::envs_get(&wrong_http).await
    else {
        anyhow::bail!("wrong HTTP token did not return authentication rejection");
    };
    assert_eq!(response.status.as_u16(), 401);
    let work = first.work_dir()?.to_owned();
    let socket = tokio::net::UnixStream::connect(work.join("runner.sock")).await?;
    let credential = socket.peer_cred()?;
    assert_eq!(credential.uid(), unsafe { libc::geteuid() });
    let pid = credential.pid().context("runner peer PID")?;
    drop(socket);
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    tokio::time::timeout(Duration::from_secs(2), async {
        while first.executor().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    first.stop().await?;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "runner not reaped"
    );
    assert!(
        !work.exists(),
        "runner work still live after confirmed teardown"
    );
    let sibling = second
        .run_command("/bin/sh", &["-ec", "printf sibling-still-running"])
        .await?;
    second.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(
        output.stdout.starts_with("401\n"),
        "foreign metadata token accepted"
    );
    assert!(!output.stdout.contains(&one_id));
    assert!(output.stdout.ends_with(&two_id));
    assert_eq!(sibling.stdout, "sibling-still-running");
    Ok(())
}

fn runtime_work_is_released(path: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(path)? {
        // Failed/cancelled starts retain bounded diagnostics, independently of
        // the runtime directories and leases that must have been released.
        if entry?.file_name() != "dragonball-evidence" {
            return Ok(false);
        }
    }
    Ok(true)
}

#[tokio::test]
#[ignore = "requires disposable namespace; disk failure after runner spawn must release ownership"]
async fn dragonball_lifecycle_disk_failure_reaps_early_runner() -> Result<()> {
    let (mut app, assets) = fixture().await?;
    let isolated = tempfile::tempdir()?;
    app.runtime_path = isolated.path().join("runtime");
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut build = spec(&assets)?;
    build.image_config_path = isolated.path().join("invalid-root-image.json");
    std::fs::write(&build.image_config_path, b"invalid image config")?;
    let mut sandbox = factory.build_fresh(build, launch(&assets)?)?;
    let error = sandbox
        .start()
        .await
        .err()
        .context("invalid disk was accepted")?;
    assert!(
        format!("{error:#}").contains("invalid-root-image.json"),
        "{error:#}"
    );
    assert!(sandbox.executor().is_err());
    sandbox.stop().await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime_work_is_released(&app.runtime_path)? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let evidence = app.runtime_path.join("dragonball-evidence/failures-v1");
    let archives = std::fs::read_dir(evidence)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(archives.len(), 1);
    assert!(
        archives[0].join("runner-error.log").is_file(),
        "runner must start before disk preparation"
    );
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        if let Ok(command) = std::fs::read(entry.path().join("cmdline")) {
            assert!(
                !command
                    .windows(app.runtime_path.as_os_str().len())
                    .any(|part| {
                        use std::os::unix::ffi::OsStrExt;
                        part == app.runtime_path.as_os_str().as_bytes()
                    }),
                "failed startup retained a runner: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable namespace; proves metadata failure rolls back allocations"]
async fn dragonball_lifecycle_metadata_failure_rolls_back() -> Result<()> {
    let (app, assets) = fixture().await?;
    let isolated = tempfile::tempdir()?;
    let mut app = app.clone();
    app.runtime_path = isolated.path().join("runtime");
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let mut launch = launch(&assets)?;
    launch
        .extra_mmds
        .insert("oversized".into(), Value::String("x".repeat(1024 * 1024)));
    let mut sandbox = factory.build_fresh(spec(&assets)?, launch)?;
    let error = sandbox
        .start()
        .await
        .err()
        .context("oversized metadata was accepted")?;
    assert!(
        format!("{error:#}").contains("metadata document exceeds"),
        "{error:#}"
    );
    assert!(sandbox.executor().is_err());
    sandbox.stop().await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime_work_is_released(&app.runtime_path)? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires real configured extension HTTP failure and disposable namespace"]
async fn dragonball_lifecycle_extension_failure_fails_start_and_pairs_stop() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut app, assets) = fixture().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    app.custom_extension.url = Some(format!("http://{}", listener.local_addr()?));
    app.custom_extension.timeout_ms = 1000;
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await?;
            let mut bytes = vec![];
            let boundary = loop {
                bytes.push(stream.read_u8().await?);
                if bytes.ends_with(b"\r\n\r\n") {
                    break bytes.len();
                }
                anyhow::ensure!(bytes.len() < 8192, "oversized HTTP fixture header");
            };
            let header = std::str::from_utf8(&bytes)?.to_string();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|size| size.trim().to_owned())
                })
                .context("missing request body length")?
                .parse()?;
            bytes.resize(boundary + length, 0);
            stream.read_exact(&mut bytes[boundary..]).await?;
            requests.push((
                header.lines().next().unwrap().to_owned(),
                serde_json::from_slice::<Value>(&bytes[boundary..])?,
            ));
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
        }
        Ok::<_, anyhow::Error>(requests)
    });
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let mut sandbox = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    let result = sandbox.start().await;
    sandbox.stop().await?;
    if result.is_ok() {
        server.abort();
    }
    assert!(
        result.is_err(),
        "configured extension start failure was ignored"
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("start-fresh hook failed"));
    let requests = tokio::time::timeout(Duration::from_secs(3), server).await???;
    assert!(requests[0].0.contains("/sandbox-hook/start-fresh"));
    assert!(requests[1].0.contains("/sandbox-hook/stop"));
    assert_eq!(requests[0].1["sandboxId"], requests[1].1["sandboxId"]);
    assert_eq!(
        requests[0].1["sandboxInstanceId"],
        requests[1].1["sandboxInstanceId"]
    );
    assert!(requests[0].1["sandboxInstanceId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    Ok(())
}

#[tokio::test]
#[ignore = "requires real detached startup and disposable namespace"]
async fn dragonball_lifecycle_cancel_during_startup_keeps_owner_until_reap() -> Result<()> {
    let (mut app, assets) = fixture().await?;
    let isolated = tempfile::tempdir()?;
    app.runtime_path = isolated.path().join("runtime");
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let mut sandbox = factory.build_fresh(spec(&assets)?, launch(&assets)?)?;
    {
        let mut startup = Box::pin(sandbox.start());
        tokio::select! {
            result = &mut startup => { result?; anyhow::bail!("startup finished before cancellation fixture"); }
            result = async {
                loop {
                    if app.runtime_path.exists() && std::fs::read_dir(&app.runtime_path)?.next().is_some() {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            } => { result?; }
        }
        drop(startup);
    }
    assert!(sandbox.executor().is_err());
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if runtime_work_is_released(&app.runtime_path)? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    sandbox.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real snapshot and fork must confirm identity without a tools shell"]
async fn dragonball_lifecycle_new_identity_requires_guest_confirmation() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut build = spec(&assets)?;
    build.context.user = Some("root".into());
    let initial = launch(&assets)?;
    let id = initial.sandbox_id;
    let mut source = factory.build_fresh(build.clone(), initial)?;
    source.start().await?;
    let setup = source.run_command("/bin/sh", &["-ec",
        "printf '#!/bin/sh\\nexit 97\\n' >/var/tmp/no-identity-shell; chmod 755 /var/tmp/no-identity-shell; mount --bind /var/tmp/no-identity-shell /agentenv/bin/busybox"
    ]).await?;
    assert_eq!(setup.exit_code, 0, "{}", setup.stderr);
    let fork = fork_spec()?;
    let fork_id = fork.sandbox_id;
    let fork_token = fork.envd_access_token.clone();
    let mut children = source.fork(&[fork]).await?;
    let mut forked = children.remove(0)?;
    assert_ne!(fork_id, id);
    let fork_output = fork_command(
        forked.as_ref(),
        fork_token.as_ref(),
        "printf '%s\\n%s' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\"",
    )
    .await;
    forked.stop().await?;
    let fork_output = fork_output?;
    assert_eq!(fork_output.exit_code, 0, "{}", fork_output.stderr);
    assert_eq!(
        fork_output.stdout,
        format!("{fork_id}\ndragonball-m1-existing-guest")
    );
    let captured = source.snapshot().await?;
    let metadata = publish_metadata(id, &build, &assets, &captured)?;
    let root = tempfile::tempdir()?;
    let manager = posix_manager(root.path())?;
    let record = agentenv::sandbox::publish_captured(&manager, metadata, captured).await?;
    source.stop().await?;
    let runnable = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("snapshot")?;
    let mut next = launch(&assets)?;
    next.snapshot_id = record.id.to_string();
    let child_id = next.sandbox_id;
    assert_ne!(child_id, id);
    let template_id = next.snapshot_id.clone();
    let child_token = next.envd_access_token.clone();
    let mut child = factory.build_from_snapshot(&runnable, next)?;
    child.start().await?;
    let output = fork_command(
        child.as_ref(),
        child_token.as_ref(),
        "printf '%s\\n%s' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\"",
    )
    .await;
    child.stop().await?;
    let output = output?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, format!("{child_id}\n{template_id}"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux KVM/ublk/assets and disposable network/mount namespace"]
async fn dragonball_lifecycle_same_id_resume_does_not_require_identity_shell() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut build = spec(&assets)?;
    build.context.user = Some("root".into());
    let launch = launch(&assets)?;
    let (id, token) = (launch.sandbox_id, launch.envd_access_token.clone());
    let mut source = factory.build_fresh(build, launch)?;
    source.start().await?;
    // Health and /init remain available. Only the optional tools shell is
    // unavailable; restoring an application must not depend on executing it.
    let setup = source.run_command("/bin/sh", &["-ec",
        "printf '#!/bin/sh\\nexit 97\\n' >/var/tmp/no-identity-shell; chmod 755 /var/tmp/no-identity-shell; mount --bind /var/tmp/no-identity-shell /agentenv/bin/busybox"
    ]).await?;
    assert_eq!(setup.exit_code, 0, "{}", setup.stderr);
    let paused = source.pause(None).await?;
    source.stop().await?;
    let mut restored = factory.build_paused(id, paused.as_ref(), token)?;
    let result = restored.start().await;
    if let Err(error) = result {
        restored.stop().await?;
        anyhow::bail!("health/init readiness depended on an identity shell: {error:#}");
    }
    let output = restored
        .run_command("/bin/sh", &["-ec", "printf application-resumed"])
        .await;
    restored.stop().await?;
    let output = output?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, "application-resumed");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux KVM/ublk/assets and disposable network/mount namespace"]
async fn dragonball_lifecycle_fresh_ready_preserves_guest_contract() -> Result<()> {
    let (app, assets) = fixture().await?;
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let launch = launch(&assets)?;
    let id = launch.sandbox_id.to_string();
    let mut sandbox = factory.build_fresh(spec(&assets)?, launch)?;
    assert!(
        sandbox.executor().is_err(),
        "unready backend exposed an executor"
    );
    sandbox.start().await?;
    let output = sandbox.run_command("/bin/sh", &["-ec",
        "test -b /dev/vda; test -b /dev/vdb; test -x /agentenv/envd; findmnt -n -o SOURCE /; findmnt -n -o OPTIONS /agentenv; id -u; pwd; printf '%s\\n%s\\n%s\\n' \"$DB_M1_ENV\" \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\""]).await?;
    sandbox.stop().await?;
    assert!(
        sandbox.executor().is_err(),
        "stopped backend exposed an executor"
    );
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("vdb"), "{}", output.stdout);
    assert!(output.stdout.contains("ro"), "{}", output.stdout);
    assert!(
        output.stdout.contains("1000\n/tmp\nguest-contract\n"),
        "{}",
        output.stdout
    );
    assert!(
        output
            .stdout
            .contains(&format!("{id}\ndragonball-m1-existing-guest\n")),
        "{}",
        output.stdout
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real guest readiness with non-root USER and WORKDIR on a newly mounted volume"]
async fn dragonball_lifecycle_readiness_with_user_and_volume_workdir() -> Result<()> {
    use agentenv::volume::{VolumeManager, VolumeMode};
    let (app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, _, repository) = crate::common::snapshot_test_parts(root.path());
    let manager =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let volume = manager
        .create(
            "readiness-workdir".into(),
            VolumeMode::Exclusive,
            None,
            None,
            128,
        )
        .await?;
    let mut build = spec(&assets)?;
    build.context.user = Some("nobody".into());
    build.context.workdir = "/mnt/readiness-workdir".into();
    build.extra_drives.push(ExtraDrive::Overlaybd {
        drive_id: volume.id.clone(),
        image_config_path: volume.backing_image_config.context("volume backing")?,
        read_only: false,
        mount_path: build.context.workdir.clone().into(),
        virtual_size: Some(128 << 20),
        sub_path: None,
        snapshot_output_dir: None,
        volume: true,
    });
    let mut launch = launch(&assets)?;
    launch.env_vars = Some(HashMap::from([(
        "DB_M1_ENV".into(),
        "launch-override".into(),
    )]));
    let id = launch.sandbox_id;
    manager.reserve(&volume.id, &id.to_string()).await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut sandbox = factory.build_fresh(build, launch)?;
    sandbox.start().await?;
    for resumed in [false, true] {
        if resumed {
            let paused = sandbox.pause(None).await?;
            sandbox.resume().await?;
            drop(paused);
        }
        let output = sandbox.run_command("/agentenv/bin/busybox", &["sh", "-ec",
            "test \"$(id -un)\" = nobody; test \"$PWD\" = /mnt/readiness-workdir; mountpoint -q .; test \"$DB_M1_ENV\" = launch-override; printf '%s\\n%s' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\""
        ]).await?;
        assert_eq!(output.exit_code, 0, "resumed={resumed}: {}", output.stderr);
        assert_eq!(output.stdout, format!("{id}\ndragonball-m1-existing-guest"));
    }
    sandbox.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real guest startup with empty image USER and WORKDIR"]
async fn dragonball_lifecycle_readiness_empty_context_keeps_bootstrap_defaults() -> Result<()> {
    let (app, assets) = fixture().await?;
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let mut build = spec(&assets)?;
    build.context.user = Some(String::new());
    build.context.workdir.clear();
    let mut sandbox = factory.build_fresh(build, launch(&assets)?)?;
    sandbox.start().await?;
    let output = sandbox
        .run_command("/agentenv/bin/busybox", &["sh", "-ec", "id -un; pwd"])
        .await?;
    sandbox.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, "root\n/\n");
    Ok(())
}

#[tokio::test]
#[ignore = "requires prepared assets; validates refusal before any managed allocation"]
async fn dragonball_lifecycle_unsupported_profile_allocates_nothing() -> Result<()> {
    let (mut app, assets) = fixture().await?;
    let isolated = tempfile::tempdir()?;
    app.runtime_path = isolated.path().join("must-not-allocate");
    app.virtualization_mode = VirtualizationMode::Pvm;
    assert!(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?
    )
    .is_err());
    assert!(
        !app.runtime_path.exists(),
        "unsupported profile allocated runtime state"
    );
    app.virtualization_mode = VirtualizationMode::Kvm;
    let policy = DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?;
    let factory = DragonballSandboxFactory::new(policy)?;
    let mut invalid = spec(&assets)?;
    invalid.resources.cpu_count = 256;
    assert!(factory.build_fresh(invalid, launch(&assets)?).is_err());
    assert!(
        !app.runtime_path.exists(),
        "invalid CPU profile allocated resources"
    );
    Ok(())
}
