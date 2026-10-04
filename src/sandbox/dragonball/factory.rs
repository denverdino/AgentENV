use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, RwLock},
};

use agentenv_vmm_protocol::runtime::{
    RuntimeBuildIdentity, QUALIFIED_DRAGONBALL_REVISION, RUNTIME_FEATURE_PROFILE,
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

use super::{config::DragonballRuntimePolicy, sandbox::DragonballSandbox};
use crate::{
    sandbox::{
        EnvdAccessToken, FreshSandboxBuildSpec, PausedSandboxState, SandboxBackend,
        SandboxBackendFactory, SandboxLaunchConfig,
    },
    snapshot::RunnableSnapshot,
    types::SandboxId,
};

#[derive(Clone)]
pub struct DragonballSandboxFactory {
    pub(super) policy: DragonballRuntimePolicy,
    pub(super) build: RuntimeBuildIdentity,
    cpu_config: Option<Arc<RwLock<Option<String>>>>,
}

#[cfg(all(test, not(feature = "hypervisor-firecracker")))]
pub(crate) fn factory_for_tests() -> DragonballSandboxFactory {
    let app = crate::cfg::AppConfig::default();
    DragonballSandboxFactory {
        policy: DragonballRuntimePolicy {
            runner: "/nonexistent/metadata-only-runner".into(),
            kernel: "/nonexistent/metadata-only-kernel".into(),
            tools: "/nonexistent/metadata-only-tools".into(),
            disk_rate: None,
            memory_mode: super::DragonballMemoryMode::SharedUblkIncremental,
            extension: None,
            app: Arc::new(app),
        },
        build: RuntimeBuildIdentity {
            runner_build_id: "metadata-only-test".into(),
            dragonball_revision: QUALIFIED_DRAGONBALL_REVISION.into(),
            snapshot_format_epoch: 2,
            restore_abi_id: "packed-ram-4k-v2".into(),
            feature_profile: RUNTIME_FEATURE_PROFILE.into(),
        },
        cpu_config: None,
    }
}

impl DragonballSandboxFactory {
    fn build_snapshot(
        &self,
        snapshot: &RunnableSnapshot,
        mut launch: SandboxLaunchConfig,
        inherit_extensions: bool,
    ) -> Result<DragonballSandbox> {
        // Read the qualified portable state before constructing or allocating any runtime.
        let reader =
            crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())?;
        let envelope =
            super::snapshot::read_for_restore(&reader, snapshot.record(), snapshot.manifest())?;
        let committed = snapshot.committed();
        let manifest = snapshot.manifest();
        ensure!(
            if manifest.volume_slots.is_empty() {
                launch.extra_drives.len() == manifest.volume_devices.len()
            } else {
                launch.extra_drives.len() <= manifest.volume_slots.len()
            },
            "volume binding count conflict"
        );
        let mut bindings = manifest
            .extra_drives()
            .into_iter()
            .map(|drive| (drive.drive_id().to_owned(), drive))
            .collect::<std::collections::HashMap<_, _>>();
        let mut volumes = std::mem::take(&mut launch.extra_drives);
        for saved in &manifest.volume_devices {
            let index = volumes
                .iter()
                .position(|drive| drive.mount_path() == saved.mount_path)
                .context("missing volume mount binding")?;
            let drive = volumes[index].clone();
            ensure!(
                drive.is_volume()
                    && drive.read_only() == saved.read_only
                    && drive.virtual_size() == Some(saved.virtual_size)
                    && drive.sub_path() == saved.sub_path.as_deref(),
                "volume binding topology conflict"
            );
            if manifest.volume_slots.is_empty() {
                volumes.remove(index);
                ensure!(
                    bindings.insert(saved.drive_id.clone(), drive).is_none(),
                    "duplicate physical binding"
                );
            }
        }
        let mut extra_drives = envelope
            .machine
            .block_devices
            .iter()
            .skip(2)
            .filter(|disk| !disk.is_volume_slot)
            .map(|disk| {
                bindings
                    .remove(&disk.drive_id)
                    .context("missing captured logical binding")
            })
            .collect::<Result<Vec<_>>>()?;
        if !manifest.volume_slots.is_empty() {
            let (_, ordered) =
                super::slots::bind(&manifest.volume_slots, std::mem::take(&mut volumes))?;
            extra_drives.extend(ordered);
        }
        ensure!(
            bindings.is_empty() && volumes.is_empty(),
            "unused physical binding"
        );
        launch.custom_extension_params = if inherit_extensions {
            launch
                .custom_extension_params
                .or_else(|| committed.custom_extension_params.clone())
        } else {
            None
        };
        let spec = FreshSandboxBuildSpec {
            image_config_path: snapshot.manifest().rootfs.image_config_path.clone(),
            context: committed.context.clone(),
            resources: *snapshot.resources(),
            extra_drives,
            extra_boot_args: None,
        };
        let state = super::capture::DragonballPausedState {
            artifacts: Arc::new(super::capture::DragonballCaptureArtifacts {
                manifest: snapshot.manifest().clone(),
                envelope,
                spec,
                launch: launch.clone(),
                recording_policy: self.policy.clone(),
                storage: None,
                runnable_snapshot: Some(snapshot.clone()),
                _memory_base: None,
            }),
            control_port: self.policy.app.tools.control_plane_port,
        };
        self.build_paused(launch.sandbox_id, &state, launch.envd_access_token)
    }
    pub fn new(policy: DragonballRuntimePolicy) -> Result<Self> {
        // This query creates no VM, network namespace, or managed device lease.
        let output = Command::new(&policy.runner)
            .args(["--runtime", "--version-json"])
            .env_clear()
            .output()
            .context("query Dragonball reader build")?;
        ensure!(
            output.status.success() && output.stdout.len() <= 4096,
            "Dragonball reader build query failed"
        );
        let build: RuntimeBuildIdentity = serde_json::from_slice(&output.stdout)?;
        crate::sandbox::restore_identity::RestoreReader::from_build(build.clone())?;
        ensure!(
            build.dragonball_revision == QUALIFIED_DRAGONBALL_REVISION
                && build.feature_profile == RUNTIME_FEATURE_PROFILE
                && build.snapshot_format_epoch == 2,
            "unqualified Dragonball reader build"
        );
        Ok(Self {
            policy,
            build,
            cpu_config: None,
        })
    }

    pub fn with_cpu_config(
        policy: DragonballRuntimePolicy,
        cpu: Arc<RwLock<Option<String>>>,
    ) -> Result<Self> {
        let mut factory = Self::new(policy)?;
        factory.cpu_config = Some(cpu);
        factory.validate_cpu_policy()?;
        Ok(factory)
    }

    pub(super) fn validate_cpu_policy(&self) -> Result<()> {
        ensure!(
            self.cpu_config
                .as_ref()
                .is_none_or(|cpu| cpu.read().unwrap().is_none()),
            "Dragonball does not support Firecracker cluster CPU templates"
        );
        Ok(())
    }

    pub fn build_fresh(
        &self,
        spec: FreshSandboxBuildSpec,
        launch: SandboxLaunchConfig,
    ) -> Result<DragonballSandbox> {
        self.validate_cpu_policy()?;
        let mut sandbox = DragonballSandbox::new(self.clone(), spec, launch)?;
        sandbox
            .spec
            .extra_drives
            .sort_by_key(|drive| drive.is_volume());
        let attached = sandbox
            .spec
            .extra_drives
            .iter()
            .filter(|drive| !drive.is_volume())
            .count();
        ensure!(
            sandbox.spec.extra_drives.len() - attached
                <= self.policy.app.volume.max_volume_count.min(24 - attached),
            "volume count exceeds reserved slot budget"
        );
        Ok(sandbox)
    }

    pub fn build_paused(
        &self,
        id: SandboxId,
        state: &dyn PausedSandboxState,
        token: Option<EnvdAccessToken>,
    ) -> Result<DragonballSandbox> {
        self.validate_cpu_policy()?;
        let saved = state
            .downcast_ref::<super::capture::DragonballPausedState>()
            .context("not a Dragonball paused state")?;
        let reader =
            crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())?;
        let envelope = super::snapshot::read_envelope(&saved.artifacts.manifest.vm_state.path)?;
        reader.validate_restore(&envelope.identity)?;
        ensure!(
            envelope == saved.artifacts.envelope
                && saved.artifacts.manifest.restore_identity.as_ref() == Some(&envelope.identity)
                && saved.artifacts.manifest.backend == "dragonball"
                && id == saved.artifacts.launch.sandbox_id,
            "paused identity/manifest conflict"
        );
        ensure!(
            self.policy.app.resolved_kernel_version() == envelope.identity.kernel_version,
            "captured kernel version is not available in the current runtime policy"
        );
        let mut factory = self.clone();
        if factory.policy.app.resolved_tools_version() != envelope.identity.tools_drive_version {
            factory.policy.tools = factory
                .policy
                .app
                .resolved_tools_drive_path_for_version(&envelope.identity.tools_drive_version)?;
        }
        ensure!(
            factory.policy.tools.is_file(),
            "captured tools version is unavailable"
        );
        let mut launch = saved.artifacts.launch.clone();
        launch.envd_access_token = token;
        let mut sandbox = DragonballSandbox::new(factory, saved.artifacts.spec.clone(), launch)?;
        sandbox.restore = Some(Arc::new(saved.clone()));
        Ok(sandbox)
    }

    pub(super) async fn build_fork(
        &self,
        artifacts: Arc<super::capture::DragonballCaptureArtifacts>,
        child: crate::sandbox::SandboxForkSpec,
    ) -> Result<DragonballSandbox> {
        let reader =
            crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())?;
        reader.validate_restore(&artifacts.envelope.identity)?;
        ensure!(
            super::snapshot::read_envelope(&artifacts.manifest.vm_state.path)?
                == artifacts.envelope,
            "captured envelope changed before fork"
        );
        let mut spec = artifacts.spec.clone();
        let mut replaced = std::collections::HashSet::new();
        let mut replacement_ids = std::collections::HashSet::new();
        for (old_id, new_id) in &child.replace_drive_ids {
            ensure!(
                replaced.insert(old_id) && replacement_ids.insert(new_id),
                "duplicate volume replacement"
            );
            let captured = spec
                .extra_drives
                .iter_mut()
                .find(|drive| drive.drive_id() == old_id)
                .context("replacement is not a captured device")?;
            ensure!(
                captured.is_volume(),
                "cannot replace captured attached disk"
            );
            let replacement = child
                .extra_drives
                .iter()
                .find(|drive| drive.drive_id() == new_id)
                .context("replacement volume was not supplied")?;
            ensure!(
                replacement.is_volume()
                    && replacement.virtual_size() == captured.virtual_size()
                    && replacement.read_only() == captured.read_only()
                    && replacement.mount_path() == captured.mount_path()
                    && replacement.sub_path() == captured.sub_path(),
                "replacement changes captured capacity/features/mount topology"
            );
            // Allocation supplies identity/ownership, never the consistency cut.
            // Adopt the actual capture's immutable lowers into the child catalog
            // directory before its runtime opens a private writable upper.
            let target = replacement.image_config_path();
            overlaybd::config::load_image_config(target)
                .context("load allocated child volume backing")?;
            let source = captured.image_config_path();
            ensure!(
                std::fs::canonicalize(source)? != std::fs::canonicalize(target)?,
                "child volume aliases captured backing"
            );
            let source_root =
                std::fs::canonicalize(source.parent().context("captured volume parent")?)?;
            let staging = target
                .parent()
                .context("child volume parent")?
                .join(format!("fork-{}", uuid::Uuid::new_v4()));
            let config =
                crate::sandbox::storage_snapshot::stage_overlaybd_snapshot_from_live_runtime(
                    source,
                    &staging,
                    None,
                    &[source_root],
                )
                .await?;
            crate::sandbox::storage_snapshot::write_bytes_atomically(
                target,
                &tokio::fs::read(config).await?,
                "fork volume backing",
            )?;
            *captured = replacement.clone();
        }
        for drive in &child.extra_drives {
            let bound = spec
                .extra_drives
                .iter_mut()
                .find(|bound| bound.drive_id() == drive.drive_id())
                .context("child drive is outside captured topology")?;
            ensure!(
                bound.is_volume()
                    && drive.is_volume()
                    && bound.virtual_size() == drive.virtual_size()
                    && bound.read_only() == drive.read_only()
                    && bound.mount_path() == drive.mount_path()
                    && bound.sub_path() == drive.sub_path(),
                "child override changes captured volume topology"
            );
            // Replacements above already carry the adopted generation. Shared RO
            // volumes retain this capture's leased copy, not a mutable catalog path.
            if !replacement_ids.contains(&drive.drive_id().to_string()) {
                *bound = drive.with_image_config_path(bound.image_config_path().to_path_buf());
            }
        }
        for drive in &artifacts.spec.extra_drives {
            ensure!(
                !drive.is_volume()
                    || drive.read_only()
                    || replaced.contains(&drive.drive_id().to_string()),
                "writable volume requires an independently owned child replacement"
            );
        }
        let mut launch = artifacts.launch.clone();
        launch.sandbox_id = child.sandbox_id;
        launch.envd_access_token = child.envd_access_token;
        let mut sandbox = DragonballSandbox::new(self.clone(), spec, launch)?;
        sandbox.restore = Some(Arc::new(super::capture::DragonballPausedState {
            artifacts,
            control_port: self.policy.app.tools.control_plane_port,
        }));
        Ok(sandbox)
    }
}

