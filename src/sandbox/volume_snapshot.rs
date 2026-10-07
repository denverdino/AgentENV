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

/// Seal all disks at the VM capture cut, then lease immutable volume copies.
/// Catalog image.json is updated for future mounts but is never a capture artifact.
pub(crate) async fn capture_extra_drives(
    drives: &[ExtraDrive],
    runtimes: &[OverlaybdRuntimeHandle],
    at: &Path,
    runtime_roots: &[std::path::PathBuf],
) -> Result<Vec<ExtraDrive>> {
    use super::storage_snapshot::{
        restack_snapshot_overlaybd_device_with_runtime_roots,
        stage_overlaybd_snapshot_from_live_runtime,
    };
    use anyhow::ensure;
    use std::fs;
    ensure!(
        drives.len() == runtimes.len(),
        "extra drive bookkeeping conflict"
    );
    let mut extras = vec![];
    for (drive, runtime) in drives.iter().zip(runtimes) {
        let mut drive_roots = runtime_roots.to_vec();
        if drive.is_volume() {
            // Catalog paths are mutable and may be explicitly deleted.
            // The capture owns its own immutable copies of these lowers.
            let parent = drive
                .image_config_path()
                .parent()
                .context("volume config parent")?;
            drive_roots.push(fs::canonicalize(parent)?);
        }
        let destination = drive
            .snapshot_output_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| at.join("drives").join(drive.drive_id()));
        let layer_name = drive.snapshot_output_dir().map_or_else(
            || "snapshot.commit".to_owned(),
            |_| format!("snapshot-{}.commit", uuid::Uuid::new_v4()),
        );
        let mut config = restack_snapshot_overlaybd_device_with_runtime_roots(
            &runtime.device,
            drive.read_only(),
            &runtime.image_config_path,
            &destination,
            &layer_name,
            "drive",
            &drive_roots,
        )
        .await?;
        if drive.is_volume() && drive.snapshot_output_dir().is_some() {
            drive_roots.push(fs::canonicalize(&destination)?);
            config = stage_overlaybd_snapshot_from_live_runtime(
                &config,
                &at.join("drives").join(drive.drive_id()),
                None,
                &drive_roots,
            )
            .await?;
        }
        extras.push(
            drive
                .with_image_config_path(config)
                .try_with_virtual_size(runtime.actual_virtual_size)?,
        );
    }
    Ok(extras)
}

/// Child allocation chooses ownership; the captured generation chooses contents.
pub(crate) async fn bind_fork_drives(
    source: &[ExtraDrive],
    child: &super::SandboxForkSpec,
) -> Result<Vec<ExtraDrive>> {
    use anyhow::ensure;
    let mut drives = source.to_vec();
    let mut replaced = std::collections::HashSet::new();
    let mut replacement_ids = std::collections::HashSet::new();
    for (old_id, new_id) in &child.replace_drive_ids {
        ensure!(
            replaced.insert(old_id) && replacement_ids.insert(new_id),
            "duplicate volume replacement"
        );
        let captured = drives
            .iter_mut()
            .find(|drive| drive.drive_id() == old_id)
            .context("replacement is not a captured device")?;
        ensure!(
            captured.is_volume(),
            "cannot replace captured attached disk"
        );
        let replacement = child
            .extra_drives
            .iter()
            .find(|drive| drive.drive_id() == new_id)
            .context("replacement volume was not supplied")?;
        ensure!(
            replacement.is_volume()
                && replacement.virtual_size() == captured.virtual_size()
                && replacement.read_only() == captured.read_only()
                && replacement.mount_path() == captured.mount_path()
                && replacement.sub_path() == captured.sub_path(),
            "replacement changes captured capacity/features/mount topology"
        );
        // Allocation supplies identity/ownership, never the consistency cut.
        // Adopt the actual capture's immutable lowers into the child catalog
        // directory before its runtime opens a private writable upper.
        let target = replacement.image_config_path();
        overlaybd::config::load_image_config(target)
            .context("load allocated child volume backing")?;
        let source = captured.image_config_path();
        ensure!(
            std::fs::canonicalize(source)? != std::fs::canonicalize(target)?,
            "child volume aliases captured backing"
        );
        let source_root =
            std::fs::canonicalize(source.parent().context("captured volume parent")?)?;
        let staging = target
            .parent()
            .context("child volume parent")?
            .join(format!("fork-{}", uuid::Uuid::new_v4()));
        let config = crate::sandbox::storage_snapshot::stage_overlaybd_snapshot_from_live_runtime(
            source,
            &staging,
            None,
            &[source_root],
        )
        .await?;
        crate::sandbox::storage_snapshot::write_bytes_atomically(
            target,
            &tokio::fs::read(config).await?,
            "fork volume backing",
        )?;
        *captured = replacement.clone();
    }
    for drive in &child.extra_drives {
        let bound = drives
            .iter_mut()
            .find(|bound| bound.drive_id() == drive.drive_id())
            .context("child drive is outside captured topology")?;
        ensure!(
            bound.is_volume()
                && drive.is_volume()
                && bound.virtual_size() == drive.virtual_size()
                && bound.read_only() == drive.read_only()
                && bound.mount_path() == drive.mount_path()
                && bound.sub_path() == drive.sub_path(),
            "child override changes captured volume topology"
        );
        // Replacements above already carry the adopted generation. Shared RO
        // volumes retain this capture's leased copy, not a mutable catalog path.
        if !replacement_ids.contains(&drive.drive_id().to_string()) {
            *bound = drive.with_image_config_path(bound.image_config_path().to_path_buf());
        }
    }
    for drive in source {
        ensure!(
            !drive.is_volume()
                || drive.read_only()
                || replaced.contains(&drive.drive_id().to_string()),
            "writable volume requires an independently owned child replacement"
        );
    }

    Ok(drives)
}

