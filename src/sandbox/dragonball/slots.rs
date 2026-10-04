use anyhow::{ensure, Context, Result};

use crate::sandbox::{manifest::SnapshotVolumeSlot, ExtraDrive};

/// Resolve by guest mount, never catalog ID or capacity. Existing mounts retain
/// their filesystem geometry; only empty, explicit slots accept a new geometry.
pub(super) fn bind(
    saved: &[SnapshotVolumeSlot],
    mut volumes: Vec<ExtraDrive>,
) -> Result<(Vec<SnapshotVolumeSlot>, Vec<ExtraDrive>)> {
    ensure!(
        volumes.len() <= saved.len(),
        "volume binding count conflict"
    );
    let mut mounts = std::collections::HashSet::new();
    for drive in &volumes {
        crate::sandbox::extra_drive::validate_mount_path(drive.mount_path())?;
        if let Some(sub_path) = drive.sub_path() {
            crate::sandbox::extra_drive::validate_sub_path(sub_path)?;
        }
        ensure!(
            drive.is_volume() && mounts.insert(drive.mount_path()),
            "duplicate or non-volume slot binding"
        );
        ensure!(
            drive
                .virtual_size()
                .is_some_and(|size| size > 0 && size % 512 == 0),
            "invalid volume capacity"
        );
    }
    volumes.sort_by(|a, b| a.mount_path().cmp(b.mount_path()));
    let mut slots = saved.to_vec();
    let mut ordered = Vec::new();
    // Bound slots form a prefix: adding a volume never moves an existing disk.
    let mut empty_seen = false;
    for slot in &mut slots {
        let drive = if let Some(mount) = &slot.mount_path {
            ensure!(!empty_seen, "noncontiguous bound volume slots");
            let index = volumes
                .iter()
                .position(|drive| drive.mount_path() == mount)
                .context("missing captured volume mount binding")?;
            let drive = volumes.remove(index);
            ensure!(
                drive.virtual_size() == Some(slot.capacity_bytes)
                    && drive.sub_path() == slot.sub_path.as_deref(),
                "mounted volume geometry conflict"
            );
            drive
        } else {
            empty_seen = true;
            if volumes.is_empty() {
                continue;
            }
            volumes.remove(0)
        };
        ensure!(
            !slot.guest_read_only || drive.read_only(),
            "read-only guest slot cannot become writable"
        );
        slot.mount_path = Some(drive.mount_path().into());
        slot.sub_path = drive.sub_path().map(Into::into);
        slot.capacity_bytes = drive.virtual_size().unwrap();
        ordered.push(drive);
    }
    ensure!(volumes.is_empty(), "unused volume binding");
    Ok((slots, ordered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(mount: Option<&str>, size: u64, ro: bool) -> SnapshotVolumeSlot {
        SnapshotVolumeSlot {
            slot_id: "agentenv_volume_slot_0".into(),
            mount_path: mount.map(Into::into),
            sub_path: None,
            guest_read_only: ro,
            capacity_bytes: size,
        }
    }

    fn volume(mount: &str, size: u64, ro: bool) -> ExtraDrive {
        ExtraDrive::Overlaybd {
            drive_id: "catalog_id".into(),
            image_config_path: "/unused/preflight.json".into(),
            read_only: ro,
            mount_path: mount.into(),
            virtual_size: Some(size),
            sub_path: None,
            snapshot_output_dir: None,
            volume: true,
        }
    }

    #[test]
    fn only_empty_slots_accept_new_capacity() {
        for size in [64 << 20, 128 << 20] {
            let (bound, _) = bind(
                &[slot(None, 4096, false)],
                vec![volume("/mnt/data", size, false)],
            )
            .unwrap();
            assert_eq!(bound[0].capacity_bytes, size);
            assert_eq!(
                bound[0].mount_path.as_deref(),
                Some(std::path::Path::new("/mnt/data"))
            );
        }
        assert!(bind(
            &[slot(Some("/mnt/data"), 128 << 20, false)],
            vec![volume("/mnt/data", 64 << 20, false)]
        )
        .is_err());
    }

    #[test]
    fn read_only_guest_slot_cannot_upgrade_features() {
        assert!(bind(
            &[slot(None, 4096, true)],
            vec![volume("/mnt/data", 64 << 20, false)]
        )
        .is_err());
        let (bound, _) = bind(
            &[slot(None, 4096, false)],
            vec![volume("/mnt/data", 64 << 20, true)],
        )
        .unwrap();
        assert!(
            !bound[0].guest_read_only,
            "RO backing must not alter negotiated guest features"
        );
    }

    #[test]
    fn equal_capacity_binding_uses_mount_identity() {
        let mut second = slot(Some("/mnt/b"), 64 << 20, false);
        second.slot_id = "agentenv_volume_slot_1".into();
        let first = slot(Some("/mnt/a"), 64 << 20, false);
        let (_, ordered) = bind(
            &[first, second],
            vec![
                volume("/mnt/b", 64 << 20, false),
                volume("/mnt/a", 64 << 20, false),
            ],
        )
        .unwrap();
        assert_eq!(ordered[0].mount_path(), std::path::Path::new("/mnt/a"));
        assert_eq!(ordered[1].mount_path(), std::path::Path::new("/mnt/b"));
    }
}
