use agentenv::volume::VolumeCatalog;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use agentenv::{
    sandbox::{
        selected, SandboxBackend, SandboxExecutor, SandboxLaunchConfig, TemplateSandboxFactory,
    },
    snapshot::{SnapshotId, SnapshotManager, SnapshotRecord},
    template::TemplateBuildSpec,
    types::SandboxId,
};
use anyhow::{Context, Result};

async fn wait_for_recording(manager: &SnapshotManager, id: &SnapshotId) -> Result<SnapshotRecord> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let current = manager
                .get(&id.to_string())
                .await?
                .context("template missing")?;
            if current.committed.as_ref().unwrap().memory_startup.is_some() {
                return Ok(current);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("template startup recording did not publish")?
}

#[tokio::test]
#[ignore = "requires selected backend assets, KVM, ublk and startup recording"]
async fn selected_backend_builds_records_and_derives_templates() -> Result<()> {
    crate::common::setup().await;
    let root = tempfile::tempdir()?;
    let (builder, manager, _) = crate::common::snapshot_test_parts(root.path());
    let spec = crate::common::default_rootfs_template_build_spec()
        .resources(1, 1024)
        .run("printf component > /tmp/component-marker");
    let record = builder.build_and_publish(&manager, spec).await?;
    #[cfg(feature = "hypervisor-firecracker")]
    let expected_backend = "firecracker";
    #[cfg(not(feature = "hypervisor-firecracker"))]
    let expected_backend = "dragonball";
    assert_eq!(
        record.committed.as_ref().unwrap().runtime_versions.backend,
        expected_backend
    );
    let mut record = wait_for_recording(&manager, &record.id).await?;
    record.committed.as_mut().unwrap().custom_extension_params = Some(
        serde_json::json!({"sourceSandbox": "must-not-inherit"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let runnable = manager.resolve_runnable(record).await?;
    assert_eq!(runnable.manifest().backend, expected_backend);
    #[cfg(all(
        feature = "hypervisor-dragonball",
        not(feature = "hypervisor-firecracker")
    ))]
    {
        use agentenv::sandbox::dragonball::{DragonballRuntimePolicy, DragonballSandboxFactory};
        let mut app = agentenv::cfg::ConfigManager::global_config().clone();
        app.envd.version = "new-host-envd-version".into();
        let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
            &app,
            app.dragonball.binary_path.clone().unwrap(),
            app.kernel.image_path.clone().unwrap(),
        )?)?;
        let mut restored =
            factory.build_template_from_snapshot(&runnable, SandboxLaunchConfig::default())?;
        restored.start().await?;
        let versions = factory
            .template_versions(&restored, "new-host-tools-version".into())
            .await;
        restored.stop().await?;
        assert_eq!(
            versions?.restore_identity,
            runnable.committed().runtime_versions.restore_identity
        );
    }
    let derived = builder
        .build_from_snapshot_and_publish(
            &manager,
            TemplateBuildSpec::new().run("printf derived >> /tmp/component-marker"),
            SnapshotId::generate(),
            &runnable,
        )
        .await?;
    assert_eq!(
        derived.committed.as_ref().unwrap().runtime_versions.backend,
        expected_backend
    );
    assert!(derived
        .committed
        .as_ref()
        .unwrap()
        .custom_extension_params
        .is_none());
    let derived = wait_for_recording(&manager, &derived.id).await?;
    let runnable = manager.resolve_runnable(derived).await?;
    let factory = selected::factory_with_cpu_config(Arc::new(RwLock::new(None)))?;
    let mut sandbox = factory.build_template_from_snapshot(
        &runnable,
        SandboxLaunchConfig {
            sandbox_id: SandboxId::new(),
            snapshot_id: runnable.record().id.to_string(),
            ..Default::default()
        },
    )?;
    let result = async {
        SandboxBackend::start(&mut sandbox).await?;
        let output = sandbox
            .run_command("/agentenv/bin/busybox", &["cat", "/tmp/component-marker"])
            .await?;
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "componentderived");
        Ok(())
    }
    .await;
    let stopped = SandboxBackend::stop(&mut sandbox).await;
    result.and(stopped)
}

