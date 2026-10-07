//! Abstractions for sandbox backends.
//!
//! [`SandboxBackend`] represents the lifecycle of a single sandbox instance.
//! [`SandboxBackendFactory`] is responsible for constructing new sandbox
//! instances (from scratch, from a committed snapshot, or from paused state).

use std::any::Any;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    EnvdAccessToken, EnvdApi, Executor, ExtraDrive, FreshSandboxBuildSpec, ProcessHandle,
    ProcessOpts, ProcessOutput, SandboxLaunchConfig, SandboxNetworkPolicy,
};
use crate::repository::RepositoryResult;
use crate::sandbox::CustomExtensionParams;
use crate::snapshot::{
    RunnableSnapshot, SandboxSnapshotManifest, SnapshotManager, SnapshotPublishMetadata,
    SnapshotRecord,
};
use crate::types::{RuntimeArtifactSet, SandboxId};

/// A concrete sandbox backend's paused state.
///
/// The Orchestrator treats this value as completely opaque: it stores it in
/// its `SandboxMetadata` after a `pause` call and passes it back to
/// [`SandboxBackendFactory::build_from_paused_state`] when a resume is requested.
/// Concrete implementations own their serialized form.
pub trait PausedSandboxState: Any + fmt::Debug + Send + Sync + 'static {
    fn encode(&self) -> Result<Value>;

    /// Transfer captured directory ownership only after the durable record commit.
    fn persistence_committed(&self) {}

    /// A live lease owns cleanup; persistence failure must not unlink its files.
    fn owns_artifact_cleanup(&self) -> bool {
        false
    }

    /// Local artifacts this paused sandbox will reopen on resume.
    /// The orchestrator only carries this value to the image-liveness layer; it
    /// does not interpret the backend-specific artifact identities inside it.
    fn runtime_artifacts(&self) -> RuntimeArtifactSet;
    /// Effective envd control-plane port persisted with the paused runtime, when available.
    fn control_plane_port(&self) -> Option<u16> {
        None
    }
}

