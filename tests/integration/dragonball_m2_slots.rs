use agentenv::sandbox::{
    dragonball::{DragonballMemoryMode, DragonballRuntimePolicy, DragonballSandboxFactory},
    ExtraDrive, SandboxBackend, SandboxBackendFactory, SandboxExecutor,
};
use agentenv::snapshot::{
    repository::backends::{PosixFsBackend, PosixFsBackendConfig},
    SnapshotManager,
};
use agentenv::volume::{VolumeManager, VolumeMode};
use anyhow::{Context, Result};

use super::dragonball_m1::{asset, fixture, fork_command, launch, publish_metadata, spec};

#[tokio::test]
#[ignore = "real empty Dragonball volume slot binding; isolated KVM fixture"]
async fn dragonball_m2_placeholder_slot_binds_real_volume() -> Result<()> {
    slot_launch_cases(&[(64, false, 1)]).await
}

#[tokio::test]
#[ignore = "same 4KiB slot template restored with 64/128MiB filesystems"]
async fn dragonball_m2_slot_resize_reports_new_capacity() -> Result<()> {
    slot_launch_cases(&[(64, false, 1), (128, false, 1)]).await
}

#[tokio::test]
#[ignore = "real shared RO backing, guest mount/raw-write policy and binding cleanup"]
async fn dragonball_m2_slot_ro_policy_and_failure_rollback() -> Result<()> {
    slot_launch_cases(&[(64, true, 2)]).await
}

