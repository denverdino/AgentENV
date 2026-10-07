//! Own runner resources independently of the M0 qualification harness.
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tempfile::TempDir;

use super::process::RunnerProcessGuard;
use crate::sandbox::{
    network::{metadata::MetadataListener, Slot},
    ublk::UblkDevice,
    NetworkManager, UblkDeviceManager,
};

use crate::sandbox::runtime_cleanup::RetainedArtifacts;
pub(super) use crate::sandbox::runtime_cleanup::RetainedWork;

pub(super) struct RuntimeResources {
    pub(super) process: Option<RunnerProcessGuard>,
    pub(super) metadata: Option<MetadataListener>,
    pub(super) devices: Vec<UblkDevice>,
    pub(super) dedicated_devices: Vec<UblkDevice>,
    pub(super) slot: Option<Slot>,
    pub(super) work: Arc<TempDir>,
    failure_evidence_root: Option<PathBuf>,
    pub(super) retained_artifacts: Vec<Arc<dyn std::any::Any + Send + Sync>>,
    cleanup_task: Option<crate::sandbox::runtime_cleanup::CleanupTask>,
}

impl RuntimeResources {
    pub(super) fn new(work: TempDir, failure_evidence_root: Option<PathBuf>) -> Self {
        Self {
            process: None,
            metadata: None,
            devices: Vec::new(),
            dedicated_devices: Vec::new(),
            slot: None,
            work: Arc::new(work),
            failure_evidence_root,
            retained_artifacts: Vec::new(),
            cleanup_task: None,
        }
    }
    pub(super) async fn reap(&mut self) -> Result<()> {
        if let Some(process) = self.process.as_mut() {
            process.terminate_and_wait(Duration::from_secs(10)).await?;
        }
        self.process = None;
        Ok(())
    }

    pub(super) async fn cleanup(&mut self) -> Result<()> {
        self.reap().await?;
        if let Some(metadata) = self.metadata.as_mut() {
            metadata.stop().await?;
        }
        self.metadata = None;
        if self.cleanup_task.is_none() {
            let dedicated = std::mem::take(&mut self.dedicated_devices);
            let devices = std::mem::take(&mut self.devices);
            let slot = self.slot.take();
            let mut owners = std::mem::take(&mut self.retained_artifacts);
            owners.push(self.work.clone());
            let mut artifacts = RetainedArtifacts(Some(owners));
            self.cleanup_task = Some(crate::sandbox::runtime_cleanup::CleanupTask::new(
                async move {
                    let mut errors = Vec::new();
                    for device in dedicated {
                        if let Err(error) = UblkDeviceManager::global().delete_device(&device).await
                        {
                            errors.push(format!("delete dedicated device: {error:#}"));
                        }
                    }
                    for device in devices {
                        if let Err(error) =
                            UblkDeviceManager::global().release_device(&device).await
                        {
                            errors.push(format!("release runtime device: {error:#}"));
                        }
                    }
                    if let Some(slot) = slot {
                        if let Err(error) = NetworkManager::global().release(slot) {
                            errors.push(format!("release network: {error:#}"));
                        }
                    }
                    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
                    artifacts.0.take();
                    Ok(())
                },
            )?);
        }
        self.cleanup_task.as_ref().unwrap().wait().await?;
        self.failure_evidence_root = None;
        tracing::info!(work = %self.work.path().display(), "runner reaped; metadata stopped; device and network leases released");
        Ok(())
    }
}

// Admit only a bounded first set of failure archives. Never remove or overwrite
// historical evidence; operators can move these archives after inspection.
const MAX_FAILURE_ARCHIVES: usize = 16;
const MAX_DIAGNOSTIC_BYTES: u64 = 64 * 1024;

impl RuntimeResources {
    pub(super) fn archive_failure(&mut self) {
        let Some(root) = self.failure_evidence_root.take() else {
            return;
        };
        match reserve_failure_archive(&root) {
            Ok(Some(directory)) => self.archive_diagnostics(&directory),
            Ok(None) => {
                tracing::warn!(path = %root.display(), "failure evidence capacity reached; skipping archive")
            }
            Err(error) => {
                tracing::warn!(%error, "cannot create runtime failure evidence; continuing cleanup")
            }
        }
    }