impl SandboxBackendFactory for DragonballSandboxFactory {
    fn backend(&self) -> &str {
        "dragonball"
    }

    fn validate_snapshot(&self, snapshot: &RunnableSnapshot) -> Result<()> {
        let reader =
            crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())?;
        super::snapshot::read_for_restore(&reader, snapshot.record(), snapshot.manifest())?;
        Ok(())
    }

    fn supports_restore(
        &self,
        identity: &crate::sandbox::restore_identity::SnapshotRestoreIdentity,
    ) -> bool {
        crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())
            .is_ok_and(|reader| reader.validate_restore(identity).is_ok())
    }

    fn runtime_versions(&self) -> Result<Option<crate::snapshot::SnapshotRuntimeVersions>> {
        let reader =
            crate::sandbox::restore_identity::RestoreReader::from_build(self.build.clone())?;
        Ok(Some(
            crate::snapshot::SnapshotRuntimeVersions::from_dragonball(reader.identity(
                self.policy.app.resolved_kernel_version().into(),
                self.policy.app.resolved_tools_version().into(),
                self.policy.app.envd.version.clone(),
            ))?,
        ))
    }

    fn build(
        &self,
        spec: FreshSandboxBuildSpec,
        launch: SandboxLaunchConfig,
    ) -> Result<Box<dyn SandboxBackend>> {
        Ok(Box::new(self.build_fresh(spec, launch)?))
    }
    fn build_from_snapshot(
        &self,
        snapshot: &RunnableSnapshot,
        launch: SandboxLaunchConfig,
    ) -> Result<Box<dyn SandboxBackend>> {
        Ok(Box::new(self.build_snapshot(snapshot, launch, true)?))
    }
    fn decode_paused_state(
        &self,
        root: PathBuf,
        state: Value,
    ) -> Result<Arc<dyn PausedSandboxState>> {
        Ok(super::paused::decode(self, root, state)?)
    }
    fn build_from_paused_state(
        &self,
        id: SandboxId,
        state: &dyn PausedSandboxState,
        token: Option<EnvdAccessToken>,
    ) -> Result<Box<dyn SandboxBackend>> {
        Ok(Box::new(self.build_paused(id, state, token)?))
    }
}