impl dyn PausedSandboxState {
    pub fn downcast_ref<T>(&self) -> Option<&T>
    where
        T: PausedSandboxState,
    {
        (self as &dyn Any).downcast_ref::<T>()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SandboxCaptureError {
    #[error("{0}")]
    Recoverable(#[source] anyhow::Error),
    #[error("{0}")]
    Terminal(#[source] anyhow::Error),
}

impl SandboxCaptureError {
    pub fn recoverable(err: anyhow::Error) -> Self {
        Self::Recoverable(err)
    }

    pub fn terminal(err: anyhow::Error) -> Self {
        Self::Terminal(err)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }
}

impl From<anyhow::Error> for SandboxCaptureError {
    fn from(err: anyhow::Error) -> Self {
        match err.downcast::<Self>() {
            Ok(snapshot_err) => snapshot_err,
            Err(err) => Self::Recoverable(err),
        }
    }
}

pub type SandboxCaptureResult<T> = std::result::Result<T, SandboxCaptureError>;
pub type SandboxForkResult = anyhow::Result<Box<dyn SandboxBackend>>;

#[cfg(test)]
mod recording_contract_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn capture_recording_is_deferred_once_and_retains_template_workspace() {
        fn record(
            capture: &CapturedSandboxSnapshot,
            lease: Arc<dyn Any + Send + Sync>,
        ) -> Option<crate::snapshot::StartupRecording> {
            let calls = capture
                .downcast_artifacts_ref::<Arc<AtomicUsize>>()
                .unwrap()
                .clone();
            calls.fetch_add(1, Ordering::SeqCst);
            Some(crate::snapshot::StartupRecording::spawn_with_capture_lease(
                async { None },
                Arc::new(lease),
            ))
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let workspace = Arc::new(tempfile::tempdir().unwrap());
        let path = workspace.path().to_owned();
        let mut capture = CapturedSandboxSnapshot::new(
            SandboxSnapshotManifest::for_test(4096, &[]),
            calls.clone(),
        )
        .with_recording_factory(record)
        .with_recording_lease(workspace.clone());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let recording = capture.start_recording().unwrap();
        assert!(capture.start_recording().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(capture);
        drop(workspace);
        let crate::snapshot::StartupRecording { trace, keep_alive } = recording;
        trace.await.unwrap();
        assert!(path.exists(), "publication still owns the workspace");
        drop(keep_alive);
        assert!(!path.exists());
    }
}

#[derive(Clone, Debug)]
pub struct SandboxForkSpec {
    pub sandbox_id: SandboxId,
    pub envd_access_token: Option<EnvdAccessToken>,
    pub extra_drives: Vec<super::ExtraDrive>,
    /// Pairs of `(source_drive_id, replacement_drive_id)`.
    pub replace_drive_ids: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxRuntimeInfo {
    pub rootfs_virtual_size: Option<u64>,
    pub runtime_artifacts: RuntimeArtifactSet,
}

/// Opaque captured snapshot artifacts produced from a running sandbox.
///
/// Unlike [`PausedSandboxState`], this value is intended for one-shot
/// consumption by snapshot publication code. Concrete backends may use it to
/// keep temporary artifact directories alive until publication finishes.
pub struct CapturedSandboxSnapshot {
    manifest: SandboxSnapshotManifest,
    artifacts: Box<dyn Any + Send>,
    // Node-local immutable generations, kept alive by `artifacts`. Only their
    // published logical layers enter the committed volume_snapshots record.
    volume_drives: Option<Vec<ExtraDrive>>,
    recording_factory: Option<RecordingFactory>,
    recording_lease: Arc<dyn Any + Send + Sync>,
}

type RecordingFactory = fn(
    &CapturedSandboxSnapshot,
    Arc<dyn Any + Send + Sync>,
) -> Option<crate::snapshot::StartupRecording>;

impl CapturedSandboxSnapshot {
    /// Take a capture, with whatever the backend has to hold on to until
    /// publication is over. A backend which writes its artifacts under a
    /// temporary root passes the guard of that root as `artifacts`, and one
    /// which writes them somewhere durable passes `()`.
    pub fn new<T>(manifest: SandboxSnapshotManifest, artifacts: T) -> Self
    where
        T: Send + 'static,
    {
        Self {
            manifest,
            artifacts: Box::new(artifacts),
            volume_drives: None,
            recording_factory: None,
            recording_lease: Arc::new(()),
        }
    }

    pub fn with_recording_factory(mut self, factory: RecordingFactory) -> Self {
        self.recording_factory = Some(factory);
        self
    }

    /// Retain caller-owned template output through both recording and publication.
    pub fn with_recording_lease(mut self, lease: Arc<dyn Any + Send + Sync>) -> Self {
        self.recording_lease = lease;
        self
    }

    pub fn start_recording(&mut self) -> Option<crate::snapshot::StartupRecording> {
        self.recording_factory.take()?(self, self.recording_lease.clone())
    }

    pub fn with_volume_drives(mut self, drives: Vec<ExtraDrive>) -> Self {
        self.volume_drives = Some(drives);
        self
    }

    pub fn volume_drives(&self) -> Option<&[ExtraDrive]> {
        self.volume_drives.as_deref()
    }

    /// What was captured, in the form the snapshot layer publishes.
    pub fn manifest(&self) -> &SandboxSnapshotManifest {
        &self.manifest
    }

    /// The backend which took the capture.
    pub fn backend(&self) -> &str {
        &self.manifest.backend
    }

    pub fn downcast_artifacts_ref<T>(&self) -> Option<&T>
    where
        T: Send + 'static,
    {
        self.artifacts.downcast_ref::<T>()
    }
}

impl fmt::Debug for CapturedSandboxSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedSandboxSnapshot")
            .field("backend", &self.manifest.backend)
            .finish_non_exhaustive()
    }
}

/// Publishes `captured_snapshot` through `snapshots`. Its artifacts stay
/// alive until publication is over.
#[tracing::instrument(skip(snapshots, metadata), fields(snapshot_id = %metadata.id))]
pub async fn publish_captured(
    snapshots: &SnapshotManager,
    metadata: SnapshotPublishMetadata,
    captured_snapshot: CapturedSandboxSnapshot,
) -> RepositoryResult<SnapshotRecord> {
    let manifest = captured_snapshot.manifest().clone();
    let mut metadata = metadata;
    if let Some(volumes) = captured_snapshot.volume_drives() {
        metadata.volume_snapshots.clear();
        for drive in volumes {
            let size = drive
                .virtual_size()
                .filter(|size| *size > 0 && size % (1 << 20) == 0)
                .ok_or_else(|| crate::repository::RepositoryError::InvalidRequest {
                    reason: "captured volume has invalid capacity".into(),
                })?;
            let layers = crate::snapshot::store::store_volume_layers(
                snapshots.repository().as_ref(),
                drive.image_config_path(),
            )
            .await?;
            metadata
                .volume_snapshots
                .push(crate::snapshot::SnapshotVolume {
                    mount_path: drive.mount_path().to_string_lossy().into_owned(),
                    mode: if drive.read_only() {
                        crate::volume::VolumeMode::ReadOnly
                    } else {
                        crate::volume::VolumeMode::Exclusive
                    },
                    size_mb: size >> 20,
                    layers,
                });
        }
    }
    let mut captured_snapshot = captured_snapshot;
    let recording = captured_snapshot.start_recording();
    let result = snapshots.publish(metadata, manifest, recording).await;
    drop(captured_snapshot);
    result
}

/// Lifecycle interface for a single sandbox instance.
///
/// Implementors must be `Send + 'static` so that they can be stored inside
/// `Arc<Mutex<Box<dyn SandboxBackend>>>` handles managed by the Orchestrator.
#[async_trait]
pub trait SandboxBackend: Send + 'static {
    /// The guest's envd API, or None while no guest agent is connected. Take
    /// it under the runtime lock and call it after releasing the lock, so
    /// guest I/O never blocks lifecycle operations.
    fn envd(&self) -> Option<Arc<dyn EnvdApi>>;

    /// Start the sandbox and block until readiness.
    async fn start(&mut self) -> Result<()>;

    /// Start the sandbox without waiting for the sandbox to become ready.
    async fn start_nowait(&mut self) -> Result<()>;

    /// Block until the sandbox signals readiness.
    ///
    /// Should be called after [`start_nowait`][Self::start_nowait] before any
    /// workload is submitted.
    async fn wait_for_ready(&self) -> Result<()>;

    /// Pause the sandbox and capture its state for later resume.
    ///
    /// After this call the caller is expected to invoke [`stop`][Self::stop]
    /// to release system resources; the paused state encapsulates everything
    /// needed to resume the sandbox later via
    /// [`SandboxBackendFactory::build_from_paused_state`].
    ///
    /// [`SandboxCaptureError::Terminal`] indicates snapshot capture mutated the live
    /// runtime before failing, so callers must not keep treating the sandbox
    /// as safely runnable.
    ///
    /// For simplicity, [`SandboxCaptureError::Recoverable`] must guarantee the sandbox
    /// has already been restored to a running state before the error is returned.
    async fn pause(
        &mut self,
        artifact_root: Option<&Path>,
    ) -> SandboxCaptureResult<Arc<dyn PausedSandboxState>>;

    /// Resume a paused but not-yet-stopped sandbox from its snapshot.
    ///
    /// Idempotent: calling `resume` more than once must not return an error.
    async fn resume(&mut self) -> Result<()>;

    /// Capture a persistent snapshot from a running sandbox.
    ///
    /// After this call the sandbox is expected to continue running.
    ///
    /// [`SandboxCaptureError::Terminal`] indicates snapshot capture mutated the live
    /// runtime before failing, so callers must not keep treating the sandbox
    /// as safely runnable.
    async fn snapshot(&mut self) -> SandboxCaptureResult<CapturedSandboxSnapshot>;

    /// Flush and seal writable persistent-volume upper layers while keeping
    /// the sandbox running.
    ///
    /// The sealed layers are node-local until the volume catalog publishes
    /// them. This operation provides the runtime half of that publication
    /// barrier without capturing rootfs, memory, or VM state.
    async fn snapshot_volumes(&mut self) -> SandboxCaptureResult<()>;

    /// Fork this running sandbox into ready child backends.
    ///
    /// The outer error is reserved for failures before child startup begins.
    /// After the source has been restored, implementations must attempt every
    /// child concurrently and return one result per `spec` entry in the
    /// same order. Successful children stay running when a sibling fails.
    ///
    /// [`SandboxCaptureError::Terminal`] indicates the fork attempt mutated the
    /// source runtime past safe resume, so callers must stop treating the
    /// source as runnable. Child construction/start failures after source
    /// recovery belong in the corresponding [`SandboxForkResult`].
    async fn fork(
        &mut self,
        spec: &[SandboxForkSpec],
    ) -> SandboxCaptureResult<Vec<SandboxForkResult>>;

    /// Capture the guest into `at` and describe what was written.
    ///
    /// The guest is left held. A template build takes its snapshot this way
    /// and stops the sandbox afterwards, so unlike [`snapshot`][Self::snapshot]
    /// nothing is resumed and the directory is the caller's to keep. The
    /// returned capture owns its opaque backend payload and recording factory.
    async fn capture_to_dir(&mut self, at: &Path) -> SandboxCaptureResult<CapturedSandboxSnapshot>;

    /// Stop the sandbox and release all associated system resources.
    ///
    /// Repeated calls must not repeat completed or uncertain resource releases.
    /// A successful stop remains successful; an incomplete cleanup may keep returning its error.
    async fn stop(&mut self) -> Result<()>;

    /// Freeze writable persistent filesystems and seal their volume layers,
    /// without capturing memory, rootfs, or VM state. On success, writes remain
    /// frozen until `stop` or `thaw_volumes`. Recoverable errors guarantee that
    /// writes have resumed; terminal errors require runtime teardown.
    /// Callers must keep ownership through completion and thaw/stop; dropping
    /// this future does not cancel guest I/O. The orchestrator shields deletion
    /// from request cancellation with an owned task.
    async fn freeze_and_snapshot_volumes(&mut self) -> SandboxCaptureResult<()>;

    /// Resume writes after abandoning a deletion that froze the volumes.
    async fn thaw_volumes(&mut self) -> Result<()>;

    /// Obtain the IP address that the sandbox can use to interact with the host.
    fn host_interaction_ip(&self) -> Option<std::net::Ipv4Addr>;

    /// Return runtime facts that are only known after the backend has started.
    fn runtime_info(&self) -> SandboxRuntimeInfo;

    /// Local runtime artifacts this sandbox opens on start.
    fn startup_artifacts(&self) -> RuntimeArtifactSet;

    /// Update the sandbox network policy at runtime.
    async fn update_network_policy(&mut self, policy: Option<SandboxNetworkPolicy>) -> Result<()>;

    /// Update the custom extension params held by the sandbox runtime.
    ///
    /// Plain assignment of an already-approved value: the custom extension
    /// patch-params hook is invoked by the caller (orchestrator layer), not
    /// by the backend. Cannot fail.
    fn update_custom_extension_params(&mut self, params: Option<CustomExtensionParams>);
}

/// Factory interface for creating and restoring sandbox backend instances.
///
/// A single factory instance is stored inside the `Orchestrator` and is
/// used for every `create_sandbox` and `resume_sandbox` request.
pub trait SandboxBackendFactory: Send + Sync + 'static {
    fn backend(&self) -> &str;

    /// Validate a resolved snapshot before the caller allocates volumes or runtime
    /// resources. Implementations must not create a sandbox or acquire leases.
    fn validate_snapshot(&self, snapshot: &RunnableSnapshot) -> Result<()>;

    /// Pure preflight: an unsupported reader must never decode or allocate state.
    fn supports_restore(
        &self,
        _identity: &super::restore_identity::SnapshotRestoreIdentity,
    ) -> bool {
        false
    }

    /// Optional backend override; None preserves the configured Firecracker metadata.
    fn runtime_versions(&self) -> Result<Option<crate::snapshot::SnapshotRuntimeVersions>> {
        Ok(None)
    }
    /// Build a brand-new sandbox backend from a high-level launch request.
    fn build(
        &self,
        build_spec: FreshSandboxBuildSpec,
        launch_config: SandboxLaunchConfig,
    ) -> Result<Box<dyn SandboxBackend>>;

    /// Build a sandbox backend from a runnable committed snapshot plus launch request.
    fn build_from_snapshot(
        &self,
        snapshot: &RunnableSnapshot,
        launch_config: SandboxLaunchConfig,
    ) -> Result<Box<dyn SandboxBackend>>;

    /// Decode backend-specific paused state loaded from persistence.
    fn decode_paused_state(
        &self,
        artifact_root: PathBuf,
        state: Value,
    ) -> Result<Arc<dyn PausedSandboxState>>;

    /// Build a sandbox backend from backend-specific paused state captured by `pause`.
    fn build_from_paused_state(
        &self,
        sandbox_id: crate::types::SandboxId,
        state: &dyn PausedSandboxState,
        envd_access_token: Option<EnvdAccessToken>,
    ) -> Result<Box<dyn SandboxBackend>>;
}

/// Template construction combines lifecycle and guest-command capabilities.
/// Concrete adapters translate the neutral inputs without downcasts in callers.
#[async_trait(?Send)]
pub trait TemplateSandboxFactory: SandboxBackendFactory {
    type Sandbox: SandboxBackend + SandboxExecutor;

    fn build_template(
        &self,
        spec: FreshSandboxBuildSpec,
        launch: SandboxLaunchConfig,
        ublk: Option<super::UblkConfig>,
    ) -> Result<Self::Sandbox>;
    fn build_template_from_snapshot(
        &self,
        snapshot: &RunnableSnapshot,
        launch: SandboxLaunchConfig,
    ) -> Result<Self::Sandbox>;
    async fn template_versions(
        &self,
        sandbox: &Self::Sandbox,
        tools_version: String,
    ) -> Result<crate::snapshot::SnapshotRuntimeVersions>;
}

/// Process execution capability of a running sandbox.
///
/// Implement [`executor`][Self::executor] to provide a [`ProcessClient`][envd::process::ProcessClient]-backed
/// [`Executor`]. The three convenience methods (`run_command`,
/// `run_command_with_opts`, `start_process`) have default implementations that
/// simply call `self.executor()?` and delegate, so callers can continue using
/// the familiar `sandbox.run_command(...)` pattern without boilerplate.
///
/// # Note on `Send`
/// `&Self` may be `!Send` (e.g. `FirecrackerSandbox` holds tonic clients that
/// are `!Sync`), so the generated futures are not required to be `Send`.
#[async_trait(?Send)]
pub trait SandboxExecutor: Send {
    /// Obtain a process executor backed by this sandbox's envd connection.
    ///
    /// Returns an error if the sandbox is not running.
    fn executor(&self) -> Result<Executor>;

    /// Run a command inside the sandbox and wait for it to complete.
    ///
    /// Returns the captured stdout, stderr, and exit code.
    ///
    /// # Example
    /// ```no_run
    /// use agentenv::sandbox::SandboxExecutor;
    /// # async fn example(sandbox: &impl SandboxExecutor) -> anyhow::Result<()> {
    /// let output = sandbox.run_command("echo", &["hello", "world"]).await?;
    /// assert_eq!(output.exit_code, 0);
    /// println!("{}", output.stdout);
    /// # Ok(())
    /// # }
    /// ```
    async fn run_command(&self, cmd: &str, args: &[&str]) -> Result<ProcessOutput> {
        self.executor()?.run_command(cmd, args).await
    }

    /// Run a command with custom options and wait for it to complete.
    ///
    /// # Example
    /// ```no_run
    /// use agentenv::sandbox::{ProcessOpts, SandboxExecutor};
    /// use std::collections::HashMap;
    /// # async fn example(sandbox: &impl SandboxExecutor) -> anyhow::Result<()> {
    /// let opts = ProcessOpts::new().with_cwd("/tmp");
    /// let output = sandbox.run_command_with_opts("ls", &["-la"], &opts).await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn run_command_with_opts(
        &self,
        cmd: &str,
        args: &[&str],
        opts: &ProcessOpts,
    ) -> Result<ProcessOutput> {
        self.executor()?
            .run_command_with_opts(cmd, args, opts)
            .await
    }

    /// Create a directory (and any missing parents) inside the sandbox.
    ///
    /// Goes through envd's filesystem service rather than exec'ing a binary,
    /// so it works in images that ship no userland (scratch, distroless).
    /// An already-existing directory is not an error.
    ///
    /// # Example
    /// ```no_run
    /// use agentenv::sandbox::SandboxExecutor;
    /// # async fn example(sandbox: &impl SandboxExecutor) -> anyhow::Result<()> {
    /// sandbox.create_dir_all("/home/user/work").await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn create_dir_all(&self, path: &str) -> Result<()> {
        self.executor()?.create_dir_all(path).await
    }

    /// Start a long-running process and return a handle.
    ///
    /// # Example
    /// ```no_run
    /// use agentenv::sandbox::{ProcessOpts, SandboxExecutor};
    /// # async fn example(sandbox: &impl SandboxExecutor) -> anyhow::Result<()> {
    /// let mut handle = sandbox.start_process("cat", &[], &ProcessOpts::default()).await?;
    /// handle.send_stdin(b"hello\n").await?;
    /// handle.kill().await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn start_process(
        &self,
        cmd: &str,
        args: &[&str],
        opts: &ProcessOpts,
    ) -> Result<ProcessHandle> {
        self.executor()?.start_process(cmd, args, opts).await
    }
}