    /// Diagnostic errors must never prevent metadata/device/namespace release.
    pub(super) fn archive_diagnostics(&self, directory: &Path) {
        if let Err(error) = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            tracing::warn!(%error, "cannot create diagnostic directory; continuing cleanup");
            return;
        }
        for name in ["serial.log", "runner-error.log"] {
            if let Err(error) = copy_log_tail(&self.work.path().join(name), &directory.join(name)) {
                tracing::warn!(%error, name, "cannot archive runner log; continuing cleanup");
            }
        }
        if let Some(metadata) = &self.metadata {
            let result = (|| -> Result<()> {
                let mut requests = metadata.recorded_requests()?;
                loop {
                    let bytes = serde_json::to_vec_pretty(&requests)?;
                    if bytes.len() as u64 <= MAX_DIAGNOSTIC_BYTES {
                        fs::write(directory.join("metadata-requests.json"), bytes)?;
                        break;
                    }
                    // Preserve complete JSON and the most recent bounded requests.
                    requests.remove(0);
                }
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "cannot archive metadata requests; continuing cleanup");
            }
        }
        let work = self.work.path().display().to_string();
        if let Err(error) = fs::write(
            directory.join("runtime-directory.txt"),
            &work.as_bytes()[..work.len().min(MAX_DIAGNOSTIC_BYTES as usize)],
        ) {
            tracing::warn!(%error, "cannot archive runtime identity; continuing cleanup");
        }
    }
}