async fn slot_launch_cases(cases: &[(u64, bool, usize)]) -> Result<()> {
    let (mut app, assets) = fixture().await?;
    app.volume.max_volume_count = 2;
    let factory = DragonballSandboxFactory::new(
        DragonballRuntimePolicy::from_app_config(
            &app,
            asset(&assets, "runner_path")?,
            asset(&assets, "kernel_path")?,
        )?
        .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental),
    )?;
    let root = tempfile::tempdir_in(&app.runtime_path)?;
    let (repository, resolver) = PosixFsBackend::new(PosixFsBackendConfig {
        root: root.path().join("repository"),
        cache_root: Some(root.path().join("cache")),
        runtime_cache_root: Some(root.path().join("runtime-cache")),
    })?
    .into_parts();
    let snapshots = SnapshotManager::from_parts(repository.clone(), resolver, None);
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let mut build = spec(&assets)?;
    // Raw block capacity/write checks must run as guest root, not accidentally
    // pass RO tests because the unprivileged default user cannot open devices.
    build.context.user = Some("root".into());
    build
        .context
        .env_vars
        .insert("PATH".into(), "/no-guest-commands".into());
    let source_launch = launch(&assets)?;
    let source_id = source_launch.sandbox_id;
    let mut source = factory.build_fresh(build.clone(), source_launch)?;
    source.start().await?;
    let output = source
        .run_command(
            "/bin/sh",
            &["-ec", "printf slot-template >/dev/shm/db-m2-slot"],
        )
        .await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let captured = source.snapshot().await?;
    assert_eq!(
        captured.manifest().volume_drive_slots,
        2,
        "fresh template must reserve the configured volume slots"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&captured.manifest().vm_state.path)?)?;
    let disks = state["machine"]["block_devices"]
        .as_array()
        .context("captured disks")?;
    assert_eq!(disks.len(), 4, "tools/root must precede exactly two slots");
    for disk in &disks[2..] {
        assert_eq!(
            disk["capacity_bytes"], 4096,
            "unbound slot is not a 4KiB placeholder"
        );
    }
    let metadata = publish_metadata(source_id, &build, &assets, &captured)?;
    let record = snapshots.publish_captured(metadata, captured).await?;
    source.stop().await?;
    drop(source);
    let runnable = snapshots
        .load_runnable(record.id.to_string())
        .await?
        .context("slot template")?;
    for &(size_mb, read_only, count) in cases {
        let mut target_launch = launch(&assets)?;
        target_launch.snapshot_id = record.id.to_string();
        let owner = target_launch.sandbox_id.to_string();
        let mut ids = vec![];
        for index in 0..count {
            let volume = volumes
                .create(
                    format!("db-m2-slot-{}-{index}", uuid::Uuid::new_v4()),
                    if read_only {
                        VolumeMode::ReadOnly
                    } else {
                        VolumeMode::Exclusive
                    },
                    None,
                    None,
                    size_mb,
                )
                .await?;
            let volume = volumes.materialize_backing(&volume.id).await?;
            volumes.reserve(&volume.id, &owner).await?;
            ids.push(volume.id.clone());
            target_launch.extra_drives.push(ExtraDrive::Overlaybd {
                drive_id: volume.id,
                image_config_path: volume.backing_image_config.context("volume backing")?,
                read_only,
                mount_path: format!("/mnt/slot-{index}").into(),
                virtual_size: Some(size_mb << 20),
                sub_path: None,
                snapshot_output_dir: None,
                volume: true,
            });
        }
        // Equal-capacity disks must bind by their explicit slot/mount identity,
        // not by volume catalog ID or caller iteration order.
        target_launch.extra_drives.reverse();
        if read_only {
            let live_works =
                || -> std::io::Result<std::collections::HashSet<std::path::PathBuf>> {
                    std::fs::read_dir(&app.runtime_path)?
                        .filter_map(|entry| match entry {
                            Ok(entry)
                                if entry
                                    .file_name()
                                    .to_string_lossy()
                                    .starts_with("db-runtime-") =>
                            {
                                Some(Ok(entry.path()))
                            }
                            Ok(_) => None,
                            Err(error) => Some(Err(error)),
                        })
                        .collect()
                };
            let before = live_works()?;
            let mut duplicate = target_launch.clone();
            let mount = duplicate.extra_drives[0].mount_path().to_path_buf();
            let ExtraDrive::Overlaybd { mount_path, .. } = &mut duplicate.extra_drives[1];
            *mount_path = mount;
            assert!(factory.build_from_snapshot(&runnable, duplicate).is_err());
            assert_eq!(before, live_works()?, "invalid binding allocated a runtime");

            // Fail preparation of slot 1 after root and slot 0 were allocated.
            // The caller retains catalog ownership; runtime leases must roll back.
            let mut broken = target_launch.clone();
            let drive = broken
                .extra_drives
                .iter_mut()
                .find(|drive| drive.mount_path() == std::path::Path::new("/mnt/slot-1"))
                .unwrap();
            let ExtraDrive::Overlaybd {
                image_config_path, ..
            } = drive;
            *image_config_path = root.path().join("missing-volume-image.json");
            let mut failed = factory.build_from_snapshot(&runnable, broken)?;
            assert!(
                failed.start().await.is_err(),
                "missing backing unexpectedly started"
            );
            drop(failed);
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while live_works()? != before {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            for id in &ids {
                assert_eq!(volumes.get(id).await?.read_only_mounts, vec![owner.clone()]);
            }
        }
        let token = target_launch.envd_access_token.clone();
        let target_id = target_launch.sandbox_id;
        let bound_launch = target_launch.clone();
        let mut target = factory.build_from_snapshot(&runnable, target_launch)?;
        target.start().await?;
        let inherited = fork_command(
            target.as_ref(),
            token.as_ref(),
            "/bin/cat /dev/shm/db-m2-slot",
        )
        .await?;
        assert_eq!(inherited.exit_code, 0, "{}", inherited.stderr);
        assert_eq!(inherited.stdout, "slot-template");
        for index in 0..count {
            let capacity = fork_command(target.as_ref(), token.as_ref(), &format!(
                "export PATH=/usr/sbin:/usr/bin:/sbin:/bin; device=$(findmnt -n -o SOURCE /mnt/slot-{index}); test -b \"$device\"; blockdev --getsize64 \"$device\""
            )).await?;
            assert_eq!(capacity.exit_code, 0, "{}", capacity.stderr);
            assert_eq!(capacity.stdout.trim(), (size_mb << 20).to_string());
            let script = if read_only {
                format!("! touch /mnt/slot-{index}/forbidden && device=$(findmnt -n -o SOURCE /mnt/slot-{index}) && ! dd if=/dev/zero of=\"$device\" bs=4096 count=1 oflag=direct conv=notrunc status=none")
            } else {
                format!("printf slot-{index} >/mnt/slot-{index}/marker && sync && test \"$(cat /mnt/slot-{index}/marker)\" = slot-{index}")
            };
            let output = fork_command(
                target.as_ref(),
                token.as_ref(),
                &format!("export PATH=/usr/sbin:/usr/bin:/sbin:/bin; {script}"),
            )
            .await?;
            assert_eq!(output.exit_code, 0, "{}", output.stderr);
            if read_only {
                assert!(
                    output.stderr.contains("Input/output error"),
                    "raw write did not reach the read-only backend: {}",
                    output.stderr
                );
            }
        }
        if size_mb == 128 || read_only {
            // A slot that was empty in the original template is now a mounted
            // filesystem. Its next snapshot must stop permitting size/mode changes.
            let captured = target.snapshot().await?;
            assert_eq!(
                captured.manifest().volume_slots[0].capacity_bytes,
                size_mb << 20
            );
            assert!(
                !captured.manifest().volume_slots[0].guest_read_only,
                "rebinding backing access changed negotiated features"
            );
            let mut metadata = publish_metadata(target_id, &build, &assets, &captured)?;
            for (index, id) in ids.iter().enumerate() {
                let volume = volumes.get(id).await?;
                metadata
                    .volume_snapshots
                    .push(agentenv::snapshot::SnapshotVolume {
                        mount_path: format!("/mnt/slot-{index}"),
                        mode: volume.mode,
                        size_mb,
                        layers: volume.backing_layers,
                    });
            }
            let bound_record = snapshots.publish_captured(metadata, captured).await?;
            let bound = snapshots
                .load_runnable(bound_record.id.to_string())
                .await?
                .context("bound slot snapshot")?;
            let mut invalid = bound_launch;
            invalid.snapshot_id = bound_record.id.to_string();
            let ExtraDrive::Overlaybd {
                virtual_size,
                read_only: mode,
                ..
            } = &mut invalid.extra_drives[0];
            if read_only {
                *mode = false;
            } else {
                *virtual_size = Some(64 << 20);
            }
            let before = std::fs::read_dir(&app.runtime_path)?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
            assert!(
                factory.build_from_snapshot(&bound, invalid).is_err(),
                "mounted slot accepted a new size/access policy"
            );
            let after = std::fs::read_dir(&app.runtime_path)?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
            assert_eq!(
                before, after,
                "mounted-slot refusal allocated runtime leases"
            );
            drop(bound);
            snapshots.delete(bound_record.id.to_string()).await?;
        }
        target.stop().await?;
        drop(target);
        volumes.replace_owner_for(&owner, None, &ids).await?;
        for id in ids {
            volumes.delete(&id).await?;
        }
    }
    snapshots.delete(record.id.to_string()).await?;
    Ok(())
}
