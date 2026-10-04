//! Shared OverlayBD snapshot sealing and runtime restacking.
//!
//! During pause, [`restack_snapshot_overlaybd_rootfs`] either stages a
//! read-only runtime config as-is or asks the ublk daemon to
//! `close_seal + restack` the live upper before writing the persisted
//! snapshot config.
//!
//! Sealed layers always stay raw locally: compression, when enabled via
//! `[snapshot.publish_compression]`, happens once at publish time when the
//! snapshot repository uploads layers to OSS/ACR.
//!
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{Context, Result};
use overlaybd::config::{ImageConfig, LayerConfig};
use tracing::{debug, warn};

use crate::cfg::ConfigManager;
use crate::sandbox::ublk::UblkDevice;
use crate::sandbox::ublk::{compact_layers, OverlaybdCompactOutput, UblkDeviceManager};
use crate::sandbox::SandboxCaptureError;

// Preserve the existing host layout: this is shared artifact storage, not a
// hypervisor identity or snapshot format identifier.
pub(crate) fn managed_snapshot_base() -> PathBuf {
    ConfigManager::global_config()
        .firecracker
        .work_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("aenv"))
        .join("managed-snapshots")
}

#[derive(Debug)]
pub(crate) struct PersistentSnapshotRootGuard {
    path: PathBuf,
    persisted: AtomicBool,
}

impl PersistentSnapshotRootGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            persisted: AtomicBool::new(false),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(feature = "hypervisor-dragonball")]
    pub(crate) fn persistence_committed(&self) {
        self.persisted.store(true, Ordering::Release);
    }

    pub(crate) async fn prepare(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.path)
            .await
            .with_context(|| format!("create managed snapshot root {}", self.path.display()))
    }
}

impl Drop for PersistentSnapshotRootGuard {
    fn drop(&mut self) {
        if self.persisted.load(Ordering::Acquire) {
            return;
        }
        if let Err(err) = std::fs::remove_dir_all(&self.path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(snapshot_root = %self.path.display(), error = %err,
                    "failed to clean up managed snapshot root");
            }
        }
    }
}

/// Owns the captured directory and sealed local layers, independently of the
/// source VM. Backend artifacts retain their rootfs/extra/volume descriptors
/// alongside this lease; live device leases remain with the running VM.
#[derive(Debug)]
pub(crate) struct StorageCaptureOwner {
    #[cfg_attr(not(feature = "hypervisor-firecracker"), allow(dead_code))]
    pub(crate) snapshot_dir: PathBuf,
    pub(crate) snapshot_root: Arc<PersistentSnapshotRootGuard>,
}

impl StorageCaptureOwner {
    pub(crate) fn new(
        snapshot_dir: PathBuf,
        snapshot_root: Arc<PersistentSnapshotRootGuard>,
    ) -> Self {
        Self {
            snapshot_dir,
            snapshot_root,
        }
    }
}

/// Base budget of runtime-owned overlaybd snapshot lowers before compaction
/// triggers; the effective budget shrinks as the stable prefix grows. Well
/// below the hard limit of 255 (`MAX_STACK_LAYERS`) in overlaybd.
const DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS: usize = 32;
const INHERITED_LAYERS_DIR: &str = "inherited-layers";
const MANAGED_BASE_LAYER_FILE: &str = "managed-base.commit";

enum LiveOverlaybdSnapshotState {
    ReadOnly,
    Restacked(PathBuf),
}

impl LiveOverlaybdSnapshotState {
    fn snapshot_layer_path(&self) -> Option<&Path> {
        match self {
            Self::ReadOnly => None,
            Self::Restacked(path) => Some(path.as_path()),
        }
    }

    fn finish_staging(self, staged_snapshot: Result<PathBuf>) -> Result<PathBuf> {
        match self {
            Self::ReadOnly => staged_snapshot,
            Self::Restacked(_) => staged_snapshot.map_err(into_terminal_snapshot_failure),
        }
    }
}

fn into_terminal_snapshot_failure(err: anyhow::Error) -> anyhow::Error {
    SandboxCaptureError::terminal(err).into()
}

fn local_layer_config(path: &Path) -> LayerConfig {
    local_layer_config_with_descriptor(path, None)
}

fn local_layer_config_with_descriptor(
    path: &Path,
    descriptor: Option<&overlaybd::LayerDescriptor>,
) -> LayerConfig {
    LayerConfig {
        file: path.display().to_string(),
        digest: descriptor
            .map(|descriptor| descriptor.digest.clone())
            .unwrap_or_default(),
        size: descriptor.map(|descriptor| descriptor.size).unwrap_or(0),
        ..Default::default()
    }
}