fn reserve_failure_archive(root: &Path) -> std::io::Result<Option<PathBuf>> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    for index in 0..MAX_FAILURE_ARCHIVES {
        let directory = root.join(index.to_string());
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok(Some(directory)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn copy_log_tail(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut source = match File::open(source) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let length = source.metadata()?.len();
    source.seek(SeekFrom::Start(length.saturating_sub(MAX_DIAGNOSTIC_BYTES)))?;
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(destination)?;
    std::io::copy(&mut source.take(MAX_DIAGNOSTIC_BYTES), &mut destination)?;
    Ok(())
}

impl Drop for RuntimeResources {
    fn drop(&mut self) {
        if let Some(process) = self.process.as_mut() {
            if let Err(error) = process.terminate_blocking(Duration::from_secs(10)) {
                tracing::error!(%error, "retaining runtime leases: runner exit unconfirmed");
                // Fail closed: never recycle disks/netns behind a live VMM.
                std::mem::forget(self.metadata.take());
                std::mem::forget(self.slot.take());
                std::mem::forget(std::mem::take(&mut self.devices));
                std::mem::forget(std::mem::take(&mut self.dedicated_devices));
                std::mem::forget(self.work.clone());
                std::mem::forget(std::mem::take(&mut self.retained_artifacts));
                return;
            }
        }
        self.archive_failure();
        self.metadata = None; // join before slot release, including cancelled startup
        if self.devices.is_empty() && self.dedicated_devices.is_empty() && self.slot.is_none() {
            return;
        }
        let work = RetainedWork(Some(self.work.clone()));
        let devices = std::mem::take(&mut self.devices);
        let dedicated_devices = std::mem::take(&mut self.dedicated_devices);
        // Arm before spawn: a shutting-down runtime may drop an unpolled future.
        let artifacts = RetainedArtifacts(Some(std::mem::take(&mut self.retained_artifacts)));
        let slot = self.slot.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Try each independent lease once even when another release fails.
                // Keep backing if any device may still be live.
                let mut failed = false;
                for device in dedicated_devices {
                    if let Err(error) = UblkDeviceManager::global().delete_device(&device).await {
                        tracing::error!(%error, "recording device rollback; retaining artifacts");
                        failed = true;
                    }
                }
                for device in devices {
                    if let Err(error) = UblkDeviceManager::global().release_device(&device).await {
                        tracing::error!(%error, "runtime device rollback; retaining work directory");
                        failed = true;
                    }
                }
                if let Some(slot) = slot {
                    if let Err(error) = NetworkManager::global().release(slot) {
                        tracing::error!(%error, "runtime network rollback");
                        failed = true;
                    }
                }
                if failed {
                    return;
                }
                let mut work = work;
                work.0.take();
                let mut artifacts = artifacts;
                artifacts.0.take();
            });
        } else {
            // Runtime shutdown cannot service daemon RPCs: retain their artifacts.
            std::mem::forget(devices);
            std::mem::forget(dedicated_devices);
            drop(artifacts); // RetainedArtifacts keeps backing until confirmed release.
            if let Some(slot) = slot {
                let _ = NetworkManager::global().release(slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_device_cleanup_still_returns_network_slot_and_retains_backing() -> Result<()> {
        UblkDeviceManager::init_global(None).await;
        assert!(!UblkDeviceManager::global().is_available());
        let fixture = tempfile::tempdir()?;
        let mut resources = RuntimeResources::new(tempfile::tempdir_in(fixture.path())?, None);
        let backing = resources.work.path().to_path_buf();
        resources
            .dedicated_devices
            .push(crate::sandbox::ublk::unallocated_test_device());
        resources
            .devices
            .push(crate::sandbox::ublk::unallocated_test_device());
        let manager = NetworkManager::global_for_cleanup_test();
        let slot = manager.allocate_slot(302)?;
        let index = slot.idx;
        resources.slot = Some(slot);

        let error = resources.cleanup().await.unwrap_err();
        let replacement = manager
            .allocate_slot(index)
            .expect("device failure must not leak the network allocation");
        assert!(error.to_string().contains("dedicated device"));
        assert!(error.to_string().contains("runtime device"));
        assert!(resources.cleanup().await.is_err());
        drop(resources);
        assert!(manager.allocate_slot(index).is_err());
        manager.release(replacement)?;
        assert!(
            backing.exists(),
            "uncertain device release lost its backing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_device_cleanup_on_drop_still_returns_network_slot() -> Result<()> {
        UblkDeviceManager::init_global(None).await;
        assert!(!UblkDeviceManager::global().is_available());
        let fixture = tempfile::tempdir()?;
        let mut resources = RuntimeResources::new(tempfile::tempdir_in(fixture.path())?, None);
        let backing = resources.work.path().to_path_buf();
        resources
            .dedicated_devices
            .push(crate::sandbox::ublk::unallocated_test_device());
        let manager = NetworkManager::global_for_cleanup_test();
        let slot = manager.allocate_slot(303)?;
        let index = slot.idx;
        resources.slot = Some(slot);
        drop(resources);
        let replacement = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(slot) = manager.allocate_slot(index) {
                    break slot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("device rollback failure must not leak the network allocation");
        manager.release(replacement)?;
        assert!(backing.exists(), "uncertain rollback lost its backing");
        Ok(())
    }

    #[test]
    fn unpolled_cleanup_retains_device_backing_on_runtime_shutdown() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let backing = Arc::new(tempfile::tempdir_in(fixture.path()).unwrap());
        let backing_path = backing.path().to_owned();
        let work = tempfile::tempdir_in(fixture.path()).unwrap();
        let work_path = work.path().to_owned();
        {
            let _entered = runtime.enter();
            let mut resources = RuntimeResources::new(work, None);
            // This runtime is never polled: no daemon RPC may run for the sentinel.
            resources
                .devices
                .push(crate::sandbox::ublk::unallocated_test_device());
            resources.retained_artifacts.push(backing);
            drop(resources);
        }
        runtime.shutdown_background();
        assert!(work_path.exists());
        assert!(
            backing_path.exists(),
            "unreleased device lost its backing before cleanup was polled"
        );
    }

    #[tokio::test]
    async fn diagnostic_io_failure_does_not_fail_cleanup() {
        let fixture = tempfile::tempdir().unwrap();
        let evidence = fixture.path().join("not-a-directory");
        fs::write(&evidence, b"occupied").unwrap();
        let mut resources = RuntimeResources::new(tempfile::tempdir().unwrap(), Some(evidence));
        let retained = Arc::new(tempfile::tempdir().unwrap());
        let retained_path = retained.path().to_owned();
        resources.retained_artifacts.push(retained);
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owner = super::super::process::OwnedRunnerChild::from(child);
        resources.process = Some(RunnerProcessGuard {
            child: owner.clone(),
            logs: None,
            fail_blocking_reap: false,
        });
        resources.archive_failure();
        assert!(resources.cleanup().await.is_ok());
        assert!(owner.try_wait().unwrap().is_some());
        drop(resources);
        assert!(
            !retained_path.exists(),
            "diagnostic failure retained released artifacts"
        );
    }

    #[tokio::test]
    async fn normal_runtime_cleanup_does_not_archive_evidence() {
        let fixture = tempfile::tempdir().unwrap();
        let evidence = fixture.path().join("evidence");
        let mut resources =
            RuntimeResources::new(tempfile::tempdir().unwrap(), Some(evidence.clone()));
        fs::write(resources.work.path().join("serial.log"), b"normal stop").unwrap();
        resources.cleanup().await.unwrap();
        drop(resources);
        assert!(
            !evidence.exists(),
            "successful stop archived diagnostic files"
        );
    }

    #[test]
    fn failed_runtime_diagnostics_keep_bounded_log_tails() {
        let fixture = tempfile::tempdir().unwrap();
        let evidence = fixture.path().join("evidence");
        let resources = RuntimeResources::new(tempfile::tempdir().unwrap(), Some(evidence.clone()));
        let mut serial = vec![b'a'; 128 * 1024];
        serial.extend_from_slice(b"final failure");
        fs::write(resources.work.path().join("serial.log"), serial).unwrap();
        fs::write(
            resources.work.path().join("runner-error.log"),
            b"runner error",
        )
        .unwrap();
        drop(resources);
        let archive = evidence.join("0");
        let serial = fs::read(archive.join("serial.log")).unwrap();
        assert_eq!(serial.len(), 64 * 1024);
        assert!(serial.ends_with(b"final failure"));
        assert_eq!(
            fs::read(archive.join("runner-error.log")).unwrap(),
            b"runner error"
        );
    }

    #[test]
    fn failed_runtime_diagnostics_stop_at_capacity_without_removing_history() {
        let fixture = tempfile::tempdir().unwrap();
        let evidence = fixture.path().join("failures-v1");
        fs::create_dir_all(&evidence).unwrap();
        fs::write(evidence.join("historical-unowned"), b"preserve").unwrap();
        for index in 0..20 {
            let resources =
                RuntimeResources::new(tempfile::tempdir().unwrap(), Some(evidence.clone()));
            fs::write(resources.work.path().join("serial.log"), index.to_string()).unwrap();
            drop(resources);
        }
        assert_eq!(fs::read_dir(&evidence).unwrap().count(), 17);
        assert_eq!(fs::read(evidence.join("0/serial.log")).unwrap(), b"0");
        assert_eq!(
            fs::read(evidence.join("historical-unowned")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn diagnostic_file_failure_does_not_skip_other_evidence() {
        let fixture = tempfile::tempdir().unwrap();
        let resources = RuntimeResources::new(tempfile::tempdir().unwrap(), None);
        fs::create_dir(resources.work.path().join("serial.log")).unwrap();
        fs::write(
            resources.work.path().join("runner-error.log"),
            b"useful failure",
        )
        .unwrap();
        resources.archive_diagnostics(fixture.path());
        assert_eq!(
            fs::read(fixture.path().join("runner-error.log")).unwrap(),
            b"useful failure"
        );
    }

    #[tokio::test]
    async fn failed_reap_retains_shared_backing_and_restore_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let backing = Arc::new(tempfile::tempdir_in(fixture.path()).unwrap());
        let backing_observer = Arc::downgrade(&backing);
        let directory = Arc::new(tempfile::tempdir_in(fixture.path()).unwrap());
        let directory_path = directory.path().to_owned();
        let sealed_layers = Arc::new(tempfile::tempdir_in(fixture.path()).unwrap());
        let layers_observer = Arc::downgrade(&sealed_layers);
        let resources = RuntimeResources {
            process: Some(RunnerProcessGuard {
                child: child.into(),
                logs: None,
                fail_blocking_reap: true,
            }),
            metadata: None,
            devices: Vec::new(),
            slot: None,
            dedicated_devices: Vec::new(),
            work: Arc::new(tempfile::tempdir_in(fixture.path()).unwrap()),
            failure_evidence_root: None,
            retained_artifacts: vec![sealed_layers, backing, directory],
            cleanup_task: None,
        };
        drop(resources);
        let runner_alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        let backing_retained = backing_observer.strong_count() > 0;
        let directory_retained = directory_path.exists();
        let layers_retained = layers_observer.strong_count() > 0;
        // Always reap this injected fixture even if the ownership assertion fails.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
        }
        assert!(
            runner_alive,
            "fixture did not exercise an unconfirmed live runner"
        );
        assert!(
            backing_retained,
            "shared backing dropped before confirmed exit"
        );
        assert!(
            directory_retained,
            "restore artifact directory removed before confirmed exit"
        );
        assert!(
            layers_retained,
            "sealed lower leases dropped before confirmed exit"
        );
    }
}
