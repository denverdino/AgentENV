use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRuntimeVersions {
    #[serde(default = "crate::snapshot::default_backend")]
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_identity: Option<crate::sandbox::restore_identity::SnapshotRestoreIdentity>,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub envd_version: String,
    #[serde(default)]
    pub tools_drive_version: String,
}

impl SnapshotRuntimeVersions {
    pub fn new(
        kernel_version: String,
        firecracker_version: String,
        envd_version: String,
        tools_drive_version: String,
    ) -> Self {
        Self {
            backend: crate::sandbox::FIRECRACKER_BACKEND.into(),
            vmm_version: None,
            restore_identity: None,
            kernel_version,
            firecracker_version,
            envd_version,
            tools_drive_version,
        }
    }

    pub fn from_dragonball(
        identity: crate::sandbox::restore_identity::SnapshotRestoreIdentity,
    ) -> anyhow::Result<Self> {
        identity.validate_profile().map_err(anyhow::Error::msg)?;
        Ok(Self {
            backend: identity.backend.clone(),
            vmm_version: Some(identity.dragonball_revision.clone()),
            kernel_version: identity.kernel_version.clone(),
            tools_drive_version: identity.tools_drive_version.clone(),
            envd_version: identity.envd_version.clone(),
            firecracker_version: String::new(),
            restore_identity: Some(identity),
        })
    }
}
