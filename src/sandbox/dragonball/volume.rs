use super::sandbox::{DragonballSandbox, RuntimeStarted};
use crate::sandbox::{
    volume_snapshot::{set_volume_freeze, snapshot_persistent_volume_drives},
    Executor, ExtraDrive, SandboxCaptureError, SandboxCaptureResult,
};
use anyhow::Context;
use std::{sync::atomic::Ordering, time::Duration};

#[derive(Clone, Copy)]
pub(super) enum VolumeOperation {
    Snapshot,
    Freeze,
    Thaw,
}

impl DragonballSandbox {
    pub(super) async fn volume_operation(
        &mut self,
        operation: VolumeOperation,
    ) -> SandboxCaptureResult<()> {
        let mut runtime = self.runtime.take().ok_or_else(|| {
            SandboxCaptureError::terminal(anyhow::anyhow!("Dragonball sandbox not running"))
        })?;
        let drives = self.spec.extra_drives.clone();
        let (runtime, mut result) = tokio::spawn(async move {
            let mut result = runtime.volume_operation(&drives, operation).await;
            if result.as_ref().is_err_and(|error| !error.is_terminal()) {
                if let Err(error) = runtime.confirm_running().await {
                    result = Err(SandboxCaptureError::terminal(error));
                }
            }
            (runtime, result)
        })
        .await
        .map_err(|error| SandboxCaptureError::terminal(error.into()))?;
        self.runtime = Some(runtime);
        if result.as_ref().is_err_and(SandboxCaptureError::is_terminal) {
            if let Err(error) = self.stop_runtime(true).await {
                result = Err(SandboxCaptureError::terminal(anyhow::anyhow!(
                    "volume operation failed; teardown not confirmed: {error:#}"
                )));
            }
        }
        result
    }
}

impl RuntimeStarted {
    pub(super) async fn sync_writable_volume_filesystems(&self) -> SandboxCaptureResult<()> {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Executor::new(self.envd.clone())
                .with_root_user()
                .run_command("/agentenv/bin/busybox", &["sync"]),
        )
        .await
        .context("guest volume sync timed out")
        .and_then(|output| output)?;
        if output.exit_code != 0 {
            return Err(SandboxCaptureError::recoverable(anyhow::anyhow!(
                "guest volume sync failed: {}",
                output.stderr.trim()
            )));
        }
        Ok(())
    }

    async fn thaw_volume_mounts(&mut self) -> SandboxCaptureResult<()> {
        while let Some((mount, device)) = self.frozen_volume_mounts.last().cloned() {
            let output = set_volume_freeze(self.envd.clone(), &mount, Some(&device))
                .await
                .map_err(SandboxCaptureError::terminal)?;
            if output.exit_code != 0 {
                return Err(SandboxCaptureError::recoverable(anyhow::anyhow!(
                    "thaw volume {} failed: {}",
                    mount.display(),
                    output.stderr.trim()
                )));
            }
            self.frozen_volume_mounts.pop();
        }
        Ok(())
    }

    async fn volume_operation(
        &mut self,
        drives: &[ExtraDrive],
        operation: VolumeOperation,
    ) -> SandboxCaptureResult<()> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(SandboxCaptureError::terminal(anyhow::anyhow!(
                "volume capture requires a Ready source"
            )));
        }
        if matches!(operation, VolumeOperation::Thaw) {
            return self.thaw_volume_mounts().await;
        }
        if !self.frozen_volume_mounts.is_empty() {
            return Err(SandboxCaptureError::recoverable(anyhow::anyhow!(
                "volume filesystems are already frozen"
            )));
        }
        let mounts = drives
            .iter()
            .filter(|drive| !drive.read_only() && drive.snapshot_output_dir().is_some())
            .map(|drive| drive.mount_path().to_path_buf())
            .collect::<Vec<_>>();
        if mounts.is_empty() {
            return Ok(());
        }
        if matches!(operation, VolumeOperation::Freeze) {
            for mount in mounts {
                self.frozen_volume_mounts
                    .push((mount.clone(), String::new()));
                let output = set_volume_freeze(self.envd.clone(), &mount, None)
                    .await
                    .map_err(SandboxCaptureError::terminal)?;
                if output.exit_code != 0 {
                    self.frozen_volume_mounts.pop();
                    self.thaw_volume_mounts().await.map_err(|error| {
                        SandboxCaptureError::terminal(anyhow::anyhow!(
                            "freeze refused and rollback thaw failed: {error:#}"
                        ))
                    })?;
                    return Err(SandboxCaptureError::recoverable(anyhow::anyhow!(
                        "freeze volume {} failed: {}",
                        mount.display(),
                        output.stderr.trim()
                    )));
                }
                self.frozen_volume_mounts.last_mut().unwrap().1 = output.stdout.trim().into();
            }
        } else {
            self.sync_writable_volume_filesystems().await?;
        }
        let report = match self.begin_capture().await {
            Ok(report) => report,
            Err(error) => {
                if !error.is_terminal() {
                    self.thaw_volume_mounts().await.map_err(|thaw| {
                        SandboxCaptureError::terminal(anyhow::anyhow!(
                            "capture rejected and rollback thaw failed: {thaw:#}"
                        ))
                    })?;
                }
                return Err(error);
            }
        };
        snapshot_persistent_volume_drives(drives, &self.extras)
            .await
            .map_err(SandboxCaptureError::terminal)?;
        self.release_source(report.token)
            .await
            .map_err(SandboxCaptureError::terminal)?;
        Ok(())
    }
}
