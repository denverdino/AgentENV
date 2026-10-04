//! Product composition. Both-feature builds retain the legacy FC default.
use std::sync::{Arc, RwLock};

use anyhow::Result;

#[cfg(not(any(feature = "hypervisor-firecracker", feature = "hypervisor-dragonball")))]
compile_error!("enable hypervisor-firecracker or hypervisor-dragonball");
#[cfg(all(
    not(feature = "hypervisor-firecracker"),
    feature = "hypervisor-dragonball",
    not(all(target_os = "linux", target_arch = "x86_64"))
))]
compile_error!("hypervisor-dragonball requires Linux x86_64");

#[cfg(all(
    not(feature = "hypervisor-firecracker"),
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
pub use super::dragonball::DragonballSandboxFactory as SelectedSandboxFactory;
#[cfg(feature = "hypervisor-firecracker")]
pub use super::FirecrackerSandboxFactory as SelectedSandboxFactory;

pub const BACKEND: &str = if cfg!(feature = "hypervisor-firecracker") {
    "firecracker"
} else {
    "dragonball"
};

/// Metadata/API fixtures never launch a VM; no host assets should be needed.
#[cfg(test)]
pub(crate) fn test_factory() -> SelectedSandboxFactory {
    #[cfg(feature = "hypervisor-firecracker")]
    {
        SelectedSandboxFactory::new()
    }
    #[cfg(all(
        not(feature = "hypervisor-firecracker"),
        feature = "hypervisor-dragonball",
        target_os = "linux",
        target_arch = "x86_64"
    ))]
    {
        super::dragonball::factory_for_tests()
    }
}

pub fn factory_with_cpu_config(cpu: Arc<RwLock<Option<String>>>) -> Result<SelectedSandboxFactory> {
    #[cfg(feature = "hypervisor-firecracker")]
    {
        Ok(SelectedSandboxFactory::with_cpu_config(cpu))
    }
    #[cfg(all(
        not(feature = "hypervisor-firecracker"),
        feature = "hypervisor-dragonball",
        target_os = "linux",
        target_arch = "x86_64"
    ))]
    {
        use super::dragonball::{DragonballMemoryMode, DragonballRuntimePolicy};
        let config = crate::cfg::ConfigManager::global_config();
        let runner = config
            .dragonball
            .binary_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("dragonball.binary_path is required"))?;
        let kernel =
            config.kernel.image_path.clone().ok_or_else(|| {
                anyhow::anyhow!("Dragonball requires an explicit kernel.image_path")
            })?;
        let policy = DragonballRuntimePolicy::from_app_config(config, runner, kernel)?
            .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental);
        SelectedSandboxFactory::with_cpu_config(policy, cpu)
    }
}

pub async fn prime_pool() -> Result<()> {
    #[cfg(feature = "hypervisor-firecracker")]
    super::FirecrackerPool::prime(std::time::Duration::from_secs(10)).await?;
    Ok(())
}

pub async fn shutdown_pool() -> Result<()> {
    #[cfg(feature = "hypervisor-firecracker")]
    if let Some(pool) = super::FirecrackerPool::global() {
        pool.shutdown().await?;
    }
    Ok(())
}

pub fn cpu_template_helper() -> Option<std::path::PathBuf> {
    if BACKEND == "firecracker" {
        crate::cfg::ConfigManager::global_config().resolved_cpu_template_helper()
    } else {
        None
    }
}
