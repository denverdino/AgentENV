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
        let reader = crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(
            self.build.clone(),
        )?;
        let envelope =
            super::snapshot::read_for_restore(&reader, snapshot.record(), snapshot.manifest())?;
        let committed = snapshot.committed();
        let manifest = snapshot.manifest();
        // Only automatic clones preserve the filesystem cached in captured guest RAM.
        ensure!(
            manifest.volume_devices.is_empty() || launch.extra_drives_in_snapshot,
            "Dragonball does not support explicit volume replacement in occupied snapshot slots"
        );
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
                memory_owner: None,
                runnable_snapshot: Some(snapshot.clone()),
            }),
            control_port: self.policy.app.tools.control_plane_port,
        };
        let mut sandbox = self.build_paused(launch.sandbox_id, &state, launch.envd_access_token)?;
        // The synthetic paused state carries the new launch, but guest memory
        // still contains the captured instance's identity.
        sandbox.require_guest_identity = true;
        Ok(sandbox)
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
        crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(build.clone())?;
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
        let reader = crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(
            self.build.clone(),
        )?;
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
        sandbox.require_guest_identity = false;
        Ok(sandbox)
    }

    pub(super) async fn build_fork(
        &self,
        artifacts: Arc<super::capture::DragonballCaptureArtifacts>,
        child: crate::sandbox::SandboxForkSpec,
    ) -> Result<DragonballSandbox> {
        let reader = crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(
            self.build.clone(),
        )?;
        reader.validate_restore(&artifacts.envelope.identity)?;
        ensure!(
            super::snapshot::read_envelope(&artifacts.manifest.vm_state.path)?
                == artifacts.envelope,
            "captured envelope changed before fork"
        );
        let mut spec = artifacts.spec.clone();
        spec.extra_drives =
            crate::sandbox::volume_snapshot::bind_fork_drives(&spec.extra_drives, &child).await?;
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
        let reader = crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(
            self.build.clone(),
        )?;
        super::snapshot::read_for_restore(&reader, snapshot.record(), snapshot.manifest())?;
        Ok(())
    }

    fn supports_restore(
        &self,
        identity: &crate::sandbox::restore_identity::SnapshotRestoreIdentity,
    ) -> bool {
        crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(self.build.clone())
            .is_ok_and(|reader| reader.validate_restore(identity).is_ok())
    }

    fn runtime_versions(&self) -> Result<Option<crate::snapshot::SnapshotRuntimeVersions>> {
        let reader = crate::sandbox::dragonball::restore_identity::RestoreReader::from_build(
            self.build.clone(),
        )?;
        crate::snapshot::SnapshotRuntimeVersions::from_dragonball(reader.identity(
            self.policy.app.resolved_kernel_version().into(),
            self.policy.app.resolved_tools_version().into(),
            self.policy.app.envd.version.clone(),
        ))
        .map(Some)
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
    fn volume() -> crate::sandbox::ExtraDrive {
        crate::sandbox::ExtraDrive::Overlaybd {
            drive_id: "replacement_volume".into(),
            image_config_path: "/unused/volume.json".into(),
            read_only: false,
            mount_path: "/mnt/data".into(),
            virtual_size: Some(64 << 20),
            sub_path: None,
            snapshot_output_dir: None,
            volume: true,
        }
    }

    fn snapshot_fixture(
        directory: &std::path::Path,
        occupied: bool,
    ) -> (DragonballSandboxFactory, RunnableSnapshot) {
        use crate::sandbox::manifest::{
            SandboxSnapshotManifest, SnapshotVolumeDevice, SnapshotVolumeSlot,
        };
        use crate::snapshot::{
            CommittedSnapshot, SnapshotRecord, SnapshotRuntimeVersions, SnapshotVolume,
        };
        use agentenv_vmm_protocol::runtime::*;
        use serde_json::json;

        let asset = directory.join("asset");
        std::fs::write(&asset, b"{}").unwrap();
        let mut factory = factory_for_tests();
        factory.policy.tools = asset.clone();
        factory.build.runner_build_id = format!("sha256:{}", "a".repeat(64));
        factory.build.restore_abi_id = agentenv_vmm_protocol::runtime::RESTORE_ABI_ID.into();
        let reader =
            super::super::restore_identity::RestoreReader::from_build(factory.build.clone())
                .unwrap();
        let identity = reader.identity(
            factory.policy.app.resolved_kernel_version().into(),
            factory.policy.app.resolved_tools_version().into(),
            factory.policy.app.envd.version.clone(),
        );
        let mut envelope = RuntimeSnapshotEnvelope {
            envelope_version: 2,
            identity: identity.clone(),
            machine: PortableMachine {
                vcpu_count: 1,
                memory_mib: 128,
                block_devices: vec![
                    LogicalBlockDevice {
                        drive_id: "rootfs".into(),
                        read_only: true,
                        is_root: true,
                        is_volume_slot: false,
                        backing_read_only: None,
                        capacity_bytes: 8192,
                        rate_limiter: None,
                    },
                    LogicalBlockDevice {
                        drive_id: "user_rootfs".into(),
                        read_only: false,
                        is_root: false,
                        is_volume_slot: false,
                        backing_read_only: None,
                        capacity_bytes: 4096,
                        rate_limiter: None,
                    },
                ],
                network_id: "eth0".into(),
            },
            memory_layout: MemoryLayout {
                page_size: 4096,
                image_size: 128 * 1024 * 1024,
                regions: vec![MemoryRegion {
                    guest_base: 0,
                    image_offset: 0,
                    length: 128 * 1024 * 1024,
                }],
            },
            upstream_state: json!({}),
        };
        envelope.machine.block_devices.push(LogicalBlockDevice {
            drive_id: "agentenv_volume_slot_0".into(),
            read_only: false,
            is_root: false,
            is_volume_slot: true,
            backing_read_only: Some(false),
            capacity_bytes: if occupied { 64 << 20 } else { 4096 },
            rate_limiter: None,
        });
        envelope.upstream_state = json!({
            "header":{"format_epoch":1,"producer_version":"test"}, "vm_kvm_state":{}, "capture_cpu":{}, "vcpu_states":[{"id":0}],
            "memory_state":{"regions":[{"guest_addr":0,"file_offset":0,"size":128*1024*1024u64}]},
            "device_states":{
                "balloon":{"devices":[{
                    "config":{"balloon_id":"memory","size_mib":0,"use_shared_irq":false,"use_generic_irq":false,"f_deflate_on_oom":true,"f_reporting":true},
                    "device_info":{"device_info":{"avail_features":(1u64<<32)|(1<<2)|(1<<5),"acked_features":0,"config_space":[0,0,0,0,0,0,0,0]},"num_pages":0,"actual":0},
                    "transport":{"Mmio":{"device_activated":true,"queues":[{},{},{}]}}
                }]},
                "block":{"devices":envelope.machine.block_devices.iter().map(|disk| json!({
                    "config":{"drive_id":disk.drive_id,"device_type":"RawBlock","path_on_host":format!("aenv-block:{}",disk.drive_id),"is_root_device":disk.is_root,"part_uuid":null,"is_read_only":disk.read_only,"is_direct":!(disk.is_root && disk.read_only),"no_drop":false,"sparse":false,"is_volume_slot":disk.is_volume_slot,"backing_read_only":disk.backing_read_only,"num_queues":1,"queue_size":256,"rate_limiter":null,"use_shared_irq":false,"use_generic_irq":false,"use_pci_bus":false},
                    "device_info":{"RawBlock":{"avail_features":block_profile_features(disk.read_only),"acked_features":0,"config_space":block_profile_config_space(disk.capacity_bytes)}},
                    "transport":{"Mmio":{"device_activated":true,"queues":[{}]}}
                })).collect::<Vec<_>>()},
                "virtio_net":{"devices":[{
                    "config":{"num_queues":null,"queue_size":null,"backend":{"type":"virtio","iface_id":"eth0","host_dev_name":"aenv-net:eth0","rx_rate_limiter":null,"tx_rate_limiter":null,"allow_duplicate_mac":false},"guest_mac":null,"use_shared_irq":false,"use_generic_irq":false},
                    "device_info":{"avail_features":NET_PROFILE_FEATURES,"acked_features":0,"config_space":[0,1,2,3,4,5,0,0,0,0,220,5]},
                    "transport":{"Mmio":{"device_activated":true,"queues":[{},{}]}}
                }]}
            }
        });

        let state_path = directory.join("state.json");
        std::fs::write(&state_path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let mut manifest = SandboxSnapshotManifest::new_dragonball(
            identity.clone(),
            state_path,
            directory.join("memory.json"),
            128 << 20,
            asset,
            4096,
            &[],
        )
        .unwrap();
        manifest.volume_drive_slots = 1;
        manifest.volume_slots.push(SnapshotVolumeSlot {
            slot_id: "agentenv_volume_slot_0".into(),
            mount_path: occupied.then(|| "/mnt/data".into()),
            sub_path: None,
            guest_read_only: false,
            capacity_bytes: if occupied { 64 << 20 } else { 4096 },
        });
        let mut committed = CommittedSnapshot::mock();
        committed.virtualization_mode = crate::virtualization::VirtualizationMode::Kvm;
        committed.runtime_versions = SnapshotRuntimeVersions::from_dragonball(identity).unwrap();
        if occupied {
            manifest.volume_devices.push(SnapshotVolumeDevice {
                drive_id: "agentenv_volume_slot_0".into(),
                mount_path: "/mnt/data".into(),
                sub_path: None,
                read_only: false,
                virtual_size: 64 << 20,
            });
            committed.volume_snapshots.push(SnapshotVolume {
                mount_path: "/mnt/data".into(),
                mode: crate::volume::VolumeMode::Exclusive,
                size_mb: 64,
                layers: vec![],
            });
        }
        let record = SnapshotRecord::mock_ready(committed);
        let snapshot = crate::snapshot::mock::runnable_snapshot(record, manifest);
        factory.validate_snapshot(&snapshot).unwrap();
        (factory, snapshot)
    }

    #[test]
    fn explicit_replacement_of_captured_volume_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (factory, snapshot) = snapshot_fixture(directory.path(), true);
        let launch = SandboxLaunchConfig {
            extra_drives: vec![volume()],
            extra_drives_in_snapshot: false,
            ..Default::default()
        };
        let error = match factory.build_snapshot(&snapshot, launch, true) {
            Ok(_) => {
                panic!("unrelated same-geometry volume replaced a captured mounted filesystem")
            }
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("explicit volume replacement"),
            "{error}"
        );
    }

    #[test]
    fn automatic_cloned_captured_volume_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let (factory, snapshot) = snapshot_fixture(directory.path(), true);
        let launch = SandboxLaunchConfig {
            extra_drives: vec![volume()],
            extra_drives_in_snapshot: true,
            ..Default::default()
        };
        let sandbox = factory.build_snapshot(&snapshot, launch, true).unwrap();
        assert_eq!(
            sandbox.spec.extra_drives[0].drive_id(),
            "replacement_volume"
        );
        assert!(sandbox.restore.is_some());
    }

    #[test]
    fn explicit_volume_on_empty_captured_slot_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let (factory, snapshot) = snapshot_fixture(directory.path(), false);
        let launch = SandboxLaunchConfig {
            extra_drives: vec![volume()],
            extra_drives_in_snapshot: false,
            ..Default::default()
        };
        let sandbox = factory.build_snapshot(&snapshot, launch, true).unwrap();
        assert_eq!(
            sandbox.spec.extra_drives[0].drive_id(),
            "replacement_volume"
        );
        assert_eq!(sandbox.spec.extra_drives[0].virtual_size(), Some(64 << 20));
    }
}
