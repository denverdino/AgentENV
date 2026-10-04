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