#[async_trait::async_trait(?Send)]
impl crate::sandbox::TemplateSandboxFactory for DragonballSandboxFactory {
    type Sandbox = DragonballSandbox;

    fn build_template(
        &self,
        spec: FreshSandboxBuildSpec,
        mut launch: SandboxLaunchConfig,
        _ublk: Option<crate::sandbox::UblkConfig>,
    ) -> Result<Self::Sandbox> {
        let mut factory = self.clone();
        factory.policy.extension = None;
        launch.custom_extension_params = None;
        factory.build_fresh(spec, launch)
    }

    fn build_template_from_snapshot(
        &self,
        snapshot: &RunnableSnapshot,
        launch: SandboxLaunchConfig,
    ) -> Result<Self::Sandbox> {
        let mut sandbox = self.build_snapshot(snapshot, launch, false)?;
        sandbox.factory.policy.extension = None;
        Ok(sandbox)
    }

    async fn template_versions(
        &self,
        sandbox: &Self::Sandbox,
        _tools_version: String,
    ) -> Result<crate::snapshot::SnapshotRuntimeVersions> {
        let runtime = sandbox
            .runtime
            .as_ref()
            .context("template sandbox not started")?;
        crate::snapshot::SnapshotRuntimeVersions::from_dragonball(runtime.machine.identity.clone())
    }
}

#[cfg(all(test, not(feature = "hypervisor-firecracker")))]
mod component_tests {
    use super::*;

    #[test]
    fn every_constructor_rechecks_updated_cpu_policy() {
        let mut factory = factory_for_tests();
        let cpu = Arc::new(RwLock::new(None));
        factory.cpu_config = Some(cpu.clone());
        factory.validate_cpu_policy().unwrap();
        *cpu.write().unwrap() = Some("{}".into());
        // Fork construction calls new directly, bypassing build_fresh/paused.
        // Reject policy changes before inspecting or allocating runtime assets.
        let result = DragonballSandbox::new(
            factory,
            FreshSandboxBuildSpec {
                image_config_path: "/nonexistent/cpu-policy-fixture.json".into(),
                context: crate::snapshot::CommandContext::new(Default::default(), "/"),
                resources: crate::types::SandboxResources::default(),
                extra_drives: vec![],
                extra_boot_args: None,
            },
            SandboxLaunchConfig::default(),
        );
        let error = match result {
            Ok(_) => panic!("updated CPU policy was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("CPU templates"), "{error}");
    }
}
