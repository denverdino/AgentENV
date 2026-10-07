//! Node-local paused state. This is not the portable published snapshot format.
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    capture::{DragonballCaptureArtifacts, DragonballPausedState},
    DragonballSandboxFactory,
};
use crate::{
    sandbox::{
        CustomExtensionParams, ExtraDrive, FreshSandboxBuildSpec, SandboxLaunchConfig,
        SandboxNetworkPolicy, SandboxSnapshotManifest,
    },
    snapshot::CommandContext,
    types::{SandboxId, SandboxResources},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PausedSpec {
    image_config_path: PathBuf,
    context: CommandContext,
    resources: SandboxResources,
    extra_drives: Vec<ExtraDrive>,
    extra_boot_args: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PausedLaunch {
    sandbox_id: SandboxId,
    snapshot_id: String,
    env_vars: Option<HashMap<String, String>>,
    network: Option<SandboxNetworkPolicy>,
    extra_mmds: Map<String, Value>,
    extra_drives_in_snapshot: bool,
    custom_extension_params: Option<CustomExtensionParams>,
    // Runtime-only envd credentials are intentionally not part of this DTO.
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PausedRecord {
    version: u16,
    manifest: SandboxSnapshotManifest,
    vm_state_path: PathBuf,
    memory_image_config_path: PathBuf,
    spec: PausedSpec,
    launch: PausedLaunch,
    control_port: u16,
}

pub(super) fn encode(state: &DragonballPausedState) -> Result<Value> {
    let artifacts = &state.artifacts;
    let spec = &artifacts.spec;
    let launch = &artifacts.launch;
    ensure!(
        launch.extra_drives.is_empty(),
        "unmerged paused launch drives"
    );
    Ok(serde_json::to_value(PausedRecord {
        version: 1,
        manifest: artifacts.manifest.clone(),
        vm_state_path: artifacts.manifest.vm_state.path.clone(),
        memory_image_config_path: artifacts.manifest.memory.image_config_path.clone(),
        spec: PausedSpec {
            image_config_path: spec.image_config_path.clone(),
            context: spec.context.clone(),
            resources: spec.resources,
            extra_drives: spec.extra_drives.clone(),
            extra_boot_args: spec.extra_boot_args.clone(),
        },
        launch: PausedLaunch {
            sandbox_id: launch.sandbox_id,
            snapshot_id: launch.snapshot_id.clone(),
            env_vars: launch.env_vars.clone(),
            network: launch.network.clone(),
            extra_mmds: launch.extra_mmds.clone(),
            extra_drives_in_snapshot: launch.extra_drives_in_snapshot,
            custom_extension_params: launch.custom_extension_params.clone(),
        },
        control_port: state.control_port,
    })?)
}

fn owned_file(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.starts_with(root)
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
        "paused artifact escapes allocated root"
    );
    ensure!(
        path.is_file() && path.canonicalize()?.starts_with(root.canonicalize()?),
        "paused artifact resolves outside allocated root"
    );
    Ok(())
}

pub(super) fn decode(
    factory: &DragonballSandboxFactory,
    root: PathBuf,
    state: Value,
) -> Result<Arc<DragonballPausedState>> {
    let saved: PausedRecord =
        serde_json::from_value(state).context("decode Dragonball paused record")?;
    ensure!(
        saved.version == 1 && saved.control_port != 0,
        "unsupported paused schema/control port"
    );
    ensure!(
        root.is_absolute()
            && !root
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
        "invalid allocated paused root"
    );
    for path in [
        &saved.vm_state_path,
        &saved.memory_image_config_path,
        &saved.spec.image_config_path,
    ] {
        owned_file(&root, path)?;
    }
    let envelope = super::snapshot::read_envelope(&saved.vm_state_path)?;
    crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(factory.build.clone())?
        .validate_restore(&envelope.identity)?;
    let mut manifest = saved.manifest;
    ensure!(
        manifest.version == crate::sandbox::manifest::MANIFEST_FORMAT_VERSION
            && manifest.backend == "dragonball"
            && manifest.restore_identity.as_ref() == Some(&envelope.identity)
            && manifest.memory.virtual_size == envelope.memory_layout.image_size
            && saved.spec.resources.cpu_count == u32::from(envelope.machine.vcpu_count)
            && saved.spec.resources.memory_mib == envelope.machine.memory_mib,
        "paused manifest/resource identity conflict"
    );
    manifest.vm_state.path = saved.vm_state_path;
    manifest.memory.image_config_path = saved.memory_image_config_path;
    manifest.rootfs.image_config_path = saved.spec.image_config_path.clone();
    let attached = saved
        .spec
        .extra_drives
        .iter()
        .filter(|drive| !drive.is_volume())
        .collect::<Vec<_>>();
    ensure!(
        attached.len() == manifest.attached_drives.len(),
        "paused attached graph conflict"
    );
    for (descriptor, drive) in manifest.attached_drives.iter_mut().zip(attached) {
        ensure!(
            descriptor.drive_id == drive.drive_id()
                && descriptor.read_only == drive.read_only()
                && Some(descriptor.virtual_size) == drive.virtual_size()
                && descriptor.mount_path == drive.mount_path()
                && descriptor.sub_path.as_deref() == drive.sub_path(),
            "paused attached binding conflict"
        );
        owned_file(&root, drive.image_config_path())?;
        descriptor.image_config_path = drive.image_config_path().into();
    }
    let launch = SandboxLaunchConfig {
        sandbox_id: saved.launch.sandbox_id,
        snapshot_id: saved.launch.snapshot_id,
        env_vars: saved.launch.env_vars,
        network: saved.launch.network,
        extra_mmds: saved.launch.extra_mmds,
        extra_drives_in_snapshot: saved.launch.extra_drives_in_snapshot,
        custom_extension_params: saved.launch.custom_extension_params,
        ..Default::default()
    };
    let state = Arc::new(DragonballPausedState {
        artifacts: Arc::new(DragonballCaptureArtifacts {
            manifest,
            envelope,
            spec: FreshSandboxBuildSpec {
                image_config_path: saved.spec.image_config_path,
                context: saved.spec.context,
                resources: saved.spec.resources,
                extra_drives: saved.spec.extra_drives,
                extra_boot_args: saved.spec.extra_boot_args,
            },
            launch,
            recording_policy: factory.policy.clone(),
            storage: None,
            memory_owner: None,
            runnable_snapshot: None,
        }),
        control_port: saved.control_port,
    });
    // The shared restore preflight validates attached drives and reserved volume
    // slots, including unbound slots absent from extra_drives; no leases allocated.
    factory.build_paused(state.artifacts.launch.sandbox_id, state.as_ref(), None)?;
    Ok(state)
}
