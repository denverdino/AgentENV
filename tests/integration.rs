mod common;

#[path = "integration/hypervisor_components.rs"]
mod hypervisor_components;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_m0.rs"]
mod dragonball_m0;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_m1.rs"]
mod dragonball_m1;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_m2.rs"]
mod dragonball_m2;

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
#[path = "integration/dragonball_m2_slots.rs"]
mod dragonball_m2_slots;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_m2_startup.rs"]
mod dragonball_m2_startup;

#[cfg(all(
    feature = "hypervisor-dragonball",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[path = "integration/dragonball_m3.rs"]
mod dragonball_m3;

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