#[tokio::test]
#[ignore = "isolated process exercises both recording callbacks after publication is dropped"]
async fn selected_backend_shutdown_drains_unpublished_recorders() -> Result<()> {
    if std::env::var("AENV_TEST_COMPONENT_SHUTDOWN_CHILD").as_deref() != Ok("1") {
        let runtime = tempfile::tempdir()?;
        let status = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "hypervisor_components::selected_backend_shutdown_drains_unpublished_recorders",
                    "--ignored",
                    "--nocapture",
                ])
                .env("AENV_TEST_COMPONENT_SHUTDOWN_CHILD", "1")
                .env("AENV_RUNTIME_PATH", runtime.path())
                .kill_on_drop(true)
                .status(),
        )
        .await??;
        anyhow::ensure!(status.success(), "shutdown child failed: {status}");
        return Ok(());
    }
    crate::common::setup().await;
    let root = tempfile::tempdir()?;
    let (builder, manager, _) = crate::common::snapshot_test_parts(root.path());
    let record = builder
        .build_and_publish(
            &manager,
            crate::common::default_rootfs_template_build_spec().resources(1, 1024),
        )
        .await?;
    let record = wait_for_recording(&manager, &record.id).await?;
    let runnable = manager.resolve_runnable(record).await?;
    let factory = selected::factory_with_cpu_config(Arc::new(RwLock::new(None)))?;
    let mut captures = Vec::new();
    for template_capture in [false, true] {
        let mut vm =
            factory.build_template_from_snapshot(&runnable, SandboxLaunchConfig::default())?;
        SandboxBackend::start(&mut vm).await?;
        let capture = if template_capture {
            let workspace = Arc::new(tempfile::tempdir()?);
            vm.capture_to_dir(workspace.path())
                .await?
                .with_recording_lease(workspace)
        } else {
            vm.snapshot().await?
        };
        SandboxBackend::stop(&mut vm).await?;
        captures.push(capture);
    }
    // Drop publication ownership before the spawned recorder can poll. Only
    // recorder-owned shutdown registration can make drain wait for these jobs.
    let mut traces = Vec::new();
    for mut capture in captures {
        let recording = capture
            .start_recording()
            .context("recording was not admitted")?;
        traces.push(recording.trace);
        drop(recording.keep_alive);
    }
    agentenv::snapshot::drain_startup_manifest_tasks(Duration::from_secs(30)).await;
    let drained = traces.iter().all(tokio::task::JoinHandle::is_finished);
    for trace in traces {
        trace.await?;
    }
    anyhow::ensure!(
        drained,
        "shutdown returned before unpublished recorders finished cleanup"
    );
    Ok(())
}

async fn contract_fixture() -> Result<(
    agentenv::sandbox::SelectedSandboxFactory,
    agentenv::sandbox::FreshSandboxBuildSpec,
)> {
    crate::common::setup().await;
    let (_, image_config_path) =
        crate::common::default_rootfs_template_build_spec_with_image_config();
    Ok((
        selected::factory_with_cpu_config(Arc::new(RwLock::new(None)))?,
        agentenv::sandbox::FreshSandboxBuildSpec {
            image_config_path,
            context: agentenv::snapshot::CommandContext {
                user: Some("root".into()),
                ..Default::default()
            },
            resources: agentenv::types::SandboxResources {
                cpu_count: 1,
                memory_mib: 1024,
                disk_size_mib: 0,
            },
            extra_drives: vec![],
            extra_boot_args: None,
        },
    ))
}

async fn contract_command(sandbox: &dyn SandboxBackend, script: &str) -> Result<String> {
    use envd::process::{process_event, ProcessClient, ProcessConfig, StartRequest};
    let address = format!(
        "http://{}:{}",
        sandbox.host_interaction_ip().context("guest IP")?,
        agentenv::cfg::ConfigManager::global_config()
            .tools
            .control_plane_port
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut client = ProcessClient::connect(&address, None).await?;
        let mut request = tonic::Request::new(StartRequest {
            process: Some(ProcessConfig {
                cmd: "/agentenv/bin/busybox".into(),
                args: vec!["sh".into(), "-ec".into(), script.into()],
                envs: Default::default(),
                cwd: Some("/".into()),
            }),
            pty: None,
            tag: None,
            stdin: Some(false),
        });
        // Match Executor's root request: envd's default-user path invokes /bin/sh.
        request.metadata_mut().insert(
            "authorization",
            tonic::metadata::MetadataValue::from_static("Basic cm9vdDo="),
        );
        let mut stream = client.start(request).await?.into_inner();
        let mut stdout = String::new();
        while let Some(response) = stream.message().await? {
            match response.event.and_then(|event| event.event) {
                Some(process_event::Event::Data(data)) => {
                    if let Some(process_event::data_event::Output::Stdout(bytes)) = data.output {
                        stdout.push_str(std::str::from_utf8(&bytes)?);
                    }
                }
                Some(process_event::Event::End(end)) => {
                    anyhow::ensure!(end.exit_code == 0, "guest command failed: {script}");
                    return Ok(stdout);
                }
                _ => {}
            }
        }
        anyhow::bail!("guest process ended without status")
    })
    .await?
}

