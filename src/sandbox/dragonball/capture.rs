use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use agentenv_vmm_protocol::runtime::*;
use anyhow::{ensure, Context, Result};
use overlaybd::{virtual_file::VirtualFile, ImageService};
use serde::Serialize;

use super::sandbox::{DragonballSandbox, RuntimeStarted};
use crate::sandbox::ublk::OverlaybdCompactOutput;
use crate::sandbox::{
    storage_snapshot::{
        canonicalized_runtime_owned_roots, managed_snapshot_base,
        restack_snapshot_overlaybd_device_with_runtime_roots, PersistentSnapshotRootGuard,
        StorageCaptureOwner,
    },
    FreshSandboxBuildSpec, PausedSandboxState, RuntimeArtifactSet, SandboxCaptureError,
    SandboxCaptureResult, SandboxLaunchConfig, SandboxSnapshotManifest,
};

#[derive(Debug)]
pub(super) struct DragonballCaptureArtifacts {
    pub(super) manifest: SandboxSnapshotManifest,
    pub(super) envelope: RuntimeSnapshotEnvelope,
    pub(super) spec: FreshSandboxBuildSpec,
    pub(super) launch: SandboxLaunchConfig,
    pub(super) recording_policy: super::DragonballRuntimePolicy,
    pub(super) storage: Option<StorageCaptureOwner>,
    // Source disks retain the outer capture root, but not capture-only RAM/state.
    pub(super) memory_owner: Option<Arc<PersistentSnapshotRootGuard>>,
    // Preserve resolved cache pins across a derivative capture/fork and source teardown.
    pub(super) runnable_snapshot: Option<crate::snapshot::RunnableSnapshot>,
}

#[derive(Clone, Debug)]
pub(super) struct DragonballPausedState {
    pub(super) artifacts: Arc<DragonballCaptureArtifacts>,
    pub(super) control_port: u16,
}

impl PausedSandboxState for DragonballPausedState {
    fn encode(&self) -> Result<serde_json::Value> {
        super::paused::encode(self)
    }
    fn persistence_committed(&self) {
        if let Some(storage) = &self.artifacts.storage {
            storage.snapshot_root.persistence_committed();
        }
        if let Some(memory) = &self.artifacts.memory_owner {
            memory.persistence_committed();
        }
    }

    fn owns_artifact_cleanup(&self) -> bool {
        self.artifacts.storage.is_some()
    }
    fn runtime_artifacts(&self) -> RuntimeArtifactSet {
        let manifest = &self.artifacts.manifest;
        let mut paths = vec![
            manifest.memory.image_config_path.clone(),
            manifest.rootfs.image_config_path.clone(),
        ];
        paths.extend(
            self.artifacts
                .spec
                .extra_drives
                .iter()
                .map(|drive| drive.image_config_path().into()),
        );
        RuntimeArtifactSet::from_overlaybd_image_configs(paths)
    }
    fn control_plane_port(&self) -> Option<u16> {
        Some(self.control_port)
    }
}

pub(super) fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    Ok(())
}

pub(super) async fn materialize_memory(
    config: &Path,
    size: u64,
    global: &Path,
    output: &Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let service = ImageService::from_config_path(global).await?;
    let image = service.create_image_file(config).await?;
    ensure!(
        image.size().await? == size,
        "memory image capacity conflict"
    );
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)
        .await?;
    let mut offset = 0;
    while offset < size {
        let count = (size - offset).min(4 * 1024 * 1024) as usize;
        let bytes = image.read_at(offset, count).await?;
        ensure!(bytes.len() == count, "short memory materialization read");
        file.write_all(&bytes).await?;
        offset += count as u64;
    }
    // Full RAM writes/fsync must not stall sibling control connections on the
    // same executor; their bounded frame deadlines keep running in the runner.
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

impl DragonballSandbox {
    pub(super) async fn capture_paused(
        &mut self,
        root: Option<&Path>,
    ) -> SandboxCaptureResult<Arc<DragonballPausedState>> {
        let artifacts = self
            .capture_owned(None, root.map(Path::to_path_buf), true)
            .await?;
        Ok(Arc::new(DragonballPausedState {
            artifacts,
            control_port: self.factory.policy.app.tools.control_plane_port,
        }))
    }

