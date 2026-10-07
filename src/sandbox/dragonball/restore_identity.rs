pub use agentenv_vmm_protocol::runtime::SnapshotRestoreIdentity;

use agentenv_vmm_protocol::runtime::{
    RuntimeBuildIdentity, RuntimeHello, RuntimeSnapshotEnvelope, QUALIFIED_DRAGONBALL_REVISION,
    RUNTIME_FEATURE_PROFILE,
};
use anyhow::{ensure, Context, Result};

pub struct RestoreReader {
    build: RuntimeBuildIdentity,
}
impl RestoreReader {
    pub fn new(hello: RuntimeHello) -> Result<Self> {
        ensure!(
            hello.feature_profile == RUNTIME_FEATURE_PROFILE
                && hello.dragonball_revision == QUALIFIED_DRAGONBALL_REVISION
                && !hello.runner_instance_id.is_empty()
                && [
                    "full_memory_capture",
                    "held_restore",
                    "state_only_capture",
                    "leased_memory_export"
                ]
                .iter()
                .all(|required| hello.capabilities.iter().any(|c| c == required)),
            "unqualified runtime reader"
        );
        Self::from_build(RuntimeBuildIdentity {
            runner_build_id: hello.runner_build_id,
            dragonball_revision: hello.dragonball_revision,
            snapshot_format_epoch: hello.snapshot_format_epoch,
            restore_abi_id: hello.restore_abi_id,
            feature_profile: hello.feature_profile,
        })
    }
    pub fn from_build(build: RuntimeBuildIdentity) -> Result<Self> {
        ensure!(
            build.feature_profile == RUNTIME_FEATURE_PROFILE
                && build.dragonball_revision == QUALIFIED_DRAGONBALL_REVISION,
            "unqualified runtime reader build"
        );
        let reader = Self { build };
        reader
            .identity("readiness".into(), "readiness".into(), "readiness".into())
            .validate_profile()
            .map_err(anyhow::Error::msg)?;
        Ok(reader)
    }
    pub fn identity(
        &self,
        kernel_version: String,
        tools_drive_version: String,
        envd_version: String,
    ) -> SnapshotRestoreIdentity {
        SnapshotRestoreIdentity {
            backend: "dragonball".into(),
            architecture: "x86_64".into(),
            virtualization_mode: "kvm".into(),
            runner_build_id: self.build.runner_build_id.clone(),
            dragonball_revision: self.build.dragonball_revision.clone(),
            snapshot_format_epoch: self.build.snapshot_format_epoch,
            restore_abi_id: self.build.restore_abi_id.clone(),
            cpu_policy_id: "host-default".into(),
            device_profile_id: RUNTIME_FEATURE_PROFILE.into(),
            memory_layout_id: "packed-ram-4k-v2".into(),
            kernel_version,
            tools_drive_version,
            envd_version,
        }
    }
    pub fn validate_restore(&self, identity: &SnapshotRestoreIdentity) -> Result<()> {
        identity.validate_profile().map_err(anyhow::Error::msg)?;
        ensure!(
            *identity
                == self.identity(
                    identity.kernel_version.clone(),
                    identity.tools_drive_version.clone(),
                    identity.envd_version.clone()
                ),
            "snapshot requires a different qualified reader build"
        );
        Ok(())
    }
}