#[tokio::test]
#[ignore = "requires selected backend assets, KVM and ublk"]
async fn selected_backend_reports_metrics_and_invalidates_dropped_executor() -> Result<()> {
    let (factory, spec) = contract_fixture().await?;
    let ublk = agentenv::sandbox::UblkConfig::overlaybd(spec.image_config_path.clone(), false);
    let mut source = factory.build_template(
        spec,
        SandboxLaunchConfig {
            sandbox_id: SandboxId::new(),
            ..Default::default()
        },
        Some(ublk),
    )?;
    source.start().await?;
    let result = async {
        let sample = source
            .envd()
            .context("backend has no guest metrics")?
            .metrics()
            .await?;
        assert!(sample.cpu_count > 0 && sample.mem_total > 0);
        Ok(())
    }
    .await;
    let executor = source.executor()?;
    let pending = source.envd();
    drop(source);
    assert!(
        executor
            .run_command("/agentenv/bin/busybox", &["true"])
            .await
            .is_err(),
        "dropped runtime executor remained live"
    );
    if let Some(pending) = pending {
        assert!(
            pending.metrics().await.is_err(),
            "dropped runtime metrics remained live"
        );
    }
    result
}

#[tokio::test]
#[ignore = "requires selected backend assets, KVM and ublk"]
async fn selected_backend_restores_with_only_tools_shell() -> Result<()> {
    use agentenv::sandbox::SandboxBackendFactory;
    let (factory, spec) = contract_fixture().await?;
    let sandbox_id = SandboxId::new();
    let mut source = factory.build(
        spec,
        SandboxLaunchConfig {
            sandbox_id,
            ..Default::default()
        },
    )?;
    source.start().await?;
    let result = async {
        // Bootstrap supplies this shim for shell-less images. envd itself still
        // launches processes through /bin/sh, so preserve its supported contract.
        contract_command(
            source.as_ref(),
            "/agentenv/bin/busybox ln -sf /agentenv/bin/busybox /bin/sh",
        )
        .await?;
        let state = source.pause(None).await?;
        source.stop().await?;
        let mut restored = factory.build_from_paused_state(sandbox_id, state.as_ref(), None)?;
        let result = async {
            restored.start().await?;
            assert_eq!(
                contract_command(restored.as_ref(), "printf restored").await?,
                "restored"
            );
            Ok(())
        }
        .await;
        let stopped = restored.stop().await;
        result.and(stopped)
    }
    .await;
    let stopped = source.stop().await;
    result.and(stopped)
}

