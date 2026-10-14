//! Production runtime wire types. Deliberately separate from the M0 probe protocol.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const QUALIFIED_DRAGONBALL_REVISION: &str = "9e41795fea9109bc1acf7b1f6d6b878cfff7b72e";
pub const RUNTIME_FEATURE_PROFILE: &str = "db-virtio-blk-net-balloon-v2";
pub const SNAPSHOT_FORMAT_EPOCH: u16 = 2;
/// Bump when serialized state or restore semantics become incompatible.
/// Source revisions and build fingerprints are provenance, not ABI versions.
pub const RESTORE_ABI_ID: &str = "agentenv-dragonball-snapshot-v2";
// Kata's internal serializer epoch is independent of the AgentENV envelope ABI.
pub const UPSTREAM_SNAPSHOT_FORMAT_EPOCH: u16 = 1;

// Qualified pinned RawBlock/LocalFile and virtio-net constructors; no PCI/IOMMU,
// multiqueue or sparse/discard features. Never rewrite negotiated guest state.
pub fn block_profile_features(read_only: bool) -> u64 {
    (1u64 << 32) | (1u64 << 1) | (1u64 << 2) | if read_only { 1u64 << 5 } else { 0 }
}
pub const NET_PROFILE_FEATURES: u64 = (1u64 << 32)
    | (1u64 << 0)
    | (1u64 << 1)
    | (1u64 << 7)
    | (1u64 << 10)
    | (1u64 << 11)
    | (1u64 << 14);