pub(crate) fn canonicalized_runtime_owned_roots() -> &'static [PathBuf] {
    static RUNTIME_OWNED_ROOTS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    RUNTIME_OWNED_ROOTS
        .get_or_init(|| {
            let config = ConfigManager::global_config();
            [
                managed_snapshot_base(),
                config.orchestrator.persisted_sandbox_store_path.clone(),
            ]
            .into_iter()
            .map(|root| fs::canonicalize(&root).unwrap_or(root))
            .collect::<Vec<_>>()
        })
        .as_slice()
}

/// Split a list of overlaybd lowers into two parts: the prefix of stable lowers and the suffix of runtime-owned lowers.
///
/// Runtime-created local lowers are appended during resume/fork/pause.
/// Published or cache-owned layers must be materialized before that
/// suffix; otherwise adopting only the tail would either leave a dangling
/// runtime artifact reference or change lower precedence.
fn split_runtime_suffix(
    mut lowers: Vec<LayerConfig>,
    runtime_owned_roots: &[PathBuf],
) -> (Vec<LayerConfig>, Vec<LayerConfig>) {
    let Some(first_runtime_owned_index) = lowers.iter().position(|lower| {
        let lower_path =
            fs::canonicalize(&lower.file).unwrap_or_else(|_| PathBuf::from(&lower.file));
        runtime_owned_roots
            .iter()
            .any(|root| lower_path.starts_with(root))
    }) else {
        return (lowers, Vec::new());
    };

    let runtime_owned_suffix = lowers.split_off(first_runtime_owned_index);

    debug_assert!(lowers.iter().all(|lower| {
        let lower_path =
            fs::canonicalize(&lower.file).unwrap_or_else(|_| PathBuf::from(&lower.file));
        runtime_owned_roots
            .iter()
            .all(|root| !lower_path.starts_with(root))
    }));

    (lowers, runtime_owned_suffix)
}

fn link_or_copy_file_blocking(source: &Path, destination: &Path) -> std::io::Result<()> {
    if source == destination {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::remove_file(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    if let Err(error) = fs::hard_link(source, destination) {
        warn!(
            source = %source.display(),
            destination = %destination.display(),
            error = %error,
            "hard-link runtime overlaybd layer failed; falling back to sparse copy"
        );
    } else {
        return Ok(());
    }

    let mut cmd = std::process::Command::new("cp");
    if cfg!(target_os = "linux") {
        cmd.arg("--reflink=auto").arg("--sparse=always");
    }
    let status = cmd.arg(source).arg(destination).status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            let _ = fs::remove_file(destination);
            Err(std::io::Error::other(format!(
                "sparse copy command failed with status {status}"
            )))
        }
        Err(error) => {
            let _ = fs::remove_file(destination);
            Err(error)
        }
    }
}

async fn link_or_copy_runtime_layer(source: &Path, destination: &Path) -> Result<()> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let description = format!(
        "link or copy runtime overlaybd layer {} -> {}",
        source.display(),
        destination.display()
    );
    tokio::task::spawn_blocking(move || link_or_copy_file_blocking(&source, &destination))
        .await
        .context("link runtime overlaybd layer task failed")?
        .with_context(|| description)
}

async fn prepare_specific_snapshot_layer_path(snapshot_layer_path: &Path) -> Result<()> {
    match tokio::fs::remove_file(&snapshot_layer_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "remove stale overlaybd snapshot layer {}",
                    snapshot_layer_path.display()
                )
            })
        }
    }
    Ok(())
}

fn load_existing_image_config(
    image_config_path: Option<&Path>,
    description: &str,
) -> Result<ImageConfig> {
    let Some(image_config_path) = image_config_path else {
        return Ok(ImageConfig::default());
    };

    let image_config =
        overlaybd::config::load_image_config(image_config_path).with_context(|| {
            format!(
                "load {description} image config {}",
                image_config_path.display()
            )
        })?;
    Ok(image_config)
}

fn restack_target_upper_data_path(live_runtime_image_config_path: &Path) -> Result<PathBuf> {
    let image_config = overlaybd::config::load_image_config(live_runtime_image_config_path)
        .with_context(|| {
            format!(
                "load live overlaybd runtime config {}",
                live_runtime_image_config_path.display()
            )
        })?;
    if image_config.upper.data.is_empty() {
        anyhow::bail!(
            "restack snapshot requires writable upper.data in live runtime config {}",
            live_runtime_image_config_path.display()
        );
    }
    Ok(PathBuf::from(image_config.upper.data))
}

fn on_same_filesystem(src_path: &Path, dst_dir: &Path) -> Result<bool> {
    let src_meta = fs::metadata(src_path)
        .with_context(|| format!("stat restack source path {}", src_path.display()))?;
    let dst_meta = fs::metadata(dst_dir)
        .with_context(|| format!("stat restack destination dir {}", dst_dir.display()))?;
    Ok(src_meta.dev() == dst_meta.dev())
}