    pub(super) async fn capture_owned(
        &mut self,
        at: Option<PathBuf>,
        root: Option<PathBuf>,
        keep_held: bool,
    ) -> SandboxCaptureResult<Arc<DragonballCaptureArtifacts>> {
        let runtime = self.runtime.take().ok_or_else(|| {
            SandboxCaptureError::terminal(anyhow::anyhow!("Dragonball sandbox not running"))
        })?;
        let spec = self.spec.clone();
        let launch = self.launch.clone();
        let recording_policy = self.factory.policy.clone();
        let task = tokio::spawn(async move {
            let mut runtime = runtime;
            let mut captured: SandboxCaptureResult<Arc<DragonballCaptureArtifacts>> = async {
                let (at, guard) = if let Some(at) = at {
                    (at, None)
                } else {
                    let base = root.unwrap_or_else(|| managed_snapshot_base().join("dragonball"));
                    let guard = Arc::new(PersistentSnapshotRootGuard::new(
                        base.join(uuid::Uuid::new_v4().to_string()),
                    ));
                    guard.prepare().await?;
                    (guard.path().join("capture"), Some(guard))
                };
                let artifacts = runtime
                    .capture_held(&spec, &launch, &at, guard, recording_policy, keep_held)
                    .await?;
                if !keep_held {
                    runtime
                        .resume_capture()
                        .await
                        .map_err(SandboxCaptureError::terminal)?;
                }
                Ok(Arc::new(artifacts))
            }
            .await;
            if captured.as_ref().is_err_and(|error| !error.is_terminal()) {
                if let Err(error) = runtime.confirm_running().await {
                    captured = Err(SandboxCaptureError::terminal(
                        error.context("recoverable capture did not confirm Running"),
                    ));
                }
            }
            (runtime, captured)
        });
        let (runtime, result) = task
            .await
            .map_err(|error| SandboxCaptureError::terminal(error.into()))?;
        self.runtime = Some(runtime);
        if result.as_ref().is_err_and(SandboxCaptureError::is_terminal) {
            if let Err(error) = self.stop_runtime(true).await {
                tracing::error!(%error, "terminal Dragonball capture teardown not confirmed; leases remain owned");
            }
        }
        result
    }

    pub(super) async fn resume_capture_owned(&mut self) -> Result<()> {
        let mut runtime = self
            .runtime
            .take()
            .context("Dragonball sandbox not running")?;
        let task = tokio::spawn(async move {
            let result = runtime.resume_capture().await;
            (runtime, result)
        });
        let (runtime, result) = task.await?;
        self.runtime = Some(runtime);
        if result.is_err() {
            self.stop_runtime(true).await?;
        }
        result
    }
}

impl RuntimeStarted {
    pub(super) async fn begin_capture(&self) -> SandboxCaptureResult<CaptureReport> {
        self.ready.store(false, Ordering::Release);
        let result = self.client.lock().await.begin_capture().await;
        if result.as_ref().is_err_and(|error| !error.is_terminal()) {
            self.ready.store(true, Ordering::Release);
        }
        result
    }

    pub(super) async fn confirm_running(&self) -> Result<()> {
        let reply = self
            .client
            .lock()
            .await
            .request(RuntimeCommand::Status, Duration::from_secs(5))
            .await?;
        ensure!(
            matches!(
                reply,
                RuntimeReply::Status(RuntimeStatus {
                    phase: RuntimePhase::Running,
                    capture: None,
                    ..
                })
            ),
            "source Running not confirmed"
        );
        Ok(())
    }

    async fn resume_capture(&mut self) -> Result<()> {
        let reply = self
            .client
            .lock()
            .await
            .request(RuntimeCommand::Status, Duration::from_secs(5))
            .await?;
        match reply {
            RuntimeReply::Status(status) if status.phase == RuntimePhase::Running => {
                ensure!(
                    self.ready.load(Ordering::Acquire),
                    "Running source was never Ready"
                );
                Ok(())
            }
            RuntimeReply::Status(status) if status.phase == RuntimePhase::Held => {
                let token = status
                    .capture
                    .context("held source has no capture proof")?
                    .token;
                self.release_source(token).await
            }
            _ => anyhow::bail!("source is not Running or Held"),
        }
    }

