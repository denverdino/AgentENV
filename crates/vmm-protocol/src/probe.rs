use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub protocol_version: u16,
    pub request_id: u64,
    pub sandbox_instance_id: String,
    pub budget_ms: u64,
    pub command: ProbeCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub protocol_version: u16,
    pub request_id: u64,
    pub sandbox_instance_id: String,
    pub result: Result<ProbeReply, ProbeError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProbeCommand {
    Hello,
    Configure(ProbeVmConfig),
    Start,
    Pause,
    Resume,
    SaveQuiesced {
        state_path: PathBuf,
        memory_path: PathBuf,
    },
    RestoreQuiesced {
        state_path: PathBuf,
        memory_path: PathBuf,
    },
    Status,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProbeReply {
    Hello(RunnerIdentity),
    Status(ProbePhase),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerIdentity {
    pub protocol_version: u16,
    pub runner_version: String,
    pub dragonball_revision: String,
    pub feature_profile: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhase {
    Unconfigured,
    Configured,
    Running,
    Paused,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeVmConfig {
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub kernel_path: PathBuf,
    pub boot_args: String,
    pub block_devices: Vec<ProbeBlockDevice>,
    pub network: ProbeNetworkDevice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeBlockDevice {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub read_only: bool,
    pub is_root: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeNetworkDevice {
    pub iface_id: String,
    pub tap_name: String,
}
