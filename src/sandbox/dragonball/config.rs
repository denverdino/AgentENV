use std::{path::PathBuf, sync::Arc};

use agentenv_vmm_protocol::runtime::{RuntimeRateLimiter, RuntimeTokenBucket};
use anyhow::{ensure, Result};

use crate::{cfg::AppConfig, virtualization::VirtualizationMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragonballMemoryMode {
    FullFileOracle,
    SharedUblkIncremental,
}

#[derive(Clone)]
pub struct DragonballRuntimePolicy {
    pub(super) app: Arc<AppConfig>,
    pub(super) runner: PathBuf,
    pub(super) kernel: PathBuf,
    pub(super) tools: PathBuf,
    pub(super) disk_rate: Option<RuntimeRateLimiter>,
    pub(super) memory_mode: DragonballMemoryMode,
    pub(super) extension: Option<Arc<crate::sandbox::custom_extension::CustomExtensionClient>>,
}

impl std::fmt::Debug for DragonballRuntimePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragonballRuntimePolicy")
            .field("runner", &self.runner)
            .field("kernel", &self.kernel)
            .field("memory_mode", &self.memory_mode)
            .finish_non_exhaustive()
    }
}

impl DragonballRuntimePolicy {
    pub fn from_app_config(config: &AppConfig, runner: PathBuf, kernel: PathBuf) -> Result<Self> {
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64"))
                && config.virtualization_mode == VirtualizationMode::Kvm,
            "Dragonball runtime requires Linux x86_64/KVM with host-default CPU policy"
        );
        let tools = config
            .tools
            .drive_path
            .clone()
            .map(Ok)
            .unwrap_or_else(|| config.resolved_tools_drive_path())?;
        for path in [&runner, &kernel, &tools] {
            ensure!(
                path.is_absolute() && path.is_file(),
                "missing absolute runtime input {}",
                path.display()
            );
        }
        ensure!(
            config.runtime_path.is_absolute(),
            "runtime path must be absolute"
        );
        let rate = &config.machine.disk_rate_limit;
        let bucket = |size, one_time_burst| RuntimeTokenBucket {
            size,
            one_time_burst,
            refill_time: if size == 0 { 0 } else { 1000 },
        };
        let disk_rate = if rate.enabled && (rate.bandwidth_bytes_per_sec != 0 || rate.iops != 0) {
            let rate = RuntimeRateLimiter {
                bandwidth: bucket(rate.bandwidth_bytes_per_sec, rate.bandwidth_burst_bytes),
                ops: bucket(rate.iops, rate.iops_burst),
            };
            rate.validate().map_err(anyhow::Error::msg)?;
            Some(rate)
        } else {
            None
        };
        let extension = crate::sandbox::custom_extension::CustomExtensionClient::from_config(
            &config.custom_extension,
        )?
        .map(Arc::new);
        Ok(Self {
            app: Arc::new(config.clone()),
            runner,
            kernel,
            tools,
            disk_rate,
            memory_mode: DragonballMemoryMode::FullFileOracle,
            extension,
        })
    }

    pub fn with_memory_mode(mut self, mode: DragonballMemoryMode) -> Self {
        self.memory_mode = mode;
        self
    }
}