    pub(super) async fn release_source(&self, token: CaptureToken) -> Result<()> {
        self.client
            .lock()
            .await
            .request(
                RuntimeCommand::EndCapture { token },
                Duration::from_secs(30),
            )
            .await?;
        self.confirm_running().await?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn capture_held(
        &mut self,
        spec: &FreshSandboxBuildSpec,
        launch: &SandboxLaunchConfig,
        at: &Path,
        root_guard: Option<Arc<PersistentSnapshotRootGuard>>,
        recording_policy: super::DragonballRuntimePolicy,
        sync_persistent_volumes: bool,
    ) -> SandboxCaptureResult<DragonballCaptureArtifacts> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(SandboxCaptureError::terminal(anyhow::anyhow!(
                "capture requires a Ready source"
            )));
        }
        (|| -> Result<()> {
            ensure!(
                at.is_absolute()
                    && !at
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir)),
                "capture output must be absolute and nonescaping"
            );
            fs::create_dir_all(at)?;
            ensure!(
                fs::read_dir(at)?.next().is_none(),
                "capture output must be empty; never replace durable files"
            );
            Ok(())
        })()?;
        if sync_persistent_volumes
            && spec
                .extra_drives
                .iter()
                .any(|drive| !drive.read_only() && drive.snapshot_output_dir().is_some())
        {
            // Paused deletion publishes disk backings without resuming guest
            // RAM, so completed writes must reach the devices before the hold.
            self.sync_writable_volume_filesystems().await?;
        }
        let started = std::time::Instant::now();
        let report = self.begin_capture().await?;
        let held_us = started.elapsed().as_micros() as u64;
        let token = report.token.clone();
        let mut storage_may_have_changed = false;
        let captured: Result<DragonballCaptureArtifacts> = async {
            let memory_owner = if root_guard.is_some() {
                let guard = Arc::new(PersistentSnapshotRootGuard::new(at.join("memory")));
                guard.prepare().await?;
                Some(guard)
            } else {
                None
            };
            let memory_dir = memory_owner.as_ref().map_or(at, |owner| owner.path());
            let export = tempfile::Builder::new()
                .prefix("export-")
                .tempdir_in(self.resources.work.path())?;
            let state = export.path().join("state.json");
            self.client
                .lock()
                .await
                .request(
                    RuntimeCommand::ExportHeldState {
                        token: token.clone(),
                        state_path: state.clone(),
                    },
                    Duration::from_secs(60),
                )
                .await?;
            let envelope = super::snapshot::read_envelope(&state)?;
            ensure!(
                envelope.identity == self.machine.identity
                    && envelope.memory_layout == report.memory_layout,
                "captured identity/layout conflict"
            );
            write_new_json(&memory_dir.join("vm-state.json"), &envelope)?;
            let exported_us = started.elapsed().as_micros() as u64;
            let reader = super::memory_reader::FrozenMemoryReadLease::acquire(
                self.client.clone(),
                self.resources
                    .process
                    .as_ref()
                    .context("runner owner missing")?
                    .child
                    .clone(),
                &report,
                self.machine.identity.memory_layout_id.clone(),
            )
            .await?;
            let mapped_us = started.elapsed().as_micros() as u64;
            let memory_config_path = super::memory::write_incremental_memory(
                reader.map(),
                reader.reader(),
                self.memory_base.as_deref(),
                memory_dir,
                OverlaybdCompactOutput::Raw,
            )
            .await;
            // Even an OverlayBD write failure must drain all reads before resume.
            let written_us = started.elapsed().as_micros() as u64;
            reader.release().await?;
            let released_us = started.elapsed().as_micros() as u64;
            let memory_config_path = memory_config_path?;
            let root = self.root.as_ref().context("rootfs runtime missing")?;
            if let Some(guard) = &root_guard {
                // Sealed layers also remain live lowers after restack, independent of publication.
                self.resources.retained_artifacts.push(guard.clone());
            }
            // A lost daemon response can already have sealed the upper. Arm before await.
            storage_may_have_changed = true;
            let mut runtime_roots = canonicalized_runtime_owned_roots().to_vec();
            for artifact in &self.resources.retained_artifacts {
                if let Some(guard) = artifact.downcast_ref::<PersistentSnapshotRootGuard>() {
                    runtime_roots.push(guard.path().to_path_buf());
                } else if let Some(saved) = artifact.downcast_ref::<DragonballCaptureArtifacts>() {
                    if let Some(storage) = &saved.storage {
                        runtime_roots.push(storage.snapshot_dir.clone());
                    } else {
                        // Decoded pauses have no cleanup guard. Their disks may
                        // live outside the default roots, separately from RAM.
                        for config in
                            std::iter::once(saved.manifest.rootfs.image_config_path.as_path())
                                .chain(
                                    saved
                                        .spec
                                        .extra_drives
                                        .iter()
                                        .map(|drive| drive.image_config_path()),
                                )
                        {
                            runtime_roots
                                .push(config.parent().context("saved disk root")?.to_path_buf());
                        }
                    }
                }
            }
            let root_config = restack_snapshot_overlaybd_device_with_runtime_roots(
                &root.device,
                false,
                &root.image_config_path,
                &at.join("rootfs"),
                "snapshot.commit",
                "rootfs",
                &runtime_roots,
            )
            .await?;
            let extras = crate::sandbox::volume_snapshot::capture_extra_drives(
                &spec.extra_drives,
                &self.extras,
                at,
                &runtime_roots,
            )
            .await?;
            tracing::debug!(
                sandbox_id = %launch.sandbox_id,
                held_us, exported_us, mapped_us, written_us, released_us,
                disks_us = started.elapsed().as_micros() as u64,
                "Dragonball capture cumulative timing"
            );
            let attached = extras
                .iter()
                .filter(|drive| !drive.is_volume())
                .cloned()
                .collect::<Vec<_>>();
            let mut manifest = SandboxSnapshotManifest::new_dragonball(
                envelope.identity.clone(),
                memory_dir.join("vm-state.json"),
                memory_config_path,
                envelope.memory_layout.image_size,
                root_config.clone(),
                root.actual_virtual_size,
                &attached,
            )?;
            manifest.volume_slots = self.volume_slots.clone();
            manifest.volume_drive_slots = self.volume_slots.len();
            manifest.physical_extra_drive_count = if self.volume_slots.is_empty() {
                extras.len()
            } else {
                attached.len()
            };
            manifest.volume_devices = extras
                .iter()
                .zip(&envelope.machine.block_devices[2..])
                .filter(|(drive, _)| drive.is_volume())
                .map(
                    |(drive, saved)| crate::sandbox::manifest::SnapshotVolumeDevice {
                        drive_id: saved.drive_id.clone(),
                        mount_path: drive.mount_path().to_path_buf(),
                        sub_path: drive.sub_path().map(Path::to_path_buf),
                        read_only: drive.read_only(),
                        virtual_size: saved.capacity_bytes,
                    },
                )
                .collect();
            let mut saved_spec = spec.clone();
            saved_spec.image_config_path = root_config;
            saved_spec.extra_drives = extras;
            saved_spec.resources.disk_size_mib = 0;
            Ok(DragonballCaptureArtifacts {
                manifest,
                envelope,
                spec: saved_spec,
                launch: launch.clone(),
                recording_policy,
                storage: root_guard.map(|guard| StorageCaptureOwner::new(at.into(), guard)),
                memory_owner,
                runnable_snapshot: self
                    .resources
                    .retained_artifacts
                    .iter()
                    .find_map(|artifact| {
                        artifact
                            .downcast_ref::<DragonballCaptureArtifacts>()
                            .and_then(|saved| saved.runnable_snapshot.clone())
                    }),
            })
        }
        .await;
        match captured {
            Ok(artifacts) => Ok(artifacts),
            Err(error) if !storage_may_have_changed => {
                if let Err(release) = self.release_source(token).await {
                    return Err(SandboxCaptureError::terminal(
                        error.context(format!("capture release not confirmed: {release:#}")),
                    ));
                }
                Err(SandboxCaptureError::recoverable(error))
            }
            Err(error) => Err(SandboxCaptureError::terminal(error)),
        }
    }
}
