use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{PersistenceResult, SandboxPersistenceError, SandboxPersister};
use crate::local_store::{LocalKvStore, LocalStoreDurability};
use crate::orchestrator::{store::SandboxMetadata, SandboxState};
use crate::sandbox::restore_identity::SnapshotRestoreIdentity;
use crate::sandbox::{PausedSandboxState, RuntimeArtifactSet, SandboxBackendFactory};
use crate::types::SandboxId;
use crate::virtualization::VirtualizationMode;

const RECORD_VERSION: u32 = 1;
const RECORD_DB_DIR: &str = "records.db";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedPausedLifecycle {
    Paused,
    Resuming,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedPausedRecord {
    version: u32,
    lifecycle: PersistedPausedLifecycle,
    metadata: SandboxMetadata,
    artifact_root: PathBuf,
    state: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_identity: Option<SnapshotRestoreIdentity>,
    #[serde(default)]
    runtime_artifacts: RuntimeArtifactSet,
}

impl PersistedPausedRecord {
    fn into_metadata<F>(mut self, factory: &F) -> PersistenceResult<SandboxMetadata>
    where
        F: SandboxBackendFactory,
    {
        ensure_supported_version(self.version)?;

        let paused_state = factory
            .decode_paused_state(self.artifact_root, self.state)
            .map_err(|source| SandboxPersistenceError::InvalidRecord {
                reason: "failed to decode paused sandbox state".to_string(),
                source: Some(source),
            })?;
        self.metadata.state = SandboxState::Paused;
        self.metadata.paused_state = Some(paused_state);
        self.metadata.retained_runtime_artifacts = self.runtime_artifacts;

        Ok(self.metadata)
    }

    fn into_metadata_without_runtime_state(mut self) -> SandboxMetadata {
        self.metadata.state = SandboxState::Paused;
        self.metadata.paused_state = None;
        self.metadata.retained_runtime_artifacts = self.runtime_artifacts;
        self.metadata
    }
}

fn decode_record(bytes: &[u8]) -> PersistenceResult<PersistedPausedRecord> {
    let record: PersistedPausedRecord =
        serde_json::from_slice(bytes).map_err(|source| SandboxPersistenceError::InvalidRecord {
            reason: "failed to deserialize record".to_string(),
            source: Some(source.into()),
        })?;
    ensure_supported_version(record.version)?;
    Ok(record)
}

fn ensure_supported_version(version: u32) -> PersistenceResult<()> {
    if version == RECORD_VERSION {
        Ok(())
    } else {
        Err(SandboxPersistenceError::InvalidRecord {
            reason: format!("unsupported record version {version}"),
            source: None,
        })
    }
}

pub struct FileBackedSandboxPersister {
    root: PathBuf,
    virtualization_mode: VirtualizationMode,
    durability: LocalStoreDurability,
    db: OnceCell<LocalKvStore>,
    dragonball: OnceCell<Box<FileBackedSandboxPersister>>,
    allows_image_gc: AtomicBool,
}

impl FileBackedSandboxPersister {
    pub fn new(root: PathBuf, virtualization_mode: VirtualizationMode) -> Self {
        Self {
            root,
            virtualization_mode,
            durability: LocalStoreDurability::Sync,
            db: OnceCell::new(),
            dragonball: OnceCell::new(),
            allows_image_gc: AtomicBool::new(true),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(root: PathBuf) -> Self {
        Self::new(root, VirtualizationMode::Kvm)
    }

    pub fn with_durability(mut self, durability: LocalStoreDurability) -> Self {
        self.durability = durability;
        self
    }

    async fn dragonball_store(&self) -> &Self {
        self.dragonball
            .get_or_init(|| async {
                Box::new(
                    Self::new(self.root.join("dragonball"), self.virtualization_mode)
                        .with_durability(self.durability),
                )
            })
            .await
    }

    async fn store_for_backend(&self, backend: &str) -> &Self {
        if backend == "dragonball" {
            self.dragonball_store().await
        } else {
            self
        }
    }

    async fn locate_record(
        &self,
        sandbox_id: &SandboxId,
    ) -> PersistenceResult<(&Self, PersistedPausedRecord)> {
        if self
            .db()
            .await?
            .get(sandbox_id.to_string())
            .await
            .map_err(|source| SandboxPersistenceError::store("locate paused record", source))?
            .is_some()
        {
            return Ok((self, self.get_record(sandbox_id).await?));
        }
        let store = self.dragonball_store().await;
        Ok((store, store.get_record(sandbox_id).await?))
    }

    fn records_db_path(&self) -> PathBuf {
        self.root.join(RECORD_DB_DIR)
    }

    fn artifacts_root(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    fn sandbox_artifact_root(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.artifacts_root().join(sandbox_id.to_string())
    }

    async fn db(&self) -> PersistenceResult<LocalKvStore> {
        self.db
            .get_or_try_init(|| async {
                LocalKvStore::open(self.records_db_path(), self.durability)
                    .await
                    .map_err(|source| SandboxPersistenceError::store("open RocksDB", source))
            })
            .await
            .cloned()
    }

    async fn get_record(&self, sandbox_id: &SandboxId) -> PersistenceResult<PersistedPausedRecord> {
        let bytes = self
            .db()
            .await?
            .get(sandbox_id.to_string())
            .await
            .map_err(|source| SandboxPersistenceError::store("read paused sandbox record", source))?
            .ok_or_else(|| SandboxPersistenceError::InvalidRecord {
                reason: format!("paused sandbox record {sandbox_id} not found"),
                source: None,
            })?;
        decode_record(&bytes)
    }

    async fn put_record(&self, record: &PersistedPausedRecord) -> PersistenceResult<()> {
        let bytes = serde_json::to_vec(record).map_err(|source| {
            SandboxPersistenceError::InvalidRecord {
                reason: "failed to serialize record".to_string(),
                source: Some(source.into()),
            }
        })?;

        self.db()
            .await?
            .put(record.metadata.id.to_string(), bytes)
            .await
            .map_err(|source| {
                SandboxPersistenceError::store("persist paused sandbox record", source)
            })
    }

    async fn remove_record(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        self.db()
            .await?
            .delete(sandbox_id.to_string())
            .await
            .map_err(|source| {
                SandboxPersistenceError::store("remove paused sandbox record", source)
            })
    }

    async fn remove_artifact_root(path: &Path) -> PersistenceResult<()> {
        match fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SandboxPersistenceError::io(
                "remove paused sandbox artifacts",
                path,
                source,
            )),
        }
    }

    async fn cleanup_invalid_record(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        debug!(sandbox_id = %sandbox_id, "cleaning up invalid paused sandbox record");
        self.remove_record(sandbox_id).await?;
        Self::remove_artifact_root(&self.sandbox_artifact_root(sandbox_id)).await?;
        Ok(())
    }

    async fn cleanup_orphan_artifacts(
        &self,
        retained_sandbox_ids: &HashSet<SandboxId>,
    ) -> PersistenceResult<()> {
        let artifacts_root = self.artifacts_root();
        let mut entries = match fs::read_dir(&artifacts_root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(SandboxPersistenceError::io(
                    "read paused sandbox artifacts",
                    &artifacts_root,
                    source,
                ));
            }
        };

        while let Some(entry) = entries.next_entry().await.map_err(|source| {
            SandboxPersistenceError::io("scan paused sandbox artifacts", &artifacts_root, source)
        })? {
            let file_type = entry.file_type().await.map_err(|source| {
                SandboxPersistenceError::io(
                    "inspect paused sandbox artifacts",
                    entry.path(),
                    source,
                )
            })?;
            if !file_type.is_dir() {
                continue;
            }

            let Some(sandbox_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| SandboxId::parse_str(name).ok())
            else {
                continue;
            };

            if !retained_sandbox_ids.contains(&sandbox_id) {
                info!(
                    sandbox_id = %sandbox_id,
                    artifacts = %entry.path().display(),
                    "removing orphaned paused sandbox artifacts"
                );
                Self::remove_artifact_root(&entry.path()).await?;
            }
        }

        Ok(())
    }
}

impl FileBackedSandboxPersister {
    async fn load_namespace<F>(&self, factory: &F) -> PersistenceResult<Vec<SandboxMetadata>>
    where
        F: SandboxBackendFactory,
    {
        info!(store = %self.root.display(), "loading paused sandbox records");
        let records = self.db().await?.entries().await.map_err(|source| {
            SandboxPersistenceError::store("scan paused sandbox records", source)
        })?;
        let mut sandboxes = Vec::new();
        let mut retained_artifacts = HashSet::new();
        let mut unknown_outer = false;

        for (key, bytes) in records {
            let sandbox_id_from_key = std::str::from_utf8(&key)
                .ok()
                .and_then(|value| SandboxId::parse_str(value).ok());
            let record = match decode_record(&bytes) {
                Ok(record) => record,
                Err(err) => {
                    warn!(record_key = %String::from_utf8_lossy(&key), error = %err, "retaining unreadable paused sandbox record");
                    unknown_outer = true;
                    self.allows_image_gc.store(false, Ordering::Release);
                    if let Some(sandbox_id) = sandbox_id_from_key {
                        retained_artifacts.insert(sandbox_id);
                    }
                    continue;
                }
            };
            let sandbox_id = record.metadata.id;

            let backend = record
                .backend
                .as_deref()
                .unwrap_or(&record.metadata.runtime_versions.backend);
            let supported = backend == factory.backend()
                && backend == record.metadata.runtime_versions.backend
                && record.restore_identity == record.metadata.runtime_versions.restore_identity
                && record
                    .restore_identity
                    .as_ref()
                    .map_or(backend == "firecracker", |identity| {
                        identity.backend == backend && factory.supports_restore(identity)
                    });
            if !supported {
                if record.runtime_artifacts.is_empty() {
                    self.allows_image_gc.store(false, Ordering::Release);
                }
                warn!(sandbox_id = %sandbox_id, backend, "retaining paused sandbox for a different backend reader");
                retained_artifacts.insert(sandbox_id);
                sandboxes.push(record.into_metadata_without_runtime_state());
                continue;
            }

            if record.lifecycle == PersistedPausedLifecycle::Resuming {
                warn!(sandbox_id = %sandbox_id, "discarding paused sandbox record left in resuming state");
                self.cleanup_invalid_record(&sandbox_id).await?;
                continue;
            }

            if record.metadata.virtualization_mode != self.virtualization_mode {
                if record.runtime_artifacts.is_empty() {
                    self.allows_image_gc.store(false, Ordering::Release);
                }
                warn!(
                    sandbox_id = %sandbox_id,
                    record_mode = %record.metadata.virtualization_mode,
                    node_mode = %self.virtualization_mode,
                    "loading paused sandbox metadata without resumable runtime state because its virtualization mode is incompatible"
                );
                retained_artifacts.insert(sandbox_id);
                sandboxes.push(record.into_metadata_without_runtime_state());
                continue;
            }

            let qualified = record.backend.is_some();
            let retained_metadata = record.clone().into_metadata_without_runtime_state();
            match record.into_metadata(factory) {
                Ok(metadata) => {
                    retained_artifacts.insert(sandbox_id);
                    sandboxes.push(metadata);
                }
                Err(err) => {
                    if qualified {
                        if retained_metadata.retained_runtime_artifacts.is_empty() {
                            self.allows_image_gc.store(false, Ordering::Release);
                        }
                        warn!(sandbox_id = %sandbox_id, error = %err, "retaining qualified paused record after decode failure");
                        retained_artifacts.insert(sandbox_id);
                        sandboxes.push(retained_metadata);
                    } else {
                        warn!(sandbox_id = %sandbox_id, error = %err, "discarding corrupt legacy paused sandbox record");
                        self.cleanup_invalid_record(&sandbox_id).await?;
                    }
                }
            }
        }

        if !unknown_outer {
            self.cleanup_orphan_artifacts(&retained_artifacts).await?;
        }

        info!(
            loaded = sandboxes.len(),
            retained = retained_artifacts.len(),
            "loaded paused sandbox records"
        );

        Ok(sandboxes)
    }
}

#[async_trait]
impl SandboxPersister for FileBackedSandboxPersister {
    fn allows_image_gc(&self) -> bool {
        self.allows_image_gc.load(Ordering::Acquire)
    }

    async fn load_all<F>(&self, factory: &F) -> PersistenceResult<Vec<SandboxMetadata>>
    where
        F: SandboxBackendFactory,
    {
        let mut records = self.load_namespace(factory).await?;
        if self.root.join("dragonball").exists() {
            let db_store = self.dragonball_store().await;
            records.extend(db_store.load_namespace(factory).await?);
            if !db_store.allows_image_gc() {
                self.allows_image_gc.store(false, Ordering::Release);
            }
        }
        Ok(records)
    }

    async fn allocate_artifact_root_for_backend(
        &self,
        sandbox_id: &SandboxId,
        backend: &str,
    ) -> PersistenceResult<Option<PathBuf>> {
        self.store_for_backend(backend)
            .await
            .allocate_artifact_root(sandbox_id)
            .await
    }

    async fn allocate_artifact_root(
        &self,
        sandbox_id: &SandboxId,
    ) -> PersistenceResult<Option<PathBuf>> {
        let artifact_root = self
            .sandbox_artifact_root(sandbox_id)
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&artifact_root).await.map_err(|source| {
            SandboxPersistenceError::io(
                "allocate paused sandbox artifact root",
                &artifact_root,
                source,
            )
        })?;
        Ok(Some(artifact_root))
    }

    async fn persist_paused(
        &self,
        metadata: &SandboxMetadata,
        artifact_root: Option<&Path>,
        paused_state: &dyn PausedSandboxState,
    ) -> PersistenceResult<()> {
        let artifact_root = artifact_root.ok_or_else(|| SandboxPersistenceError::RuntimeState {
            reason: "file-backed persister requires an allocated artifact root",
        })?;
        debug!(
            sandbox_id = %metadata.id,
            artifact_root = %artifact_root.display(),
            "persisting paused sandbox"
        );
        let state = match paused_state.encode() {
            Ok(state) => state,
            Err(source) => {
                if !paused_state.owns_artifact_cleanup() {
                    let _ = Self::remove_artifact_root(artifact_root).await;
                }
                return Err(SandboxPersistenceError::InvalidRecord {
                    reason: "failed to encode paused sandbox state".to_string(),
                    source: Some(source),
                });
            }
        };
        let record = PersistedPausedRecord {
            version: RECORD_VERSION,
            lifecycle: PersistedPausedLifecycle::Paused,
            metadata: metadata.clone(),
            artifact_root: artifact_root.to_path_buf(),
            state,
            backend: Some(metadata.runtime_versions.backend.clone()),
            restore_identity: metadata.runtime_versions.restore_identity.clone(),
            runtime_artifacts: paused_state.runtime_artifacts(),
        };
        let result = self
            .store_for_backend(&metadata.runtime_versions.backend)
            .await
            .put_record(&record)
            .await;
        if result.is_err() {
            if !paused_state.owns_artifact_cleanup() {
                let _ = Self::remove_artifact_root(artifact_root).await;
            }
        } else {
            paused_state.persistence_committed();
        }
        result
    }

    async fn mark_resuming(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        debug!(sandbox_id = %sandbox_id, "marking paused sandbox as resuming");
        let (store, mut record) = self.locate_record(sandbox_id).await?;
        record.lifecycle = PersistedPausedLifecycle::Resuming;
        store.put_record(&record).await
    }

    async fn rollback_resuming(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        debug!(sandbox_id = %sandbox_id, "rolling back paused sandbox to paused");
        let (store, mut record) = self.locate_record(sandbox_id).await?;
        record.lifecycle = PersistedPausedLifecycle::Paused;
        store.put_record(&record).await
    }

    async fn delete_record(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        debug!(sandbox_id = %sandbox_id, "deleting paused sandbox record");
        self.remove_record(sandbox_id).await?;
        if self.root.join("dragonball").exists() {
            self.dragonball_store()
                .await
                .remove_record(sandbox_id)
                .await?;
        }
        Ok(())
    }

    async fn delete_record_and_artifacts(&self, sandbox_id: &SandboxId) -> PersistenceResult<()> {
        debug!(sandbox_id = %sandbox_id, "deleting paused sandbox record and artifacts");
        self.delete_record(sandbox_id).await?;
        Self::remove_artifact_root(&self.sandbox_artifact_root(sandbox_id)).await?;
        if self.root.join("dragonball").exists() {
            Self::remove_artifact_root(
                &self
                    .dragonball_store()
                    .await
                    .sandbox_artifact_root(sandbox_id),
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{
        mock::{MockBackendFactory, MockSnapshot},
        FreshSandboxBuildSpec, PausedSandboxState, RuntimeArtifactSet, SandboxBackend,
        SandboxLaunchConfig,
    };
    use crate::snapshot::RunnableSnapshot;
    use anyhow::Result;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    #[derive(Debug)]
    struct FailingEncodeState;

    impl PausedSandboxState for FailingEncodeState {
        fn encode(&self) -> Result<Value> {
            anyhow::bail!("forced encode failure")
        }

        fn runtime_artifacts(&self) -> RuntimeArtifactSet {
            RuntimeArtifactSet::empty()
        }
    }

    #[derive(Default)]
    struct RejectingFactory;

    impl SandboxBackendFactory for RejectingFactory {
        fn backend(&self) -> &str {
            crate::sandbox::FIRECRACKER_BACKEND
        }

        fn validate_snapshot(&self, _snapshot: &RunnableSnapshot) -> Result<()> {
            unreachable!("persister tests only decode state")
        }

        fn build(
            &self,
            _build_spec: FreshSandboxBuildSpec,
            _launch_config: SandboxLaunchConfig,
        ) -> Result<Box<dyn SandboxBackend>> {
            unreachable!("persister tests only decode state")
        }

        fn build_from_snapshot(
            &self,
            _snapshot: &RunnableSnapshot,
            _launch_config: SandboxLaunchConfig,
        ) -> Result<Box<dyn SandboxBackend>> {
            unreachable!("persister tests only decode state")
        }

        fn build_from_paused_state(
            &self,
            _sandbox_id: SandboxId,
            _state: &dyn PausedSandboxState,
            _envd_access_token: Option<crate::sandbox::EnvdAccessToken>,
        ) -> Result<Box<dyn SandboxBackend>> {
            unreachable!("persister tests only decode state")
        }

        fn decode_paused_state(
            &self,
            _artifact_root: PathBuf,
            _state: Value,
        ) -> Result<Arc<dyn PausedSandboxState>> {
            anyhow::bail!("forced decode failure")
        }
    }

    fn paused_state(root: &Path) -> Arc<dyn PausedSandboxState> {
        std::fs::create_dir_all(root).expect("create test artifact root");
        Arc::new(MockSnapshot)
    }

    fn test_persister(root: &Path) -> FileBackedSandboxPersister {
        FileBackedSandboxPersister::new_for_test(root.to_path_buf())
            .with_durability(LocalStoreDurability::Memory)
    }

    async fn persist_test_record(
        persister: &FileBackedSandboxPersister,
        snapshot_root: &Path,
    ) -> anyhow::Result<(SandboxId, Arc<dyn PausedSandboxState>)> {
        let paused_state = paused_state(snapshot_root);
        let metadata = SandboxMetadata {
            id: SandboxId::new(),
            virtualization_mode: persister.virtualization_mode,
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };
        let sandbox_id = metadata.id;
        persister
            .persist_paused(&metadata, Some(snapshot_root), paused_state.as_ref())
            .await?;
        Ok((sandbox_id, paused_state))
    }

    async fn has_record(
        persister: &FileBackedSandboxPersister,
        sandbox_id: &SandboxId,
    ) -> anyhow::Result<bool> {
        Ok(persister
            .db()
            .await?
            .get(sandbox_id.to_string())
            .await?
            .is_some())
    }

    #[tokio::test]
    async fn file_persister_round_trips_paused_record() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata {
            timeout: Some(Duration::from_secs(5)),
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };

        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;
        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, metadata.id);
        assert!(loaded[0]
            .paused_state
            .as_ref()
            .expect("paused state should be restored")
            .downcast_ref::<MockSnapshot>()
            .is_some());
        Ok(())
    }

    async fn qualify_test_record(
        persister: &FileBackedSandboxPersister,
        sandbox_id: SandboxId,
        backend: &str,
        identity: Value,
        image_config: &Path,
    ) -> anyhow::Result<()> {
        let db = persister.db().await?;
        let mut record: Value = serde_json::from_slice(
            &db.get(sandbox_id.to_string())
                .await?
                .expect("stored record"),
        )?;
        record["backend"] = backend.into();
        record["restoreIdentity"] = identity.clone();
        record["metadata"]["runtime_versions"]["backend"] = backend.into();
        record["metadata"]["runtime_versions"]["restore_identity"] = identity;
        record["runtimeArtifacts"] = serde_json::json!({
            "overlaybd_image_config_paths": [image_config],
        });
        db.put(sandbox_id.to_string(), serde_json::to_vec(&record)?)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn paused_unknown_backend_is_retained_not_decoded() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let id = SandboxId::new();
        let root = temp.path().join("artifacts").join(id.to_string());
        let state = paused_state(&root);
        persister
            .persist_paused(
                &SandboxMetadata {
                    id,
                    ..Default::default()
                },
                Some(&root),
                state.as_ref(),
            )
            .await?;
        let config = root.join("image.json");
        std::fs::write(&config, b"retained-layer-reference")?;
        qualify_test_record(&persister, id, "future-vmm", Value::Null, &config).await?;

        let loaded = persister.load_all(&RejectingFactory).await?;
        assert_eq!(loaded.len(), 1, "unsupported backend must remain visible");
        assert_eq!(loaded[0].id, id);
        assert!(loaded[0].paused_state.is_none());
        assert_eq!(
            loaded[0]
                .retained_runtime_artifacts
                .clone()
                .into_overlaybd_image_config_paths(),
            vec![config.clone()]
        );
        assert!(has_record(&persister, &id).await?);
        assert_eq!(std::fs::read(&config)?, b"retained-layer-reference");

        qualify_test_record(&persister, id, "firecracker", Value::Null, &config).await?;
        let loaded = persister.load_all(&MockBackendFactory::new()).await?;
        assert_eq!(loaded[0].id, id);
        assert!(
            loaded[0].paused_state.is_some(),
            "compatible later reader may decode"
        );
        Ok(())
    }

    #[tokio::test]
    async fn paused_unknown_db_abi_keeps_artifacts() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let id = SandboxId::new();
        let root = temp.path().join("artifacts").join(id.to_string());
        let state = paused_state(&root);
        persister
            .persist_paused(
                &SandboxMetadata {
                    id,
                    ..Default::default()
                },
                Some(&root),
                state.as_ref(),
            )
            .await?;
        let config = root.join("memory-image.json");
        std::fs::write(&config, b"memory-lower-reference")?;
        qualify_test_record(
            &persister,
            id,
            "dragonball",
            serde_json::json!({
                "backend": "dragonball", "architecture": "x86_64", "virtualization_mode": "kvm",
                "runner_build_id": "future-build", "dragonball_revision": "future-revision",
                "snapshot_format_epoch": 1, "restore_abi_id": "future-abi",
                "cpu_policy_id": "host-default", "device_profile_id": "mmio",
                "memory_layout_id": "full-ram", "kernel_version": "future-kernel",
                "tools_drive_version": "future-tools", "envd_version": "future-envd",
            }),
            &config,
        )
        .await?;

        let loaded = persister.load_all(&RejectingFactory).await?;
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].paused_state.is_none());
        assert!(has_record(&persister, &id).await?);
        assert_eq!(std::fs::read(&config)?, b"memory-lower-reference");
        Ok(())
    }

    #[tokio::test]
    async fn legacy_fc_paused_record_still_resumes() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let root = temp.path().join("artifacts");
        let (id, _) = persist_test_record(&persister, &root).await?;
        let db = persister.db().await?;
        let mut record: Value = serde_json::from_slice(&db.get(id.to_string()).await?.unwrap())?;
        for field in ["backend", "restoreIdentity", "runtimeArtifacts"] {
            record.as_object_mut().unwrap().remove(field);
        }
        db.put(id.to_string(), serde_json::to_vec(&record)?).await?;
        let loaded = persister.load_all(&MockBackendFactory::new()).await?;
        assert_eq!(loaded[0].id, id);
        assert!(loaded[0]
            .paused_state
            .as_ref()
            .unwrap()
            .downcast_ref::<MockSnapshot>()
            .is_some());
        persister.mark_resuming(&id).await?;
        persister.rollback_resuming(&id).await?;
        assert_eq!(
            persister.load_all(&MockBackendFactory::new()).await?[0].id,
            id
        );
        Ok(())
    }

    #[tokio::test]
    async fn paused_conflicting_outer_backend_is_retained() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let id = SandboxId::new();
        let root = temp.path().join("artifacts").join(id.to_string());
        let state = paused_state(&root);
        persister
            .persist_paused(
                &SandboxMetadata {
                    id,
                    ..Default::default()
                },
                Some(&root),
                state.as_ref(),
            )
            .await?;
        let db = persister.db().await?;
        let mut record: Value = serde_json::from_slice(&db.get(id.to_string()).await?.unwrap())?;
        record["metadata"]["runtime_versions"]["backend"] = "dragonball".into();
        db.put(id.to_string(), serde_json::to_vec(&record)?).await?;
        let loaded = persister.load_all(&MockBackendFactory::new()).await?;
        assert_eq!(loaded.len(), 1);
        assert!(
            loaded[0].paused_state.is_none(),
            "conflicting outer and metadata backends must not decode"
        );
        assert!(has_record(&persister, &id).await?);
        assert!(root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn paused_unknown_outer_schema_keeps_record_and_artifacts() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let id = SandboxId::new();
        let root = temp.path().join("artifacts").join(id.to_string());
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("layer"), b"future-layer")?;
        persister
            .db()
            .await?
            .put(id.to_string(), b"unknown future encoding".to_vec())
            .await?;
        assert!(persister
            .load_all(&MockBackendFactory::new())
            .await?
            .is_empty());
        assert!(has_record(&persister, &id).await?);
        assert_eq!(std::fs::read(root.join("layer"))?, b"future-layer");
        assert!(
            !persister.allows_image_gc(),
            "unknown graph cannot authorize external layer GC"
        );
        Ok(())
    }

    #[tokio::test]
    async fn paused_unsupported_record_without_liveness_graph_disables_gc() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let root = temp.path().join("artifacts");
        let (id, _) = persist_test_record(&persister, &root).await?;
        let db = persister.db().await?;
        let mut record: Value = serde_json::from_slice(&db.get(id.to_string()).await?.unwrap())?;
        record["backend"] = "future-vmm".into();
        record["metadata"]["runtime_versions"]["backend"] = "future-vmm".into();
        record.as_object_mut().unwrap().remove("runtimeArtifacts");
        db.put(id.to_string(), serde_json::to_vec(&record)?).await?;
        let loaded = persister.load_all(&RejectingFactory).await?;
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].paused_state.is_none());
        assert!(has_record(&persister, &id).await?);
        assert!(
            !persister.allows_image_gc(),
            "cannot replace unknown liveness graph with an empty hold"
        );
        Ok(())
    }

    #[tokio::test]
    async fn paused_record_from_other_mode_is_visible_but_not_resumable() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let kvm_persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        let (sandbox_id, _paused_state) =
            persist_test_record(&kvm_persister, &snapshot_root).await?;
        drop(kvm_persister);
        let pvm_persister =
            FileBackedSandboxPersister::new(temp.path().to_path_buf(), VirtualizationMode::Pvm)
                .with_durability(LocalStoreDurability::Memory);

        let loaded = pvm_persister.load_all(&MockBackendFactory::new()).await?;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, sandbox_id);
        assert_eq!(loaded[0].state, SandboxState::Paused);
        assert_eq!(loaded[0].virtualization_mode, VirtualizationMode::Kvm);
        assert!(loaded[0].paused_state.is_none());
        assert!(has_record(&pvm_persister, &sandbox_id).await?);
        assert!(snapshot_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn mixed_mode_records_are_both_visible_and_retained() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let kvm_persister = test_persister(temp.path());
        let kvm_root = temp.path().join("kvm-artifacts");
        let (kvm_id, _kvm_state) = persist_test_record(&kvm_persister, &kvm_root).await?;
        drop(kvm_persister);

        let pvm_persister =
            FileBackedSandboxPersister::new(temp.path().to_path_buf(), VirtualizationMode::Pvm)
                .with_durability(LocalStoreDurability::Memory);
        let pvm_root = temp.path().join("pvm-artifacts");
        let (pvm_id, _pvm_state) = persist_test_record(&pvm_persister, &pvm_root).await?;

        let mut loaded = pvm_persister.load_all(&MockBackendFactory::new()).await?;
        loaded.sort_by_key(|metadata| metadata.id);

        let kvm_metadata = loaded
            .iter()
            .find(|metadata| metadata.id == kvm_id)
            .expect("KVM metadata should remain visible");
        assert_eq!(kvm_metadata.virtualization_mode, VirtualizationMode::Kvm);
        assert!(kvm_metadata.paused_state.is_none());

        let pvm_metadata = loaded
            .iter()
            .find(|metadata| metadata.id == pvm_id)
            .expect("PVM metadata should load");
        assert_eq!(pvm_metadata.virtualization_mode, VirtualizationMode::Pvm);
        assert!(pvm_metadata.paused_state.is_some());

        assert!(has_record(&pvm_persister, &kvm_id).await?);
        assert!(has_record(&pvm_persister, &pvm_id).await?);
        assert!(kvm_root.exists());
        assert!(pvm_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn allocate_artifact_root_creates_unique_snapshot_roots() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();

        let first_root = persister
            .allocate_artifact_root(&sandbox_id)
            .await?
            .expect("file-backed persister should allocate artifact root");
        let second_root = persister
            .allocate_artifact_root(&sandbox_id)
            .await?
            .expect("file-backed persister should allocate artifact root");
        let sandbox_id_dir = sandbox_id.to_string();

        assert_ne!(first_root, second_root);
        assert!(first_root.is_dir());
        assert!(second_root.is_dir());
        assert_eq!(
            first_root.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new(&sandbox_id_dir))
        );
        assert_eq!(
            first_root
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name),
            Some(std::ffi::OsStr::new("artifacts"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn resuming_records_are_cleaned_on_load() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let snapshot_root = persister
            .sandbox_artifact_root(&sandbox_id)
            .join("snapshot");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata {
            id: sandbox_id,
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };

        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;
        persister.mark_resuming(&metadata.id).await?;

        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert!(loaded.is_empty());
        assert!(!has_record(&persister, &metadata.id).await?);
        assert!(!persister.sandbox_artifact_root(&metadata.id).exists());
        Ok(())
    }

    #[tokio::test]
    async fn persist_paused_accepts_backend_agnostic_state() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata::default();

        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;
        drop(paused_state);

        assert!(snapshot_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn persist_paused_cleans_artifacts_when_encode_fails() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        tokio::fs::create_dir_all(&snapshot_root).await?;
        let paused_state: Arc<dyn PausedSandboxState> = Arc::new(FailingEncodeState);
        let err = persister
            .persist_paused(
                &SandboxMetadata::default(),
                Some(&snapshot_root),
                paused_state.as_ref(),
            )
            .await
            .expect_err("encode failure should reject paused state");

        assert!(matches!(err, SandboxPersistenceError::InvalidRecord { .. }));
        assert!(!snapshot_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn mark_resuming_and_rollback_preserve_loadability() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        let (sandbox_id, _paused_state) = persist_test_record(&persister, &snapshot_root).await?;

        persister.mark_resuming(&sandbox_id).await?;
        assert_eq!(
            persister.get_record(&sandbox_id).await?.lifecycle,
            PersistedPausedLifecycle::Resuming
        );

        persister.rollback_resuming(&sandbox_id).await?;
        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, sandbox_id);
        assert!(snapshot_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn delete_record_removes_record_but_keeps_artifacts() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let snapshot_root = temp.path().join("artifacts");
        let (sandbox_id, _paused_state) = persist_test_record(&persister, &snapshot_root).await?;

        persister.delete_record(&sandbox_id).await?;

        assert!(!has_record(&persister, &sandbox_id).await?);
        assert!(snapshot_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn load_all_removes_orphan_artifacts_without_records() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let artifact_root = persister
            .sandbox_artifact_root(&sandbox_id)
            .join("resumed-generation");
        tokio::fs::create_dir_all(&artifact_root).await?;

        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert!(loaded.is_empty());
        assert!(!persister.sandbox_artifact_root(&sandbox_id).exists());
        Ok(())
    }

    #[tokio::test]
    async fn load_all_keeps_artifacts_for_valid_paused_record() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let snapshot_root = persister
            .sandbox_artifact_root(&sandbox_id)
            .join("paused-generation");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata {
            id: sandbox_id,
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };
        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;

        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert_eq!(loaded.len(), 1);
        assert!(persister.sandbox_artifact_root(&sandbox_id).exists());
        Ok(())
    }

    #[tokio::test]
    async fn delete_record_and_artifacts_removes_both() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let snapshot_root = persister
            .sandbox_artifact_root(&sandbox_id)
            .join("snapshot");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata {
            id: sandbox_id,
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };
        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;

        persister.delete_record_and_artifacts(&sandbox_id).await?;

        assert!(!has_record(&persister, &sandbox_id).await?);
        assert!(!persister.sandbox_artifact_root(&sandbox_id).exists());
        Ok(())
    }

    #[tokio::test]
    async fn delete_record_and_artifacts_removes_artifacts_without_record() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let sandbox_artifact_root = persister.sandbox_artifact_root(&sandbox_id);
        tokio::fs::create_dir_all(sandbox_artifact_root.join("stale-generation")).await?;

        persister.delete_record_and_artifacts(&sandbox_id).await?;

        assert!(!sandbox_artifact_root.exists());
        Ok(())
    }

    #[tokio::test]
    async fn delete_record_and_artifacts_removes_invalid_record() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        persister
            .db()
            .await?
            .put(sandbox_id.to_string(), b"not-json")
            .await?;

        persister.delete_record_and_artifacts(&sandbox_id).await?;

        assert!(!has_record(&persister, &sandbox_id).await?);
        Ok(())
    }

    #[tokio::test]
    async fn load_all_retains_unrecognized_record() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        persister
            .db()
            .await?
            .put(sandbox_id.to_string(), b"not-json")
            .await?;

        let loaded = persister.load_all(&MockBackendFactory::new()).await?;

        assert!(loaded.is_empty());
        assert!(has_record(&persister, &sandbox_id).await?);
        Ok(())
    }

    #[tokio::test]
    async fn load_all_discards_unusable_record_and_artifacts() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let persister = test_persister(temp.path());
        let sandbox_id = SandboxId::new();
        let snapshot_root = persister
            .sandbox_artifact_root(&sandbox_id)
            .join("snapshot");
        let paused_state = paused_state(&snapshot_root);
        let metadata = SandboxMetadata {
            id: sandbox_id,
            paused_state: Some(Arc::clone(&paused_state)),
            ..Default::default()
        };
        persister
            .persist_paused(&metadata, Some(&snapshot_root), paused_state.as_ref())
            .await?;

        // Only recognized legacy FC state grants the historical corruption cleanup.
        let db = persister.db().await?;
        let mut record: Value =
            serde_json::from_slice(&db.get(sandbox_id.to_string()).await?.unwrap())?;
        record.as_object_mut().unwrap().remove("backend");
        db.put(sandbox_id.to_string(), serde_json::to_vec(&record)?)
            .await?;

        let loaded = persister.load_all(&RejectingFactory).await?;

        assert!(loaded.is_empty());
        assert!(!has_record(&persister, &sandbox_id).await?);
        assert!(!persister.sandbox_artifact_root(&sandbox_id).exists());
        Ok(())
    }
}
