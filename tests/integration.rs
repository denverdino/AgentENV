mod common;

#[path = "integration/hypervisor_components.rs"]
mod hypervisor_components;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_compatibility.rs"]
mod dragonball_compatibility;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_probe.rs"]
mod dragonball_probe;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_support.rs"]
mod dragonball_support;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_lifecycle.rs"]
mod dragonball_lifecycle;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_memory.rs"]
mod dragonball_memory;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_apf.rs"]
mod dragonball_apf;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_slots.rs"]
mod dragonball_slots;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_startup.rs"]
mod dragonball_startup;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_platform.rs"]
mod dragonball_platform;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/envd_supervisor.rs"]
mod envd_supervisor;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/fc.rs"]
mod fc;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/process.rs"]
mod process;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/snapshot.rs"]
mod snapshot;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/snapshot_attached_drive.rs"]
mod snapshot_attached_drive;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/posix_startup_pack.rs"]
mod posix_startup_pack;

#[cfg(feature = "hypervisor-firecracker")]
#[path = "integration/ublk.rs"]
mod ublk;
