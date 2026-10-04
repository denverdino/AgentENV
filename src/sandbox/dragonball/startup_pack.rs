//! Best-effort first-touch recording using a private, held-restore VM.
use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use uvm_ublk_daemon::protocol::PackRecordingState;

use super::{
    capture::{DragonballCaptureArtifacts, DragonballPausedState},
    DragonballRuntimePolicy, DragonballSandboxFactory,
};
use crate::{
    sandbox::{
        startup_pack::{cleanup_pack_files, derive_recording_mem_config, recording_enabled_for},
        CapturedSandboxSnapshot, SandboxBackend, UblkDeviceManager,
    },
    snapshot::{StartupRecording, MEMORY_STARTUP_TRACE_ARTIFACT},
};

#[derive(Clone)]
pub(super) struct RecordingConfig {
    pub(super) memory_config: PathBuf,
    pub(super) trace: PathBuf,
}

pub(crate) fn startup_recording(
    captured: &CapturedSandboxSnapshot,
    lease: Arc<dyn std::any::Any + Send + Sync>,
) -> Option<StartupRecording> {
    let artifacts = captured
        .downcast_artifacts_ref::<Arc<DragonballCaptureArtifacts>>()?
        .clone();
    let policy = artifacts.recording_policy.clone();
    if !recording_enabled_for(&policy.app.snapshot) {
        return None;
    }
    // Publication can fail or be cancelled before a backend registers its
    // continuation. The recorder itself must participate in shutdown drain.
    let guard = crate::snapshot::startup_pack::StartupManifestTaskGuard::try_register()?;
    Some(StartupRecording::spawn_with_capture_lease(
        {
            let artifacts = artifacts.clone();
            async move {
                let _guard = guard;
                record_startup_pack(artifacts, policy).await
            }
        },
        Arc::new((artifacts, lease)),
    ))
}

pub(super) async fn record_startup_pack(
    artifacts: Arc<DragonballCaptureArtifacts>,
    policy: DragonballRuntimePolicy,
) -> Option<PathBuf> {
    if !recording_enabled_for(&policy.app.snapshot) {
        return None;
    }
    // Separate directory per recorder: simultaneous publication attempts cannot
    // overwrite each other's trace/config. The publisher owns the directory
    // through the capture lease until its asynchronous continuation completes.
    let outcome = async {
        let parent = artifacts
            .manifest
            .memory
            .image_config_path
            .parent()
            .context("memory config parent")?;
        let dir = parent.join(format!("startup-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await?;
        let memory_config =
            derive_recording_mem_config(&artifacts.manifest.memory.image_config_path, &dir).await?;
        let config = RecordingConfig {
            memory_config,
            trace: dir.join(MEMORY_STARTUP_TRACE_ARTIFACT),
        };
        let result = boot_and_wait(artifacts, policy, config.clone()).await;
        if result.is_ok() {
            tokio::fs::remove_file(&config.memory_config).await?;
        } else {
            cleanup_pack_files(&config.trace, &config.memory_config).await;
        }
        result?;
        Ok::<_, anyhow::Error>(config.trace)
    }
    .await;
    match outcome {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(%error, "Dragonball startup recording failed; snapshot remains runnable");
            None
        }
    }
}

async fn boot_and_wait(
    artifacts: Arc<DragonballCaptureArtifacts>,
    policy: DragonballRuntimePolicy,
    config: RecordingConfig,
) -> Result<()> {
    let budget = Duration::from_secs(policy.app.snapshot.memory_startup_pack.record_budget_secs);
    let state = DragonballPausedState {
        artifacts: artifacts.clone(),
        control_port: policy.app.tools.control_plane_port,
    };
    let factory = DragonballSandboxFactory::new(policy)?;
    let mut vm = factory.build_paused(
        artifacts.launch.sandbox_id,
        &state,
        artifacts.launch.envd_access_token.clone(),
    )?;
    // A recorder is not a user sandbox lifecycle and must not emit extension hooks.
    vm.factory.policy.extension = None;
    vm.launch.custom_extension_params = None;
    vm.launch.sandbox_id = crate::types::SandboxId::new();
    vm.recording = Some(config);
    // Allocation/LoadHeld is owned to completion, not cancelled by the record
    // budget. Otherwise a detached boot could still read a deleted config.
    let started = vm.start_nowait().await;
    let dev_id = vm
        .runtime
        .as_ref()
        .and_then(|r| r.resources.dedicated_devices.first())
        .map(|d| d.dev_id());
    let outcome = if started.is_ok() {
        let wait = async {
            vm.wait_for_ready().await?;
            let dev_id = dev_id.context("recorder has no dedicated memory device")?;
            loop {
                match UblkDeviceManager::global()
                    .pack_recording_status(dev_id)
                    .await?
                {
                    PackRecordingState::Done { pages, .. } => {
                        anyhow::ensure!(pages > 0, "empty first-touch recording");
                        return Ok(());
                    }
                    PackRecordingState::Failed { reason } => {
                        anyhow::bail!("recording failed: {reason}")
                    }
                    PackRecordingState::Recording => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                }
            }
        };
        tokio::select! {
            result = tokio::time::timeout(budget, wait) => result.context("startup recording budget exceeded").and_then(|r| r),
            _ = crate::snapshot::startup_pack::startup_manifest_abort_notify() => Err(anyhow::anyhow!("node shutting down")),
        }
    } else {
        started
    };
    if outcome.is_err() {
        if let Some(dev_id) = dev_id {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                UblkDeviceManager::global().abort_pack_recording(dev_id),
            )
            .await;
        }
    }
    // Never truncate teardown, even on shutdown: reap before deleting the
    // dedicated device; the outer capture lease stays alive throughout.
    let stopped = vm.stop().await;
    outcome.and(stopped)
}
