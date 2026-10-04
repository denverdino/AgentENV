use super::{
    envd::EnvdInstance, ublk::OverlaybdRuntimeHandle, Executor, ExtraDrive, ProcessOpts,
    ProcessOutput,
};
use anyhow::{Context, Result};
use std::{path::Path, time::Duration};

pub(crate) async fn set_volume_freeze(
    envd: EnvdInstance,
    mount: &Path,
    frozen_device: Option<&str>,
) -> Result<ProcessOutput> {
    let mount = mount
        .to_str()
        .context("volume mount path is not valid UTF-8")?;
    // Keep one opened filesystem across validation and the ioctl, so an
    // unmount cannot redirect fsfreeze to the root filesystem hosting envd.
    let script = r#"
        set -eu
        exec 3< "$1"
        device=$(/agentenv/bin/busybox stat -Lc %d /proc/self/fd/3)
        if [ "$2" = --freeze ]; then
            test "$device" != "$(/agentenv/bin/busybox stat -c %d /)"
        else
            test "$device" = "$3"
        fi
        /agentenv/bin/busybox fsfreeze "$2" /proc/self/fd/3
        printf '%s\n' "$device"
    "#;
    let operation = if frozen_device.is_some() {
        "--unfreeze"
    } else {
        "--freeze"
    };
    tokio::time::timeout(Duration::from_secs(30), async move {
        Executor::new(envd)
            .with_root_user()
            .run_command_with_opts(
                "/agentenv/bin/busybox",
                &[
                    "sh",
                    "-c",
                    script,
                    "freeze-volume",
                    mount,
                    operation,
                    frozen_device.unwrap_or_default(),
                ],
                &ProcessOpts::default().with_cwd("/"),
            )
            .await
    })
    .await
    .context("volume filesystem freeze/thaw timed out")?
}

pub(crate) async fn snapshot_persistent_volume_drives(
    drives: &[ExtraDrive],
    runtimes: &[OverlaybdRuntimeHandle],
) -> Result<()> {
    anyhow::ensure!(
        drives.len() == runtimes.len(),
        "extra drive bookkeeping mismatch: {} configured drives but {} prepared devices",
        drives.len(),
        runtimes.len()
    );
    for (drive, runtime) in drives.iter().zip(runtimes) {
        let Some(output_dir) = drive.snapshot_output_dir() else {
            continue;
        };
        if drive.read_only() {
            continue;
        }
        let layer_name = format!("snapshot-{}.commit", uuid::Uuid::now_v7().simple());
        super::storage_snapshot::restack_snapshot_overlaybd_device(
            &runtime.device,
            false,
            &runtime.image_config_path,
            output_dir,
            &layer_name,
            "volume",
        )
        .await
        .with_context(|| format!("snapshot persistent volume '{}'", drive.drive_id()))?;
    }
    Ok(())
}