/// Pure preflight. Call before allocating VM device/network leases.
pub fn validate_artifact_identity(
    reader: &RestoreReader,
    record: &crate::snapshot::SnapshotRecord,
    manifest: &crate::sandbox::manifest::SandboxSnapshotManifest,
    envelope: &RuntimeSnapshotEnvelope,
) -> Result<()> {
    let committed = record
        .committed
        .as_ref()
        .context("snapshot is not committed")?;
    let versions = &committed.runtime_versions;
    reader.validate_restore(&envelope.identity)?;
    envelope
        .validate_state_topology()
        .map_err(anyhow::Error::msg)?;
    ensure!(
        versions.restore_identity.as_ref() == Some(&envelope.identity)
            && manifest.restore_identity.as_ref() == Some(&envelope.identity)
            && versions.backend == envelope.identity.backend
            && manifest.backend == envelope.identity.backend
            && manifest.version == crate::sandbox::manifest::MANIFEST_FORMAT_VERSION
            && versions.firecracker_version.is_empty()
            && versions.vmm_version.as_deref()
                == Some(envelope.identity.dragonball_revision.as_str())
            && versions.kernel_version == envelope.identity.kernel_version
            && versions.tools_drive_version == envelope.identity.tools_drive_version
            && versions.envd_version == envelope.identity.envd_version
            && committed.virtualization_mode == crate::virtualization::VirtualizationMode::Kvm,
        "snapshot record/manifest/envelope identity conflict"
    );
    ensure!(
        record.resources.cpu_count == u32::from(envelope.machine.vcpu_count)
            && record.resources.memory_mib == envelope.machine.memory_mib
            && manifest.memory.virtual_size == envelope.memory_layout.image_size
            && manifest.volume_drive_slots == manifest.volume_slots.len(),
        "snapshot resource/topology conflict"
    );
    let tools = envelope
        .machine
        .block_devices
        .iter()
        .find(|disk| disk.drive_id == "rootfs")
        .context("tools root device missing")?;
    let user_root = envelope
        .machine
        .block_devices
        .iter()
        .find(|disk| disk.drive_id == "user_rootfs")
        .context("user rootfs device missing")?;
    ensure!(
        tools.is_root
            && tools.read_only
            && !tools.is_volume_slot
            && !user_root.is_volume_slot
            && !user_root.is_root
            && !user_root.read_only
            && user_root.capacity_bytes == manifest.rootfs.virtual_size
            && envelope.machine.block_devices.len()
                == manifest.attached_drives.len()
                    + if manifest.volume_slots.is_empty() {
                        manifest.volume_devices.len() + 2
                    } else {
                        manifest.volume_slots.len() + 2
                    }
            && manifest.physical_extra_drive_count
                == manifest.attached_drives.len()
                    + if manifest.volume_slots.is_empty() {
                        manifest.volume_devices.len()
                    } else {
                        0
                    }
            && committed.volume_snapshots.len() == manifest.volume_devices.len(),
        "system/user root or device count conflict"
    );
    let mut seen = std::collections::HashSet::new();
    for drive in &manifest.attached_drives {
        ensure!(
            seen.insert(&drive.drive_id)
                && drive.drive_id != "rootfs"
                && drive.drive_id != "user_rootfs",
            "duplicate or system attached ID"
        );
        let saved = envelope
            .machine
            .block_devices
            .iter()
            .find(|disk| disk.drive_id == drive.drive_id)
            .context("attached logical device missing")?;
        ensure!(
            !saved.is_root
                && !saved.is_volume_slot
                && saved.read_only == drive.read_only
                && saved.capacity_bytes == drive.virtual_size,
            "attached device topology conflict"
        );
    }
    let mut mounts = std::collections::HashSet::new();
    for drive in &manifest.volume_devices {
        if !manifest
            .volume_slots
            .iter()
            .any(|slot| slot.slot_id == drive.drive_id)
        {
            crate::sandbox::validate_drive_id(&drive.drive_id)?;
        }
        let mount = crate::sandbox::normalize_mount_path_for_drive(
            &drive.drive_id,
            drive.mount_path.clone(),
        )?;
        ensure!(
            seen.insert(&drive.drive_id) && mounts.insert(mount.clone()),
            "duplicate volume identity or mount"
        );
        let saved = envelope
            .machine
            .block_devices
            .iter()
            .find(|disk| disk.drive_id == drive.drive_id)
            .context("volume logical device missing")?;
        let records = committed
            .volume_snapshots
            .iter()
            .filter(|volume| std::path::Path::new(&volume.mount_path) == mount)
            .collect::<Vec<_>>();
        ensure!(records.len() == 1, "volume record/mount conflict");
        ensure!(
            !saved.is_root
                && saved.backing_read_only.unwrap_or(saved.read_only) == drive.read_only
                && saved.capacity_bytes == drive.virtual_size
                && records[0].size_mb.checked_mul(1 << 20) == Some(drive.virtual_size)
                && (records[0].mode == crate::volume::VolumeMode::ReadOnly) == drive.read_only,
            "volume device topology conflict"
        );
    }
    ensure!(
        manifest.attached_drives.len() + manifest.volume_slots.len() <= 24,
        "slot budget exceeded"
    );
    let mut empty_seen = false;
    for (index, slot) in manifest.volume_slots.iter().enumerate() {
        let saved = &envelope.machine.block_devices[2 + manifest.attached_drives.len() + index];
        ensure!(
            slot.slot_id
                == format!(
                    "{}{index}",
                    crate::sandbox::extra_drive::VOLUME_DRIVE_SLOT_PREFIX
                )
                && saved.drive_id == slot.slot_id
                && saved.is_volume_slot
                && !saved.is_root
                && saved.capacity_bytes == slot.capacity_bytes
                && saved.read_only == slot.guest_read_only,
            "reserved slot topology conflict"
        );
        if let Some(mount) = &slot.mount_path {
            ensure!(
                !empty_seen
                    && manifest
                        .volume_devices
                        .iter()
                        .any(|drive| drive.drive_id == slot.slot_id
                            && drive.mount_path == *mount
                            && drive.sub_path == slot.sub_path
                            && drive.virtual_size == slot.capacity_bytes),
                "bound slot metadata conflict"
            );
        } else {
            empty_seen = true;
            ensure!(
                slot.sub_path.is_none()
                    && slot.capacity_bytes == 4096
                    && !slot.guest_read_only
                    && saved.backing_read_only == Some(false)
                    && !manifest
                        .volume_devices
                        .iter()
                        .any(|drive| drive.drive_id == slot.slot_id),
                "invalid empty slot"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::manifest::{SandboxSnapshotManifest, FIRECRACKER_BACKEND};
    use crate::snapshot::{CommittedSnapshot, SnapshotRecord, SnapshotRuntimeVersions};
    use agentenv_vmm_protocol::runtime::*;
    use serde_json::json;

    fn identity() -> SnapshotRestoreIdentity {
        SnapshotRestoreIdentity {
            backend: "dragonball".into(),
            architecture: "x86_64".into(),
            virtualization_mode: "kvm".into(),
            runner_build_id: format!("sha256:{}", "a".repeat(64)),
            dragonball_revision: QUALIFIED_DRAGONBALL_REVISION.into(),
            snapshot_format_epoch: 2,
            restore_abi_id: format!("dragonball-held-cow-v2@{QUALIFIED_DRAGONBALL_REVISION}"),
            cpu_policy_id: "host-default".into(),
            device_profile_id: RUNTIME_FEATURE_PROFILE.into(),
            memory_layout_id: "packed-ram-4k-v2".into(),
            kernel_version: "6.1.175".into(),
            tools_drive_version: "0.1.0".into(),
            envd_version: "0.6.0".into(),
        }
    }
    fn reader() -> RestoreReader {
        let id = identity();
        RestoreReader::new(RuntimeHello {
            runner_instance_id: "reader".into(),
            runner_build_id: id.runner_build_id,
            dragonball_revision: id.dragonball_revision,
            feature_profile: RUNTIME_FEATURE_PROFILE.into(),
            snapshot_format_epoch: id.snapshot_format_epoch,
            restore_abi_id: id.restore_abi_id,
            capabilities: vec![
                "full_memory_capture".into(),
                "held_restore".into(),
                "state_only_capture".into(),
                "leased_memory_export".into(),
            ],
        })
        .unwrap()
    }
    fn artifacts() -> (
        SnapshotRecord,
        SandboxSnapshotManifest,
        RuntimeSnapshotEnvelope,
    ) {
        let id = identity();
        let mut record =
            serde_json::to_value(SnapshotRecord::mock_ready(CommittedSnapshot::mock())).unwrap();
        record["committed"]["runtime_versions"] = json!({"backend":"dragonball","firecracker_version":"","vmm_version":QUALIFIED_DRAGONBALL_REVISION,"kernel_version":id.kernel_version,"tools_drive_version":id.tools_drive_version,"envd_version":id.envd_version,"restore_identity":id});
        let mut manifest =
            serde_json::to_value(SandboxSnapshotManifest::for_test(4096, &[])).unwrap();
        manifest["backend"] = json!("dragonball");
        manifest["restoreIdentity"] = serde_json::to_value(&id).unwrap();
        manifest["memory"]["virtualSize"] = json!(128 * 1024 * 1024u64);
        manifest["volumeDriveSlots"] = json!(0);
        let mut envelope = RuntimeSnapshotEnvelope {
            envelope_version: 2,
            identity: id,
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
                    "config":{"drive_id":disk.drive_id,"device_type":"RawBlock","path_on_host":format!("aenv-block:{}",disk.drive_id),"is_root_device":disk.is_root,"part_uuid":null,"is_read_only":disk.read_only,"is_direct":!(disk.is_root && disk.read_only),"no_drop":false,"sparse":false,"is_volume_slot":false,"backing_read_only":null,"num_queues":1,"queue_size":256,"rate_limiter":null,"use_shared_irq":false,"use_generic_irq":false,"use_pci_bus":false},
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
        (
            serde_json::from_value(record).unwrap(),
            serde_json::from_value(manifest).unwrap(),
            envelope,
        )
    }

    #[test]
    fn restore_rejects_identity_or_topology_conflict_before_allocation() {
        let (record, manifest, envelope) = artifacts();
        let reader = reader();
        validate_artifact_identity(&reader, &record, &manifest, &envelope).unwrap();
        let mut allocated = 0;
        for location in ["record", "manifest", "envelope"] {
            for field in [
                "backend",
                "architecture",
                "virtualization_mode",
                "runner_build_id",
                "dragonball_revision",
                "restore_abi_id",
                "cpu_policy_id",
                "device_profile_id",
                "memory_layout_id",
                "kernel_version",
                "tools_drive_version",
                "envd_version",
            ] {
                let mut record_value = serde_json::to_value(&record).unwrap();
                let mut manifest_value = serde_json::to_value(&manifest).unwrap();
                let mut envelope_value = serde_json::to_value(&envelope).unwrap();
                let object = match location {
                    "record" => {
                        &mut record_value["committed"]["runtime_versions"]["restore_identity"]
                    }
                    "manifest" => &mut manifest_value["restoreIdentity"],
                    _ => &mut envelope_value["identity"],
                };
                object[field] = json!("conflict");
                let result = validate_artifact_identity(
                    &reader,
                    &serde_json::from_value(record_value).unwrap(),
                    &serde_json::from_value(manifest_value).unwrap(),
                    &serde_json::from_value(envelope_value).unwrap(),
                );
                if result.is_ok() {
                    allocated += 1;
                }
                assert!(result.is_err(), "accepted {location}.{field}");
            }
        }
        let mut bad = envelope.clone();
        bad.machine.vcpu_count = 2;
        assert!(validate_artifact_identity(&reader, &record, &manifest, &bad).is_err());
        bad = envelope.clone();
        bad.machine
            .block_devices
            .push(bad.machine.block_devices[0].clone());
        assert!(validate_artifact_identity(&reader, &record, &manifest, &bad).is_err());
        bad = envelope;
        bad.identity.tools_drive_version.clear();
        assert!(validate_artifact_identity(&reader, &record, &manifest, &bad).is_err());
        assert_eq!(allocated, 0);
    }

    #[test]
    fn legacy_fc_versions_remain_readable() {
        let versions: SnapshotRuntimeVersions = serde_json::from_value(
            json!({"kernel_version":"6.1","firecracker_version":"1.14.1","envd_version":"0.6.0"}),
        )
        .unwrap();
        let value = serde_json::to_value(&versions).unwrap();
        assert_eq!(value["backend"], FIRECRACKER_BACKEND);
        assert_eq!(versions.firecracker_version, "1.14.1");
        assert!(versions.tools_drive_version.is_empty());
        assert!(value.get("restore_identity").is_none());
        assert!(value.get("vmm_version").is_none());
        let legacy: SandboxSnapshotManifest = serde_json::from_value(
            serde_json::to_value(SandboxSnapshotManifest::for_test(4096, &[])).unwrap(),
        )
        .unwrap();
        assert_eq!(legacy.backend, FIRECRACKER_BACKEND);
    }

    #[test]
    fn restore_identity_requires_exact_qualified_reader_not_just_epoch() {
        let reader = reader();
        reader.validate_restore(&identity()).unwrap();
        let mut other = identity();
        other.runner_build_id = format!("sha256:{}", "b".repeat(64));
        assert!(reader.validate_restore(&other).is_err());
        other = identity();
        other.snapshot_format_epoch = 1;
        assert!(reader.validate_restore(&other).is_err());
        other = identity();
        other.virtualization_mode = "pvm".into();
        assert!(reader.validate_restore(&other).is_err());
    }

    #[test]
    fn restore_rejects_consistent_previous_kata_revision() {
        let (mut record, mut manifest, mut envelope) = artifacts();
        let reader = reader();
        validate_artifact_identity(&reader, &record, &manifest, &envelope).unwrap();

        // Keep all three identities consistent and the build ID, profile and
        // epochs valid: rejection must come from the previous revision/ABI.
        let previous = "c77812d669737c11cb16321827acc1c79bf065f3";
        envelope.identity.dragonball_revision = previous.into();
        envelope.identity.restore_abi_id = format!("dragonball-held-cow-v2@{previous}");
        manifest.restore_identity = Some(envelope.identity.clone());
        let versions = &mut record.committed.as_mut().unwrap().runtime_versions;
        versions.vmm_version = Some(previous.into());
        versions.restore_identity = Some(envelope.identity.clone());
        let before = serde_json::to_value((&record, &manifest, &envelope)).unwrap();

        let error = validate_artifact_identity(&reader, &record, &manifest, &envelope)
            .expect_err("previous Kata revision must require its original reader");
        assert!(error
            .to_string()
            .contains("unsupported or incomplete restore identity"));
        assert_eq!(
            serde_json::to_value((&record, &manifest, &envelope)).unwrap(),
            before,
            "rejection must not rewrite snapshot identity"
        );
    }

    #[test]
    fn restore_reader_rejects_previous_kata_build_and_hello() {
        let current = reader().build;
        RestoreReader::from_build(current.clone()).unwrap();
        let previous = "c77812d669737c11cb16321827acc1c79bf065f3";
        let mut old = current;
        old.dragonball_revision = previous.into();
        old.restore_abi_id = format!("dragonball-held-cow-v2@{previous}");
        assert!(
            RestoreReader::from_build(old.clone()).is_err(),
            "old runner build must not be accepted by the upgraded node"
        );
        assert!(RestoreReader::new(RuntimeHello {
            runner_instance_id: "previous-reader".into(),
            runner_build_id: old.runner_build_id,
            dragonball_revision: old.dragonball_revision,
            feature_profile: old.feature_profile,
            snapshot_format_epoch: old.snapshot_format_epoch,
            restore_abi_id: old.restore_abi_id,
            capabilities: vec![
                "full_memory_capture".into(),
                "held_restore".into(),
                "state_only_capture".into(),
                "leased_memory_export".into(),
            ],
        })
        .is_err());
    }

    #[test]
    fn restore_identity_tools_root_and_user_rootfs_geometry_are_distinct() {
        let (record, manifest, mut envelope) = artifacts();
        validate_artifact_identity(&reader(), &record, &manifest, &envelope).unwrap();
        envelope
            .machine
            .block_devices
            .retain(|disk| disk.drive_id != "rootfs");
        assert!(
            validate_artifact_identity(&reader(), &record, &manifest, &envelope).is_err(),
            "tools drive must exist independently of user rootfs"
        );
    }

    #[test]
    fn restore_identity_checks_opaque_topology_before_allocation() {
        let (record, manifest, envelope) = artifacts();
        let reader = reader();
        validate_artifact_identity(&reader, &record, &manifest, &envelope).unwrap();
        for (pointer, value) in [
            ("/vcpu_states/0/id", json!(1)),
            ("/memory_state/regions/0/file_offset", json!(4096)),
            (
                "/device_states/block/devices/0/config/drive_id",
                json!("unknown"),
            ),
            (
                "/device_states/block/devices/0/config/path_on_host",
                json!("/dev/ublkb17"),
            ),
            (
                "/device_states/block/devices/0/config/is_read_only",
                json!(false),
            ),
            (
                "/device_states/virtio_net/devices/0/config/backend/host_dev_name",
                json!("old-tap"),
            ),
            ("/device_states/block/devices", json!([])),
        ] {
            let mut bad = envelope.clone();
            *bad.upstream_state.pointer_mut(pointer).unwrap() = value;
            assert!(
                validate_artifact_identity(&reader, &record, &manifest, &bad).is_err(),
                "allocated with {pointer}"
            );
        }
        let mut bad = envelope;
        bad.upstream_state["pid"] = json!(123);
        assert!(validate_artifact_identity(&reader, &record, &manifest, &bad).is_err());
    }

    #[test]
    fn restore_identity_manifest_cannot_construct_dragonball_without_identity() {
        assert!(
            SandboxSnapshotManifest::new(
                "dragonball",
                "/state",
                "/memory.json",
                4096,
                "/rootfs.json",
                4096,
                &[]
            )
            .is_err(),
            "must not publish an incomplete Dragonball manifest"
        );
        let manifest = SandboxSnapshotManifest::new_dragonball(
            identity(),
            "/state",
            "/memory.json",
            4096,
            "/rootfs.json",
            4096,
            &[],
        )
        .unwrap();
        assert_eq!(manifest.restore_identity.as_ref(), Some(&identity()));
        let versions = SnapshotRuntimeVersions::from_dragonball(identity()).unwrap();
        assert!(versions.firecracker_version.is_empty());
        assert_eq!(versions.tools_drive_version, "0.1.0");
        assert_eq!(
            versions.vmm_version.as_deref(),
            Some(QUALIFIED_DRAGONBALL_REVISION)
        );
        let mut missing_tools = identity();
        missing_tools.tools_drive_version.clear();
        assert!(SandboxSnapshotManifest::new_dragonball(
            missing_tools,
            "/state",
            "/memory.json",
            4096,
            "/rootfs.json",
            4096,
            &[]
        )
        .is_err());
    }
}