pub(crate) fn write_bytes_atomically(path: &Path, bytes: &[u8], description: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "{description} path has no parent directory: {}",
            path.display()
        )
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temp {description} in {}", parent.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("write temp {description} {}", temp.path().display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temp {description} {}", temp.path().display()))?;
    let temp_path = temp.path().to_path_buf();
    temp.persist(path).map_err(|error| {
        anyhow::Error::new(error.error).context(format!(
            "persist {description} {} -> {}",
            temp_path.display(),
            path.display()
        ))
    })?;
    Ok(())
}

async fn copy_file_atomically(src: &Path, dst: &Path, description: &str) -> Result<()> {
    let parent = dst.parent().with_context(|| {
        format!(
            "{description} destination has no parent directory: {}",
            dst.display()
        )
    })?;
    let temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temp {description} in {}", parent.display()))?;
    let temp_path = temp.path().to_path_buf();
    tokio::fs::copy(src, &temp_path).await.with_context(|| {
        format!(
            "copy {description} {} -> {}",
            src.display(),
            temp_path.display()
        )
    })?;
    std::fs::File::open(&temp_path)
        .with_context(|| format!("open temp {description} {}", temp_path.display()))?
        .sync_all()
        .with_context(|| format!("sync temp {description} {}", temp_path.display()))?;
    temp.persist(dst).map_err(|error| {
        anyhow::Error::new(error.error).context(format!(
            "persist {description} {} -> {}",
            temp_path.display(),
            dst.display()
        ))
    })?;
    Ok(())
}

async fn rewrite_live_runtime_config_for_restack(
    live_runtime_image_config_path: &Path,
    snapshot_layer_path: &Path,
    descriptor: Option<&overlaybd::LayerDescriptor>,
) -> Result<()> {
    let mut image_config = overlaybd::config::load_image_config(live_runtime_image_config_path)
        .with_context(|| {
            format!(
                "load live overlaybd runtime config {}",
                live_runtime_image_config_path.display()
            )
        })?;
    image_config.lowers.push(local_layer_config_with_descriptor(
        snapshot_layer_path,
        descriptor,
    ));
    let bytes = serde_json::to_vec_pretty(&image_config)
        .context("serialize rewritten live overlaybd runtime config")?;
    write_bytes_atomically(
        live_runtime_image_config_path,
        &bytes,
        "live overlaybd runtime config",
    )?;
    Ok(())
}

pub(crate) async fn stage_overlaybd_snapshot_from_live_runtime(
    live_runtime_image_config_path: &Path,
    output_dir: &Path,
    snapshot_layer_path: Option<&Path>,
    runtime_owned_roots: &[PathBuf],
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("create overlaybd snapshot dir {}", output_dir.display()))?;

    let mut image_config = overlaybd::config::load_image_config(live_runtime_image_config_path)
        .with_context(|| {
            format!(
                "load overlaybd rootfs image config {}",
                live_runtime_image_config_path.display()
            )
        })?;

    let appended_layer = if let Some(snapshot_layer_path) = snapshot_layer_path {
        let expected_snapshot_path = fs::canonicalize(snapshot_layer_path)
            .unwrap_or_else(|_| snapshot_layer_path.to_path_buf());
        let Some(latest_lower) = image_config.lowers.last() else {
            anyhow::bail!(
                "restack snapshot expected latest lower to be the newly claimed sealed upper layer"
            );
        };
        let actual_snapshot_path = fs::canonicalize(&latest_lower.file)
            .unwrap_or_else(|_| PathBuf::from(&latest_lower.file));
        anyhow::ensure!(
            actual_snapshot_path == expected_snapshot_path,
            "restack snapshot expected latest lower {} but found {}",
            expected_snapshot_path.display(),
            actual_snapshot_path.display()
        );
        Some(image_config.lowers.pop().expect("checked latest lower"))
    } else {
        None
    };

    let rewritten_lowers = rewrite_lowers_with_runtime_roots(
        image_config.lowers,
        output_dir,
        appended_layer,
        MANAGED_BASE_LAYER_FILE,
        runtime_owned_roots,
        // Sealed rootfs layers always stay raw; compression happens once at
        // publish time under `[snapshot.publish_compression]`.
        OverlaybdCompactOutput::Raw,
    )
    .await?;
    image_config.lowers = rewritten_lowers;
    image_config.upper = Default::default();
    image_config.result_file = "./result.txt".to_string();

    let output_path = output_dir.join("image.json");
    let bytes = serde_json::to_vec_pretty(&image_config)
        .context("serialize overlaybd rootfs image config with inherited runtime layers")?;
    write_bytes_atomically(&output_path, &bytes, "overlaybd rootfs image config")?;
    Ok(output_path)
}

