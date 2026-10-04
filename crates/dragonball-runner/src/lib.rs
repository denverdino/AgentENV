use serde::{Deserialize, Serialize};

pub mod probe_snapshot;
#[cfg(target_os = "linux")]
pub mod process;
pub mod runtime_service;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod runtime_snapshot;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod seccomp;
pub mod service;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod vmm;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerBuildIdentity {
    pub protocol_version: u16,
    pub runner_version: String,
    pub dragonball_revision: String,
    pub feature_profile: String,
}

pub fn build_identity() -> RunnerBuildIdentity {
    RunnerBuildIdentity {
        protocol_version: 1,
        runner_version: env!("CARGO_PKG_VERSION").into(),
        dragonball_revision: env!("DRAGONBALL_REVISION").into(),
        feature_profile: "m0-mmio-blk-net".into(),
    }
}
