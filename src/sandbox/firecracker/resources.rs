//! Host resources a running Firecracker sandbox owns.

use std::path::PathBuf;
use std::time::Duration;

use tracing::warn;

use super::startup_pack::LocalStartupPrefetch;
use crate::sandbox::custom_extension::CustomExtensionHookGuard;
use crate::sandbox::network::{NetworkManager, Slot};
use crate::sandbox::ublk::{OverlaybdRuntimeHandle, SharedReadOnlyDevice, UblkDevice};

/// Every host resource acquired while starting a sandbox.
///
/// Store each handle here as soon as it is acquired: a later startup failure
/// is cleaned up by [`release`](Self::release) on `stop()`, or by `Drop`.
#[derive(Default)]
pub(super) struct SandboxResources {
    pub(super) network_slot: Option<Slot>,
    pub(super) rootfs_runtime: Option<OverlaybdRuntimeHandle>,
    /// image.json path the rootfs device was opened with. Also released at
    /// envd ready so a rootfs background download (when enabled) never
    /// waits out the fallback with no notification.
    pub(super) rootfs_image_config_path: Option<PathBuf>,
    pub(super) tools_ublk_device: Option<SharedReadOnlyDevice>,
    pub(super) mem_ublk_device: Option<SharedReadOnlyDevice>,
    /// image.json path the memory device was opened with. Used as the device
    /// key to release held background downloads once envd is ready.
    pub(super) mem_snapshot_image_config_path: Option<PathBuf>,
    /// Dedicated, non-shared memory device used only by startup-pack
    /// recording VMs (`pack_recording = true`). Released with
    /// `UblkDeviceManager::delete_device`, never returned to the warm pool.
    pub(super) mem_dedicated_device: Option<UblkDevice>,
    pub(super) startup_prefetch_task: Option<LocalStartupPrefetch>,
    pub(super) extra_drive_runtimes: Vec<OverlaybdRuntimeHandle>,
    /// Delivers the custom extension stop hook exactly once: `release()` calls
    /// [`CustomExtensionHookGuard::stop`], otherwise its own drop fires the
    /// best-effort notification. `None` when no start hook was delivered (or
    /// no extension is configured).
    pub(super) custom_extension_hook_guard: Option<CustomExtensionHookGuard>,
}

impl SandboxResources {
    /// Ask the startup prefetch reader to stop before the VM is stopped.
    pub(super) fn cancel_startup_prefetch(&self) {
        if let Some(task) = self.startup_prefetch_task.as_ref() {
            task.cancel();
        }
    }

    /// Wait for the startup prefetch reader to stop. A timed-out reader keeps
    /// its own device lease until background cleanup completes.
    pub(super) async fn stop_startup_prefetch(&mut self, timeout: Duration) {
        if let Some(task) = self.startup_prefetch_task.take() {
            task.stop(timeout).await;
        }
    }
}

/// Handles cases where `release()` wasn't reached (panic, early return, etc.).
///
/// For ublk, devices are managed by the daemon process, so we don't need to
/// kill any server process. The device ID is NOT recycled on abnormal drop
/// since proper delete (via the daemon) was not performed.
impl Drop for SandboxResources {
    fn drop(&mut self) {
        // Fire the best-effort stop notification (fire-and-forget, never
        // blocking drop) before releasing resources.
        self.custom_extension_hook_guard.take();
        // In daemon mode, ublk devices survive sandbox drop. The daemon will
        // clean them up on its own shutdown, or they can be explicitly deleted
        // via the orchestrator's stop() path.
        self.rootfs_runtime.take();
        self.mem_ublk_device.take();
        self.tools_ublk_device.take();
        // In daemon mode, extra-drive devices survive sandbox drop. They are
        // explicitly deleted on the stop() path and otherwise cleaned up when
        // the daemon shuts down.
        self.extra_drive_runtimes.clear();
        if let Some(slot) = self.network_slot.take() {
            if let Err(e) = NetworkManager::global().release(slot) {
                warn!(error = %e, "failed to release network slot on drop");
            }
        }
    }
}