async fn rewrite_lowers_with_owned_runtime_suffix(
    existing_lowers: Vec<LayerConfig>,
    output_dir: &Path,
    appended_layer: Option<LayerConfig>,
    compaction_output_name: &'static str,
    compaction_output: OverlaybdCompactOutput,
) -> Result<Vec<LayerConfig>> {
    rewrite_lowers_with_runtime_roots(
        existing_lowers,
        output_dir,
        appended_layer,
        compaction_output_name,
        canonicalized_runtime_owned_roots(),
        compaction_output,
    )
    .await
}

async fn rewrite_lowers_with_runtime_roots(
    existing_lowers: Vec<LayerConfig>,
    output_dir: &Path,
    appended_layer: Option<LayerConfig>,
    compaction_output_name: &'static str,
    runtime_owned_roots: &[PathBuf],
    compaction_output: OverlaybdCompactOutput,
) -> Result<Vec<LayerConfig>> {
    let (mut lowers, mut runtime_owned_lowers) =
        split_runtime_suffix(existing_lowers, runtime_owned_roots);

    // Compact the runtime-owned suffix plus the appended layer once their count
    // exceeds a budget that shrinks as the stable prefix grows: the full
    // `DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS` with an empty prefix, one less per
    // 4 prefix lowers (e.g. 24 with a 32-layer prefix), clamped to 1 so a lone
    // layer is never pointlessly rewritten into itself. This keeps the total
    // stack well below overlaybd's 255-layer hard limit without compacting on
    // every pause when the prefix is already large.
    let max_runtime_owned_layers = DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS
        .saturating_sub(lowers.len() / 4)
        .max(1);
    let compactable_count = runtime_owned_lowers.len() + usize::from(appended_layer.is_some());
    if compactable_count > max_runtime_owned_layers {
        let mut runtime_suffix = runtime_owned_lowers;
        if let Some(layer) = appended_layer {
            runtime_suffix.push(layer);
        }
        if runtime_suffix.iter().any(|lower| lower.file.is_empty()) {
            anyhow::bail!("cannot compact runtime-owned overlaybd suffix with remote lowers");
        }
        if let Some(compacted_path) = compact_layers(
            &runtime_suffix,
            &output_dir.join(compaction_output_name),
            compaction_output,
        )
        .await?
        {
            lowers.push(local_layer_config(&compacted_path));
        }
        return Ok(lowers);
    }

    // Otherwise, adopt the runtime-owned suffix into the snapshot artifact dir.
    let inherited_layers_dir = output_dir.join(INHERITED_LAYERS_DIR);
    for (index, lower) in runtime_owned_lowers.iter_mut().enumerate() {
        let source = Path::new(&lower.file);
        let destination = inherited_layers_dir.join(format!("{index:04}")).join(
            source
                .file_name()
                .unwrap_or_else(|| OsStr::new("runtime-layer.commit")),
        );
        link_or_copy_runtime_layer(source, &destination).await?;
        lower.file = destination.display().to_string();
    }
    lowers.extend(runtime_owned_lowers);

    if let Some(layer) = appended_layer {
        lowers.push(layer);
    }

    Ok(lowers)
}