pub fn block_profile_config_space(capacity_bytes: u64) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..8].copy_from_slice(&(capacity_bytes / 512).to_le_bytes());
    bytes[8..12].copy_from_slice(&0x100000u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&16u32.to_le_bytes());
    bytes[34..36].copy_from_slice(&1u16.to_le_bytes());
    bytes
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalBlockDevice {
    pub drive_id: String,
    pub read_only: bool,
    pub is_root: bool,
    #[serde(default)]
    pub is_volume_slot: bool,
    #[serde(default)]
    pub backing_read_only: Option<bool>,
    pub capacity_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RuntimeRateLimiter>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTokenBucket {
    pub size: u64,
    pub one_time_burst: u64,
    pub refill_time: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRateLimiter {
    pub bandwidth: RuntimeTokenBucket,
    pub ops: RuntimeTokenBucket,
}

impl RuntimeRateLimiter {
    pub fn validate(&self) -> Result<(), &'static str> {
        for bucket in [&self.bandwidth, &self.ops] {
            if bucket.size > i64::MAX as u64
                || bucket.one_time_burst > i64::MAX as u64
                || (bucket.size == 0 && (bucket.one_time_burst != 0 || bucket.refill_time != 0))
                || (bucket.size != 0 && bucket.refill_time != 1000)
            {
                return Err("unsupported disk rate bucket");
            }
        }
        if self.bandwidth.size == 0 && self.ops.size == 0 {
            return Err("empty disk rate policy must be disabled");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableMachine {
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub block_devices: Vec<LogicalBlockDevice>,
    pub network_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotEnvelope {
    pub envelope_version: u16,
    pub identity: SnapshotRestoreIdentity,
    pub machine: PortableMachine,
    pub memory_layout: MemoryLayout,
    pub upstream_state: serde_json::Value,
}

impl SnapshotRestoreIdentity {
    pub fn validate_profile(&self) -> Result<(), &'static str> {
        let digest = self.runner_build_id.strip_prefix("sha256:").unwrap_or("");
        if self.backend != "dragonball"
            || self.architecture != "x86_64"
            || self.virtualization_mode != "kvm"
            || self.dragonball_revision.len() != 40
            || !self
                .dragonball_revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.snapshot_format_epoch != SNAPSHOT_FORMAT_EPOCH
            || self.restore_abi_id != RESTORE_ABI_ID
            || self.cpu_policy_id != "host-default"
            || self.device_profile_id != RUNTIME_FEATURE_PROFILE
            || self.memory_layout_id != "packed-ram-4k-v2"
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.kernel_version.is_empty()
            || self.tools_drive_version.is_empty()
            || self.envd_version.is_empty()
        {
            return Err("unsupported or incomplete restore identity");
        }
        Ok(())
    }

    /// Compare reader compatibility without conflating it with artifact identity.
    /// Records referring to the same captured state must still use exact equality.
    pub fn is_restore_compatible_with(&self, other: &Self) -> bool {
        self.validate_profile().is_ok()
            && other.validate_profile().is_ok()
            && self.backend == other.backend
            && self.architecture == other.architecture
            && self.virtualization_mode == other.virtualization_mode
            && self.snapshot_format_epoch == other.snapshot_format_epoch
            && self.restore_abi_id == other.restore_abi_id
            && self.cpu_policy_id == other.cpu_policy_id
            && self.device_profile_id == other.device_profile_id
            && self.memory_layout_id == other.memory_layout_id
            && self.kernel_version == other.kernel_version
            && self.tools_drive_version == other.tools_drive_version
            && self.envd_version == other.envd_version
    }
}

pub fn valid_logical_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl RuntimeSnapshotEnvelope {
    pub fn validate_metadata(&self) -> Result<(), &'static str> {
        self.identity.validate_profile()?;
        self.memory_layout.validate()?;
        if self.envelope_version != 2
            || self.machine.vcpu_count == 0
            || self.machine.memory_mib == 0
            || self.memory_layout.image_size != u64::from(self.machine.memory_mib) * 1024 * 1024
            || self.memory_layout != MemoryLayout::for_runtime_memory(self.machine.memory_mib)
            || self.machine.block_devices.is_empty()
            || self.machine.block_devices.len() >= 256
            || self
                .machine
                .block_devices
                .iter()
                .filter(|disk| disk.is_root)
                .count()
                != 1
        {
            return Err("unsupported envelope version or machine geometry");
        }
        let mut ids = std::collections::HashSet::new();
        for disk in &self.machine.block_devices {
            if let Some(rate) = &disk.rate_limiter {
                rate.validate()?;
            }
            if !valid_logical_id(&disk.drive_id)
                || !ids.insert(&disk.drive_id)
                || disk.capacity_bytes == 0
                || disk.capacity_bytes % 512 != 0
                || (disk.is_volume_slot
                    && (disk.is_root || disk.backing_read_only.is_some() == disk.read_only))
                || (!disk.is_volume_slot && disk.backing_read_only.is_some())
            {
                return Err("invalid logical block device");
            }
        }
        if !valid_logical_id(&self.machine.network_id) || !ids.insert(&self.machine.network_id) {
            return Err("invalid logical network device");
        }
        Ok(())
    }

    /// Host-independent topology preflight for the node, before resource leases.
    /// The runner additionally decodes and checks the complete pinned state schema.
    pub fn validate_state_topology(&self) -> Result<(), &'static str> {
        self.validate_metadata()?;
        let state = self
            .upstream_state
            .as_object()
            .ok_or("missing upstream state")?;
        let fields = [
            "header",
            "vm_kvm_state",
            "vcpu_states",
            "memory_state",
            "device_states",
            "capture_cpu",
        ];
        if state.len() != fields.len()
            || fields.iter().any(|field| !state.contains_key(*field))
            || self.upstream_state["header"]["format_epoch"].as_u64()
                != Some(u64::from(UPSTREAM_SNAPSHOT_FORMAT_EPOCH))
            || !self.upstream_state["vm_kvm_state"].is_object()
            || !self.upstream_state["capture_cpu"].is_object()
        {
            return Err("unknown or incomplete state topology");
        }
        let cpus = self.upstream_state["vcpu_states"]
            .as_array()
            .ok_or("CPU set missing")?;
        if cpus.len() != usize::from(self.machine.vcpu_count)
            || cpus
                .iter()
                .enumerate()
                .any(|(index, cpu)| cpu["id"].as_u64() != Some(index as u64))
        {
            return Err("CPU set conflict");
        }
        let expected_regions = self.memory_layout.regions.iter().map(|region| serde_json::json!({
            "guest_addr":region.guest_base,"file_offset":region.image_offset,"size":region.length
        })).collect::<Vec<_>>();
        if self.upstream_state["memory_state"] != serde_json::json!({"regions":expected_regions}) {
            return Err("RAM topology conflict");
        }
        let devices = self.upstream_state["device_states"]
            .as_object()
            .ok_or("device set missing")?;
        if devices.len() != 3
            || !devices.contains_key("block")
            || !devices.contains_key("virtio_net")
            || !devices.contains_key("balloon")
        {
            return Err("unsupported device set");
        }
        let blocks = devices["block"]["devices"]
            .as_array()
            .ok_or("block set missing")?;
        if blocks.len() != self.machine.block_devices.len() {
            return Err("block set conflict");
        }
        let mut ids = std::collections::HashSet::new();
        for saved in blocks {
            let id = saved["config"]["drive_id"]
                .as_str()
                .ok_or("block ID missing")?;
            let disk = self
                .machine
                .block_devices
                .iter()
                .find(|disk| disk.drive_id == id)
                .ok_or("unknown block ID")?;
            let expected = serde_json::json!({"drive_id":disk.drive_id,"device_type":"RawBlock","path_on_host":format!("aenv-block:{}",disk.drive_id),
                "is_root_device":disk.is_root,"part_uuid":null,"is_read_only":disk.read_only,"is_direct":!(disk.is_root && disk.read_only),"no_drop":false,"sparse":false,"is_volume_slot":disk.is_volume_slot,"backing_read_only":disk.backing_read_only,
                "num_queues":1,"queue_size":256,"rate_limiter":disk.rate_limiter,"use_shared_irq":false,"use_generic_irq":false,"use_pci_bus":false});
            if !ids.insert(id)
                || saved["config"] != expected
                || !topology_transport(&saved["transport"], 1)
            {
                return Err("block config/transport conflict");
            }
            let info = &saved["device_info"]["RawBlock"];
            if !topology_device_info(info)
                || info["avail_features"].as_u64() != Some(block_profile_features(disk.read_only))
                || info["config_space"]
                    != serde_json::json!(block_profile_config_space(disk.capacity_bytes))
            {
                return Err("block info/profile conflict");
            }
        }
        let nets = devices["virtio_net"]["devices"]
            .as_array()
            .ok_or("network set missing")?;
        let expected = serde_json::json!({"num_queues":null,"queue_size":null,"backend":{"type":"virtio","iface_id":self.machine.network_id,
            "host_dev_name":format!("aenv-net:{}",self.machine.network_id),"rx_rate_limiter":null,"tx_rate_limiter":null,"allow_duplicate_mac":false},
            "guest_mac":null,"use_shared_irq":false,"use_generic_irq":false});
        if nets.len() != 1
            || nets[0]["config"] != expected
            || !topology_device_info(&nets[0]["device_info"])
            || nets[0]["device_info"]["avail_features"].as_u64() != Some(NET_PROFILE_FEATURES)
            || !nets[0]["device_info"]["config_space"]
                .as_array()
                .is_some_and(|bytes| bytes.len() == 12)
            || !topology_transport(&nets[0]["transport"], 2)
        {
            return Err("network config/transport conflict");
        }
        let balloons = devices["balloon"]["devices"]
            .as_array()
            .ok_or("balloon set missing")?;
        if balloons.len() != 1 {
            return Err("balloon set conflict");
        }
        let balloon = &balloons[0];
        let expected = serde_json::json!({"balloon_id":"memory","size_mib":0,
            "use_shared_irq":false,"use_generic_irq":false,"f_deflate_on_oom":true,"f_reporting":true});
        let state = &balloon["device_info"];
        let info = &state["device_info"];
        let pages = state["num_pages"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok());
        let actual = state["actual"].as_u64().and_then(|n| u32::try_from(n).ok());
        let config_space = pages.zip(actual).map(|(pages, actual)| {
            pages
                .to_le_bytes()
                .into_iter()
                .chain(actual.to_le_bytes())
                .collect::<Vec<_>>()
        });
        if balloon["config"] != expected
            || !topology_transport(&balloon["transport"], 3)
            || !topology_device_info(info)
            || info["avail_features"].as_u64() != Some((1u64 << 32) | (1 << 2) | (1 << 5))
            || config_space.is_none_or(|bytes| info["config_space"] != serde_json::json!(bytes))
        {
            return Err("balloon config/transport conflict");
        }
        Ok(())
    }
}

fn topology_transport(value: &serde_json::Value, queue_count: usize) -> bool {
    value["Mmio"]["device_activated"] == true
        && value["Mmio"]["queues"]
            .as_array()
            .is_some_and(|queues| queues.len() == queue_count)
}

fn topology_device_info(value: &serde_json::Value) -> bool {
    let Some(available) = value["avail_features"].as_u64() else {
        return false;
    };
    let Some(acked) = value["acked_features"].as_u64() else {
        return false;
    };
    acked & !available == 0
        && value["config_space"].as_array().is_some_and(|bytes| {
            bytes
                .iter()
                .all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255))
        })
}