#[tokio::test]
#[ignore = "requires selected backend assets, KVM and ublk"]
async fn selected_backend_volume_capture_is_immutable_and_fork_uses_latest_cut() -> Result<()> {
    use agentenv::sandbox::{ExtraDrive, SandboxBackendFactory, SandboxForkSpec};
    use agentenv::volume::{VolumeManager, VolumeMode};
    let (factory, mut spec) = contract_fixture().await?;
    let root = tempfile::tempdir()?;
    let (_, snapshots, repository) = crate::common::snapshot_test_parts(root.path());
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository.clone())
            .await?;
    let volume = volumes
        .create(
            "contract-source".into(),
            VolumeMode::Exclusive,
            None,
            None,
            128,
        )
        .await?;
    let image = volume
        .backing_image_config
        .clone()
        .context("source backing")?;
    spec.extra_drives.push(ExtraDrive::Overlaybd {
        drive_id: volume.id.clone(),
        image_config_path: image.clone(),
        read_only: false,
        mount_path: "/mnt/data".into(),
        virtual_size: Some(128 << 20),
        sub_path: None,
        snapshot_output_dir: Some(image.parent().unwrap().into()),
        volume: true,
    });
    let source_id = SandboxId::new();
    let publication = agentenv::snapshot::SnapshotPublishMetadata {
        id: SnapshotId::generate(),
        alias: None,
        source: agentenv::snapshot::SnapshotPublishSource::Sandbox {
            source_sandbox_id: source_id.to_string(),
            source_sandbox_metadata: None,
        },
        context: spec.context.clone(),
        startup: None,
        resources: spec.resources,
        runtime_versions: factory.runtime_versions()?.unwrap_or_else(|| {
            let config = agentenv::cfg::ConfigManager::global_config();
            agentenv::snapshot::SnapshotRuntimeVersions::new(
                config
                    .kernel
                    .version
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                config
                    .firecracker
                    .version
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                config.envd.version.clone(),
                config.resolved_tools_version().into(),
            )
        }),
        virtualization_mode: agentenv::virtualization::VirtualizationMode::Kvm,
        image_configs: Default::default(),
        volume_snapshots: vec![],
        custom_extension_params: None,
    };
    let mut source = factory.build(
        spec,
        SandboxLaunchConfig {
            sandbox_id: source_id,
            ..Default::default()
        },
    )?;
    source.start().await?;
    let result = async {
        contract_command(source.as_ref(), "printf first >/mnt/data/value; sync").await?;
        let first = source.snapshot().await?;
        let captured = first.volume_drives().context("capture did not retain volume generation")?;
        assert_eq!(captured.len(), 1);
        let ExtraDrive::Overlaybd { image_config_path: first_path, .. } = &captured[0];
        let first_config = std::fs::read(first_path)?;
        assert_ne!(*first_path, image, "capture aliases mutable volume catalog");
        let child_volume = volumes.create("contract-child".into(), VolumeMode::Exclusive, None, Some(image.clone()), 128).await?;
        contract_command(source.as_ref(), "printf latest >/mnt/data/value; printf latest >/dev/shm/value; printf latest >/tmp/value; sync").await?;
        let second = source.snapshot().await?;
        assert_eq!(std::fs::read(first_path)?, first_config, "later capture changed earlier generation");
        let requests = [SandboxForkSpec {
            sandbox_id: SandboxId::new(), envd_access_token: None,
            replace_drive_ids: vec![(volume.id.clone(), child_volume.id.clone())],
            extra_drives: vec![ExtraDrive::Overlaybd {
                drive_id: child_volume.id, image_config_path: child_volume.backing_image_config.context("child backing")?,
                read_only: false, mount_path: "/mnt/data".into(), virtual_size: Some(128 << 20),
                sub_path: None, snapshot_output_dir: None, volume: true,
            }],
        }];
        let mut child = source.fork(&requests).await?.remove(0)?;
        let observed = contract_command(child.as_ref(), "cat /mnt/data/value /dev/shm/value /tmp/value").await;
        let stopped = child.stop().await;
        assert_eq!(observed?, "latestlatestlatest");
        stopped?;
        drop(second);
        source.stop().await?;
        volumes.delete(&volume.id).await?;
        let record = agentenv::sandbox::publish_captured(&snapshots, publication, first).await?;
        let captured_volume = &record.committed.as_ref().context("published capture")?.volume_snapshots[0];
        let restored_volume: agentenv::volume::VolumeRecord = serde_json::from_value(serde_json::json!({
            "id": format!("vol_{}", uuid::Uuid::now_v7().simple()), "name": "contract-restored",
            "mode": captured_volume.mode, "sizeMb": captured_volume.size_mb, "status": "ready",
            "reservedBySandboxId": null, "backingLayers": captured_volume.layers, "readOnlyMounts": [],
        }))?;
        repository.create_volume(restored_volume.clone()).await?;
        let restored_volume = volumes.materialize_backing(&restored_volume.id).await?;
        let runnable = snapshots.resolve_runnable(record).await?;
        let mut restored = factory.build_from_snapshot(&runnable, SandboxLaunchConfig {
            sandbox_id: SandboxId::new(), snapshot_id: runnable.record().id.to_string(),
            extra_drives: vec![ExtraDrive::Overlaybd {
                drive_id: restored_volume.id, image_config_path: restored_volume.backing_image_config.context("restored backing")?,
                read_only: false, mount_path: "/mnt/data".into(), virtual_size: Some(128 << 20),
                sub_path: None, snapshot_output_dir: None, volume: true,
            }], ..Default::default()
        })?;
        let result = async {
            restored.start().await?;
            assert_eq!(contract_command(restored.as_ref(), "cat /mnt/data/value").await?, "first",
                "publishing capture A after B/source deletion changed its volume generation");
            Ok(())
        }.await;
        let stopped = restored.stop().await;
        result.and(stopped)
    }.await;
    let stopped = source.stop().await;
    result.and(stopped)
}