async fn capture_live_overlaybd_snapshot(
    ublk_device: &UblkDevice,
    read_only: bool,
    live_runtime_image_config_path: &Path,
    output_dir: &Path,
    snapshot_layer_file_name: &str,
    kind: &'static str,
) -> Result<LiveOverlaybdSnapshotState> {
    if read_only {
        debug!(
            output_dir = %output_dir.display(),
            "staging overlaybd snapshot from read-only runtime config"
        );
        return Ok(LiveOverlaybdSnapshotState::ReadOnly);
    }

    let live_upper_data_path = restack_target_upper_data_path(live_runtime_image_config_path)
        .context("resolve restack source upper path")?;
    let snapshot_layer_path = output_dir.join(snapshot_layer_file_name);
    prepare_specific_snapshot_layer_path(&snapshot_layer_path).await?;
    let live_snapshot_layer_path = if on_same_filesystem(&live_upper_data_path, output_dir)
        .context("validate restack snapshot filesystem precondition")?
    {
        snapshot_layer_path.clone()
    } else {
        let live_snapshot_layer_path = live_upper_data_path
            .parent()
            .context("restack source upper path has no parent directory")?
            .join(snapshot_layer_file_name);
        prepare_specific_snapshot_layer_path(&live_snapshot_layer_path)
            .await
            .with_context(|| {
                format!(
                    "prepare same-filesystem restack snapshot layer {}",
                    live_snapshot_layer_path.display()
                )
            })?;
        live_snapshot_layer_path
    };

    let descriptor = UblkDeviceManager::global()
        .restack_snapshot_device(ublk_device, &live_snapshot_layer_path, kind)
        .await
        .context("request overlaybd restack snapshot from ublk device")?;

    if live_snapshot_layer_path != snapshot_layer_path {
        // If this cross-filesystem copy fails, leave the live runtime config
        // untouched and surface a terminal pause failure. The daemon has
        // already sealed the old upper and reopened a fresh one, so callers
        // must not continue treating the live runtime as safely resumable.
        copy_file_atomically(
            &live_snapshot_layer_path,
            &snapshot_layer_path,
            "restack snapshot layer",
        )
        .await
        .context("copy restack snapshot layer into managed snapshot dir")
        .map_err(into_terminal_snapshot_failure)?;
    }

    if let Some(descriptor) = descriptor.as_ref() {
        let copied_size = tokio::fs::metadata(&snapshot_layer_path)
            .await
            .with_context(|| {
                format!(
                    "read restack snapshot layer metadata {}",
                    snapshot_layer_path.display()
                )
            })
            .map_err(into_terminal_snapshot_failure)?
            .len();
        if copied_size != descriptor.size {
            // Keep the sealed layer: the live device may still reference it,
            // and terminal deletion marks the volume failed for recovery.
            return Err(into_terminal_snapshot_failure(anyhow::anyhow!(
                "restack snapshot descriptor size mismatch for {}: descriptor says {}, file has {}",
                snapshot_layer_path.display(),
                descriptor.size,
                copied_size
            )));
        }
    }

    rewrite_live_runtime_config_for_restack(
        live_runtime_image_config_path,
        &snapshot_layer_path,
        descriptor.as_ref(),
    )
    .await
    .context("rewrite live runtime config after restack snapshot")
    .map_err(into_terminal_snapshot_failure)?;

    Ok(LiveOverlaybdSnapshotState::Restacked(snapshot_layer_path))
}

pub(crate) async fn build_mem_snapshot_image_config(
    resume_mem_image_config_path: Option<&Path>,
    new_layer_path: &Path,
    output_dir: &Path,
    memory_output: OverlaybdCompactOutput,
) -> Result<ImageConfig> {
    let inherited_image_config =
        load_existing_image_config(resume_mem_image_config_path, "memory snapshot")?;
    let new_layer = local_layer_config(new_layer_path);
    let lowers = rewrite_lowers_with_owned_runtime_suffix(
        inherited_image_config.lowers,
        output_dir,
        Some(new_layer),
        "mem_compacted.commit",
        memory_output,
    )
    .await?;

    Ok(ImageConfig {
        repo_blob_url: inherited_image_config.repo_blob_url,
        lowers,
        ..Default::default()
    })
}

/// Stage a persisted snapshot config from the current live overlaybd runtime.
///
/// Writable runtimes are first restacked in-place so the sealed old upper
/// becomes the newest lower. Read-only runtimes skip the restack phase and
/// only stage the persisted snapshot image config. The sealed layer always
/// stays raw; compression, when enabled, happens once at publish time.
pub(crate) async fn restack_snapshot_overlaybd_device(
    ublk_device: &UblkDevice,
    read_only: bool,
    live_runtime_image_config_path: &Path,
    output_dir: &Path,
    snapshot_layer_file_name: &str,
    kind: &'static str,
) -> Result<PathBuf> {
    restack_snapshot_overlaybd_device_with_runtime_roots(
        ublk_device,
        read_only,
        live_runtime_image_config_path,
        output_dir,
        snapshot_layer_file_name,
        kind,
        canonicalized_runtime_owned_roots(),
    )
    .await
}

pub(crate) async fn restack_snapshot_overlaybd_device_with_runtime_roots(
    ublk_device: &UblkDevice,
    read_only: bool,
    live_runtime_image_config_path: &Path,
    output_dir: &Path,
    snapshot_layer_file_name: &str,
    kind: &'static str,
    runtime_owned_roots: &[PathBuf],
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("create overlaybd snapshot dir {}", output_dir.display()))?;

    let live_snapshot = capture_live_overlaybd_snapshot(
        ublk_device,
        read_only,
        live_runtime_image_config_path,
        output_dir,
        snapshot_layer_file_name,
        kind,
    )
    .await?;

    let staged_snapshot = stage_overlaybd_snapshot_from_live_runtime(
        live_runtime_image_config_path,
        output_dir,
        live_snapshot.snapshot_layer_path(),
        runtime_owned_roots,
    )
    .await;

    live_snapshot.finish_staging(staged_snapshot)
}