pub const RUNTIME_PROTOCOL_VERSION: u16 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRestoreIdentity {
    pub backend: String,
    pub architecture: String,
    pub virtualization_mode: String,
    pub runner_build_id: String,
    pub dragonball_revision: String,
    pub snapshot_format_epoch: u16,
    pub restore_abi_id: String,
    pub cpu_policy_id: String,
    pub device_profile_id: String,
    pub memory_layout_id: String,
    pub kernel_version: String,
    pub tools_drive_version: String,
    pub envd_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureToken {
    pub runner_instance_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRegion {
    pub guest_base: u64,
    pub image_offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryLayout {
    pub page_size: u64,
    pub regions: Vec<MemoryRegion>,
    pub image_size: u64,
}

impl MemoryLayout {
    /// Qualified x86_64, single NUMA node, no firmware recipe. GPA hole is not
    /// part of the packed RAM file (upstream dbs_boot MMIO_LOW_START/END).
    pub fn for_runtime_memory(memory_mib: u32) -> Self {
        let image_size = u64::from(memory_mib) * 1024 * 1024;
        let low = image_size.min(3u64 << 30);
        let mut regions = vec![MemoryRegion {
            guest_base: 0,
            image_offset: 0,
            length: low,
        }];
        if image_size > low {
            regions.push(MemoryRegion {
                guest_base: 4u64 << 30,
                image_offset: low,
                length: image_size - low,
            });
        }
        Self {
            page_size: 4096,
            regions,
            image_size,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.page_size != 4096 || self.regions.is_empty() || self.regions.len() > 256 {
            return Err("unsupported page size or RAM region count");
        }
        let mut packed = 0u64;
        let mut guest_end = 0u64;
        for region in &self.regions {
            if region.length == 0
                || region.guest_base % self.page_size != 0
                || region.length % self.page_size != 0
                || region.image_offset != packed
                || region.guest_base < guest_end
            {
                return Err("unaligned, overlapping or unpacked RAM layout");
            }
            guest_end = region
                .guest_base
                .checked_add(region.length)
                .ok_or("GPA overflow")?;
            packed = packed
                .checked_add(region.length)
                .ok_or("RAM image overflow")?;
        }
        if packed != self.image_size {
            return Err("RAM image size mismatch");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerAck {
    pub device_id: String,
    pub generation: u64,
    pub pending_io: u64,
    pub memory_writers: u32,
    pub flush_completed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureReport {
    pub token: CaptureToken,
    pub memory_layout: MemoryLayout,
    pub vcpu_ids: Vec<u8>,
    pub worker_acks: Vec<WorkerAck>,
}

impl CaptureReport {
    /// Structural proof; the service additionally compares the complete configured machine.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.memory_layout.validate()?;
        if self.token.runner_instance_id.is_empty()
            || self.token.generation == 0
            || self.vcpu_ids.is_empty()
            || self.vcpu_ids.len() > 255
            || self
                .vcpu_ids
                .iter()
                .enumerate()
                .any(|(index, id)| index != usize::from(*id))
        {
            return Err("invalid token or CPU proof");
        }
        if self.worker_acks.len() < 3 || self.worker_acks.len() > 257 {
            return Err("invalid worker set");
        }
        let mut ids = std::collections::HashSet::new();
        for (index, ack) in self.worker_acks.iter().enumerate() {
            let block = index > 0 && index + 1 < self.worker_acks.len();
            let id = if index == 0 {
                (ack.device_id == "balloon:memory").then_some("memory")
            } else if block {
                ack.device_id
                    .strip_prefix("block:")
                    .and_then(|id| id.strip_suffix("/q0"))
            } else {
                ack.device_id.strip_prefix("net:")
            };
            if !id.is_some_and(|id| !id.is_empty())
                || !ids.insert(&ack.device_id)
                || ack.generation != self.token.generation
                || ack.pending_io != 0
                || ack.memory_writers != 0
                || (block && !ack.flush_completed)
            {
                return Err("invalid worker acknowledgement");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVmConfig {
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub kernel_path: PathBuf,
    pub boot_args: String,
    pub block_devices: Vec<RuntimeBlockDevice>,
    pub network: RuntimeNetworkDevice,
    pub identity: SnapshotRestoreIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBlockDevice {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub read_only: bool,
    pub is_root: bool,
    /// Guest feature policy and host access are distinct for reserved slots.
    #[serde(default)]
    pub is_volume_slot: bool,
    #[serde(default)]
    pub backing_read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RuntimeRateLimiter>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNetworkDevice {
    pub iface_id: String,
    pub tap_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotInput {
    pub state_path: PathBuf,
    pub memory_path: PathBuf,
    pub identity: SnapshotRestoreIdentity,
    pub probe_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RuntimeCommand {
    Hello,
    Configure(RuntimeVmConfig),
    Start,
    BeginCapture,
    ExportHeldSnapshot {
        token: CaptureToken,
        state_path: PathBuf,
        memory_path: PathBuf,
    },
    ExportHeldState {
        token: CaptureToken,
        state_path: PathBuf,
    },
    ExportMemoryMap {
        token: CaptureToken,
        cursor: u64,
    },
    ReleaseMemoryMap {
        token: CaptureToken,
    },
    EndCapture {
        token: CaptureToken,
    },
    LoadHeld(RuntimeSnapshotInput),
    Resume {
        token: CaptureToken,
    },
    Status,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Unconfigured,
    Configured,
    Running,
    Capturing,
    Held,
    LoadedHeld,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStatus {
    pub phase: RuntimePhase,
    pub capture: Option<CaptureReport>,
    pub memory_map_pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHello {
    pub runner_instance_id: String,
    pub runner_build_id: String,
    pub dragonball_revision: String,
    pub feature_profile: String,
    pub snapshot_format_epoch: u16,
    pub restore_abi_id: String,
    pub capabilities: Vec<String>,
}

/// Immutable reader build metadata; deliberately contains no runtime nonce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBuildIdentity {
    pub runner_build_id: String,
    pub dragonball_revision: String,
    pub snapshot_format_epoch: u16,
    pub restore_abi_id: String,
    pub feature_profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "reply",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RuntimeReply {
    Hello(RuntimeHello),
    Done,
    Held(CaptureReport),
    MemoryMap(crate::runtime_memory::FrozenMemoryMapChunk),
    Status(RuntimeStatus),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequest {
    pub mode: String,
    pub protocol_version: u16,
    pub request_id: u64,
    pub sandbox_instance_id: String,
    pub budget_ms: u64,
    pub command: RuntimeCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeResponse {
    pub mode: String,
    pub protocol_version: u16,
    pub request_id: u64,
    pub sandbox_instance_id: String,
    pub result: Result<RuntimeReply, RuntimeError>,
}