#[cfg(test)]
mod tests {
    use super::super::SandboxForkSpec;
    use super::*;

    fn request() -> SandboxForkSpec {
        SandboxForkSpec {
            sandbox_id: crate::types::SandboxId::new(),
            envd_access_token: None,
            extra_drives: vec![],
            replace_drive_ids: vec![],
        }
    }

    #[tokio::test]
    async fn fork_adopts_capture_generation_and_preserves_shared_readonly_copy() -> Result<()> {
        let root = tempfile::tempdir()?;
        let captured = root.path().join("capture");
        let allocated = root.path().join("child");
        std::fs::create_dir_all(&captured)?;
        std::fs::create_dir_all(&allocated)?;
        let lower = captured.join("layer.commit");
        std::fs::write(&lower, b"latest capture")?;
        let source_config = captured.join("image.json");
        std::fs::write(
            &source_config,
            serde_json::to_vec(&serde_json::json!({"lowers": [{"file": lower}]}))?,
        )?;
        let target = allocated.join("image.json");
        let old = allocated.join("old.commit");
        std::fs::write(&old, b"stale allocation")?;
        std::fs::write(
            &target,
            serde_json::to_vec(&serde_json::json!({"lowers": [{"file": old}]}))?,
        )?;
        let source = ExtraDrive::try_new_overlaybd_with_mount_path(
            "source",
            source_config,
            false,
            "/mnt/data",
            None::<std::path::PathBuf>,
        )?
        .with_volume_snapshot_output_dir(None)
        .try_with_virtual_size(128 << 20)?;
        let replacement = ExtraDrive::try_new_overlaybd_with_mount_path(
            "child",
            target.clone(),
            false,
            "/mnt/data",
            None::<std::path::PathBuf>,
        )?
        .with_volume_snapshot_output_dir(None)
        .try_with_virtual_size(128 << 20)?;
        let mut child = request();
        child.extra_drives = vec![replacement];
        child.replace_drive_ids = vec![("source".into(), "child".into())];
        let bound = bind_fork_drives(&[source], &child).await?;
        assert_eq!(bound[0].image_config_path(), target);
        let config = overlaybd::config::load_image_config(&target)?;
        assert_eq!(config.lowers.len(), 1);
        let adopted = std::path::Path::new(&config.lowers[0].file);
        assert!(adopted.starts_with(&allocated));
        std::fs::remove_dir_all(captured)?;
        assert_eq!(std::fs::read(adopted)?, b"latest capture");

        let ro = ExtraDrive::try_new_overlaybd("readonly", "capture/readonly.json", true)?
            .with_volume_snapshot_output_dir(None);
        let mut child = request();
        child.extra_drives = vec![ro.with_image_config_path("catalog/image.json".into())];
        let bound = bind_fork_drives(std::slice::from_ref(&ro), &child).await?;
        assert_eq!(bound[0].image_config_path(), ro.image_config_path());
        Ok(())
    }

    #[tokio::test]
    async fn fork_rejects_topology_changes_before_opening_child_backing() -> Result<()> {
        let source = ExtraDrive::try_new_overlaybd_with_mount_path(
            "source",
            "capture.json",
            false,
            "/mnt/data",
            None::<std::path::PathBuf>,
        )?
        .with_volume_snapshot_output_dir(None)
        .try_with_virtual_size(128 << 20)?;
        for (read_only, mount, size) in [
            (true, "/mnt/data", 128),
            (false, "/mnt/other", 128),
            (false, "/mnt/data", 256),
        ] {
            let replacement = ExtraDrive::try_new_overlaybd_with_mount_path(
                "child",
                "missing-child.json",
                read_only,
                mount,
                None::<std::path::PathBuf>,
            )?
            .with_volume_snapshot_output_dir(None)
            .try_with_virtual_size(size << 20)?;
            let mut child = request();
            child.extra_drives = vec![replacement];
            child.replace_drive_ids = vec![("source".into(), "child".into())];
            let error = bind_fork_drives(std::slice::from_ref(&source), &child)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("topology"), "{error:#}");
        }
        Ok(())
    }
}