#[cfg(feature = "hypervisor-firecracker")]
pub(crate) async fn restack_snapshot_overlaybd_rootfs(
    ublk_device: &UblkDevice,
    read_only: bool,
    live_runtime_image_config_path: &Path,
    snapshot_root: &Path,
) -> Result<PathBuf> {
    restack_snapshot_overlaybd_device(
        ublk_device,
        read_only,
        live_runtime_image_config_path,
        &snapshot_root.join("rootfs"),
        "snapshot.commit",
        "rootfs",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use overlaybd::backend::local::LocalFile;
    use overlaybd::virtual_file::VirtualFile;
    use serde_json::json;
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn capture_owner_outlives_publish_future() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(PersistentSnapshotRootGuard::new(
            temp.path().join("capture"),
        ));
        root.prepare().await.unwrap();
        let snapshot_dir = root.path().join("snapshot");
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        for name in ["rootfs.commit", "extra.commit", "volume.commit"] {
            std::fs::write(snapshot_dir.join(name), name).unwrap();
        }
        let owner = StorageCaptureOwner::new(snapshot_dir.clone(), root.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let publisher = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            for name in ["rootfs.commit", "extra.commit", "volume.commit"] {
                assert_eq!(
                    std::fs::read(owner.snapshot_dir.join(name)).unwrap(),
                    name.as_bytes()
                );
            }
            assert!(owner.snapshot_root.path().exists());
            drop(owner);
        });
        started_rx.await.unwrap();
        // The initiating caller and source no longer own the capture directory.
        drop(root);
        assert!(snapshot_dir.join("volume.commit").exists());
        release_tx.send(()).unwrap();
        publisher.await.unwrap();
        assert!(!snapshot_dir.exists());
    }

    #[test]
    fn split_runtime_suffix_preserves_stable_lowers() {
        let config = ConfigManager::global_config();
        let snapshot_store_root = config
            .backend
            .posix_fs
            .as_ref()
            .map(|posix_fs| posix_fs.snapshot_store.clone())
            .unwrap_or_else(|| std::env::temp_dir().join("test_snapshot_store"));
        let persisted_sandbox_store_root = config.orchestrator.persisted_sandbox_store_path.clone();
        let snapshot_store_lower = snapshot_store_root
            .join("managed-layers")
            .join("sha256_base.overlaybd.commit");
        let persisted_store_lower = persisted_sandbox_store_root
            .join("artifacts")
            .join("sandbox-parent")
            .join("1700000000000")
            .join("rootfs")
            .join("snapshot.commit");
        let managed_base_lower = managed_snapshot_base().join("sandbox/1700000000001/mem.commit");
        let lowers = vec![
            LayerConfig {
                file: String::new(),
                digest: "sha256:remote".to_string(),
                size: 42,
                ..Default::default()
            },
            local_layer_config(&snapshot_store_lower),
            local_layer_config(&persisted_store_lower),
            local_layer_config(&managed_base_lower),
        ];

        let (preserved, managed) =
            split_runtime_suffix(lowers, canonicalized_runtime_owned_roots());

        assert_eq!(preserved.len(), 2);
        assert_eq!(preserved[0].file, "");
        assert_eq!(preserved[0].digest, "sha256:remote");
        assert_eq!(PathBuf::from(&preserved[1].file), snapshot_store_lower);
        assert_eq!(managed.len(), 2);
        assert_eq!(PathBuf::from(&managed[0].file), persisted_store_lower);
        assert_eq!(PathBuf::from(&managed[1].file), managed_base_lower);
    }

    #[tokio::test]
    async fn snapshot_adopts_persisted_runtime_suffix_by_link() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifacts_root = temp.path().join("persisted-sandboxes").join("artifacts");
        let output_dir = artifacts_root.join("sandbox-c").join("gen1").join("rootfs");
        let parent_root = artifacts_root.join("sandbox-b").join("gen1").join("rootfs");
        let cache_root = temp.path().join("image-cache").join("commits");
        std::fs::create_dir_all(&parent_root).expect("create parent rootfs");
        std::fs::create_dir_all(&cache_root).expect("create cache root");

        let parent_base = parent_root.join("managed-base.commit");
        let parent_snapshot = parent_root.join("snapshot.commit");
        let cache_layer = cache_root.join("sha256-base").join("overlaybd.commit");
        std::fs::create_dir_all(cache_layer.parent().unwrap()).expect("create cache layer dir");
        std::fs::write(&parent_base, b"parent base").expect("write parent base");
        std::fs::write(&parent_snapshot, b"parent snapshot").expect("write parent snapshot");
        std::fs::write(&cache_layer, b"cache base").expect("write cache layer");

        let lowers = vec![
            LayerConfig {
                file: cache_layer.display().to_string(),
                digest: "sha256:base".to_string(),
                size: 10,
                ..Default::default()
            },
            local_layer_config(&parent_base),
            local_layer_config(&parent_snapshot),
        ];
        let runtime_owned_roots = [artifacts_root.canonicalize().unwrap()];

        let rewritten = rewrite_lowers_with_runtime_roots(
            lowers,
            &output_dir,
            None,
            MANAGED_BASE_LAYER_FILE,
            &runtime_owned_roots,
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("rewrite inherited runtime layers");

        assert_eq!(rewritten.len(), 3);
        assert_eq!(PathBuf::from(&rewritten[0].file), cache_layer);
        assert_eq!(rewritten[0].digest, "sha256:base");
        assert_eq!(rewritten[0].size, 10);

        for (index, (rewritten_lower, source)) in rewritten[1..]
            .iter()
            .zip([parent_base, parent_snapshot])
            .enumerate()
        {
            let adopted = PathBuf::from(&rewritten_lower.file);
            let expected_parent = output_dir
                .join(INHERITED_LAYERS_DIR)
                .join(format!("{index:04}"));
            assert!(adopted.starts_with(expected_parent));
            assert!(!adopted.starts_with(&parent_root));
            assert_eq!(adopted.file_name(), source.file_name());
            assert!(rewritten_lower.digest.is_empty());
            assert_eq!(rewritten_lower.size, 0);

            let source_metadata = std::fs::metadata(&source).expect("source metadata");
            let adopted_metadata = std::fs::metadata(&adopted).expect("adopted metadata");
            assert_eq!(source_metadata.dev(), adopted_metadata.dev());
            assert_eq!(source_metadata.ino(), adopted_metadata.ino());
        }
    }

    #[tokio::test]
    async fn rewrite_deep_stable_prefix_adopts_without_compaction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime_root = temp.path().join("runtime");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&runtime_root).expect("create runtime root");
        std::fs::create_dir_all(&output_dir).expect("create output dir");

        // A stable prefix deeper than DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS must
        // not trigger compaction on its own: only the runtime-owned suffix counts
        // toward the budget (32 - 33/4 = 24 here, and the suffix is just 2).
        let mut lowers: Vec<LayerConfig> = (0..33)
            .map(|index| LayerConfig {
                file: String::new(),
                digest: format!("sha256:base-{index}"),
                size: 4096,
                ..Default::default()
            })
            .collect();
        let runtime_a = runtime_root.join("a.commit");
        let runtime_b = runtime_root.join("b.commit");
        std::fs::write(&runtime_a, b"runtime a").expect("write runtime layer a");
        std::fs::write(&runtime_b, b"runtime b").expect("write runtime layer b");
        lowers.push(local_layer_config(&runtime_a));
        lowers.push(local_layer_config(&runtime_b));
        let runtime_owned_roots = [runtime_root.canonicalize().unwrap()];

        let rewritten = rewrite_lowers_with_runtime_roots(
            lowers,
            &output_dir,
            None,
            MANAGED_BASE_LAYER_FILE,
            &runtime_owned_roots,
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("rewrite with deep stable prefix");

        assert_eq!(rewritten.len(), 35);
        assert!(rewritten[..33].iter().all(|lower| lower.file.is_empty()));
        assert_eq!(rewritten[0].digest, "sha256:base-0");
        for (index, rewritten_lower) in rewritten[33..].iter().enumerate() {
            let adopted = PathBuf::from(&rewritten_lower.file);
            let expected_parent = output_dir
                .join(INHERITED_LAYERS_DIR)
                .join(format!("{index:04}"));
            assert!(adopted.starts_with(expected_parent));
        }
        assert!(!output_dir.join(MANAGED_BASE_LAYER_FILE).exists());
    }

    /// Seal a tiny raw overlaybd commit for compaction tests.
    async fn seal_raw_test_layer(
        dir: &Path,
        name: &str,
        vsize: u64,
        writes: &[(u64, u8)],
    ) -> PathBuf {
        let data = Arc::new(
            LocalFile::new(dir.join(format!("{name}.data"))).expect("create layer data file"),
        );
        let index = Arc::new(
            LocalFile::new(dir.join(format!("{name}.index"))).expect("create layer index file"),
        );
        let layer = overlaybd::index_file::LSMTFile::create(data, Some(index), vsize, false)
            .await
            .expect("create layer");
        for (offset, byte) in writes {
            layer
                .write_at(*offset, &[*byte; 4096])
                .await
                .expect("write layer page");
        }
        let commit_path = dir.join(format!("{name}.commit"));
        let output: Arc<dyn VirtualFile> =
            Arc::new(LocalFile::new(&commit_path).expect("create layer commit output"));
        layer
            .commit_with_args(overlaybd::index_file::CommitArgs::new(output))
            .await
            .expect("commit raw layer");
        commit_path
    }

    #[tokio::test]
    async fn rewrite_shrinking_budget_compacts_runtime_suffix() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime_root = temp.path().join("runtime");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&runtime_root).expect("create runtime root");
        std::fs::create_dir_all(&output_dir).expect("create output dir");

        // A 124-layer stable prefix shrinks the budget to max(32 - 124/4, 1) = 1,
        // so a two-layer runtime-owned suffix already triggers compaction.
        let mut lowers: Vec<LayerConfig> = (0..124)
            .map(|index| LayerConfig {
                file: String::new(),
                digest: format!("sha256:base-{index}"),
                size: 4096,
                ..Default::default()
            })
            .collect();
        let layer_a = seal_raw_test_layer(&runtime_root, "a", 4096, &[(0, 0xAA)]).await;
        let layer_b = seal_raw_test_layer(&runtime_root, "b", 4096, &[(0, 0xBB)]).await;
        lowers.push(local_layer_config(&layer_a));
        lowers.push(local_layer_config(&layer_b));
        let runtime_owned_roots = [runtime_root.canonicalize().unwrap()];

        let rewritten = rewrite_lowers_with_runtime_roots(
            lowers,
            &output_dir,
            None,
            MANAGED_BASE_LAYER_FILE,
            &runtime_owned_roots,
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("rewrite with shrinking budget");

        assert_eq!(rewritten.len(), 125);
        assert!(rewritten[..124].iter().all(|lower| lower.file.is_empty()));
        let compacted = PathBuf::from(&rewritten[124].file);
        assert_eq!(compacted, output_dir.join(MANAGED_BASE_LAYER_FILE));
        assert!(compacted.exists());
        assert!(!compacted.with_extension("commit.tmp").exists());
    }

    #[tokio::test]
    async fn stage_live_runtime_preserves_restacked_layer_descriptor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let live_dir = temp.path().join("live");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&live_dir).expect("create live dir");
        std::fs::create_dir_all(&output_dir).expect("create output dir");

        let snapshot_lower = output_dir.join("snapshot.commit");
        std::fs::write(&snapshot_lower, b"snapshot").expect("write snapshot lower");
        let live_runtime_image_config_path = live_dir.join("image.json");
        std::fs::write(
            &live_runtime_image_config_path,
            serde_json::to_vec_pretty(&json!({
                "lowers": [
                    {
                        "file": snapshot_lower,
                        "digest": "sha256:descriptor",
                        "size": 8
                    }
                ],
                "upper": {},
                "resultFile": live_dir.join("result.txt"),
                "download": {}
            }))
            .expect("serialize live image config"),
        )
        .expect("write live image config");

        let output_path = stage_overlaybd_snapshot_from_live_runtime(
            &live_runtime_image_config_path,
            &output_dir,
            Some(&snapshot_lower),
            canonicalized_runtime_owned_roots(),
        )
        .await
        .expect("stage snapshot");

        let staged =
            overlaybd::config::load_image_config(&output_path).expect("load staged image config");
        let latest = staged.lowers.last().expect("latest lower");
        assert_eq!(PathBuf::from(&latest.file), snapshot_lower);
        assert_eq!(latest.digest, "sha256:descriptor");
        assert_eq!(latest.size, 8);
    }

    #[tokio::test]
    async fn build_mem_snapshot_image_config_skips_compaction_for_remote_lowers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent_config_path = temp.path().join("parent-mem-image.json");
        let repo_blob_url = "s3://agentenv-oss-validation/validation/managed-layers";
        let remote_lowers: Vec<_> = (0..DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS)
            .map(|index| {
                json!({
                    "digest": format!("sha256:parent-{index}"),
                    "size": 4096
                })
            })
            .collect();
        std::fs::write(
            &parent_config_path,
            serde_json::to_vec_pretty(&json!({
                "repoBlobUrl": repo_blob_url,
                "lowers": remote_lowers,
                "upper": {},
                "resultFile": "",
                "download": {}
            }))
            .expect("serialize parent image config"),
        )
        .expect("write parent image config");

        let new_layer = temp.path().join("mem_overlaybd").join("overlaybd.commit");
        std::fs::create_dir_all(new_layer.parent().unwrap()).expect("create mem layer dir");
        std::fs::write(&new_layer, b"memory delta").expect("write mem layer");
        let image_config = build_mem_snapshot_image_config(
            Some(&parent_config_path),
            &new_layer,
            temp.path(),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("build memory image config");

        assert_eq!(image_config.repo_blob_url, repo_blob_url);
        assert_eq!(
            image_config.lowers.len(),
            DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS + 1
        );
        assert_eq!(image_config.lowers[0].digest, "sha256:parent-0");
        assert!(image_config.lowers[..DEFAULT_MAX_OVERLAYBD_SNAPSHOT_LAYERS]
            .iter()
            .all(|lower| lower.file.is_empty()));
        let latest = image_config.lowers.last().expect("latest mem lower");
        assert_eq!(PathBuf::from(&latest.file), new_layer);
        assert!(latest.digest.is_empty());
        assert_eq!(latest.size, 0);
        assert!(image_config.download_override.is_none());
    }
}
