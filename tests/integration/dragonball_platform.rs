//! Real platform contracts. Each case owns a subprocess/config so global
//! repository, extension and ublk settings cannot leak between tests.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use agentenv::{
    cfg::{AppConfig, ConfigManager, OssBackendConfig, SnapshotPublishCompressionConfig},
    orchestrator::{
        CreateSandboxRequest, FileBackedSandboxPersister, InMemoryMetadataStore, NewTimeout,
        Orchestrator, SandboxLaunchSource, SandboxTimeoutAction,
    },
    sandbox::{
        dragonball::{DragonballMemoryMode, DragonballRuntimePolicy, DragonballSandboxFactory},
        SandboxBackend, SandboxBackendFactory, SandboxExecutor,
    },
    snapshot::{backends::OssBackend, SnapshotCatalog, SnapshotManager},
    virtualization::VirtualizationMode,
};
use agentenv::{
    identity::NodeIdentity, p2p::*, repository::Repository,
    snapshot::backends::build_snapshot_backend,
};
use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::dragonball_support::{asset, fixture, fork_command, launch, publish_metadata, spec};

async fn subprocess_case(name: &str) -> Result<bool> {
    if std::env::var("AENV_M3_CASE").as_deref() == Ok(name) {
        return Ok(false);
    }
    let result = case_output(
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                &format!("dragonball_platform::{name}"),
                "--nocapture",
            ])
            .env("AENV_M3_CASE", name),
        Duration::from_secs(300),
    )
    .await?;
    eprintln!("{}", String::from_utf8_lossy(&result.stdout));
    eprintln!("{}", String::from_utf8_lossy(&result.stderr));
    anyhow::ensure!(
        result.status.success(),
        "M3 case {name} failed: {}",
        result.status
    );
    Ok(true)
}

async fn case_output(
    command: &mut tokio::process::Command,
    deadline: Duration,
) -> Result<std::process::Output> {
    // The case may be SIGKILLed and cannot run fixture destructors. Own its
    // native service process group here and adopt/reap the orphaned children.
    anyhow::ensure!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0);
    let child = command
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut group = CaseProcessGroup(child.id().context("case process id")? as i32);
    let output = child.wait_with_output();
    tokio::pin!(output);
    let completed = tokio::time::timeout(deadline, &mut output).await;
    group.kill()?;
    let timed_out = completed.is_err();
    // Keep the leader's wait future alive through termination, including its pipes.
    let output = match completed {
        Ok(output) => output,
        Err(_) => output.await,
    };
    let pgid = group.0;
    tokio::task::spawn_blocking(move || loop {
        let result = unsafe { libc::waitpid(-pgid, std::ptr::null_mut(), 0) };
        if result >= 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(()),
            Some(libc::EINTR) => continue,
            _ => return Err(error),
        }
    })
    .await??;
    group.0 = 0;
    anyhow::ensure!(!timed_out, "M3 case deadline expired after {deadline:?}");
    Ok(output?)
}

struct CaseProcessGroup(i32);

impl CaseProcessGroup {
    fn kill(&self) -> std::io::Result<()> {
        if self.0 > 0 && unsafe { libc::kill(-self.0, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for CaseProcessGroup {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[tokio::test]
#[ignore = "requires native M3 S3 and registry fixtures"]
async fn dragonball_platform_timeout_reaps_native_fixtures() -> Result<()> {
    if let Some(root) = std::env::var_os("AENV_M3_TIMEOUT_ROOT") {
        let root = Path::new(&root);
        let s3 = LocalS3::start(root).await?;
        let registry = LocalRegistry::start(root).await?;
        std::fs::write(
            root.join("ready.json"),
            serde_json::to_vec(&[s3.process.id().unwrap(), registry.process.id().unwrap()])?,
        )?;
        std::future::pending::<()>().await;
        return Ok(());
    }
    // Adopt descendants even in the failing regression so its cleanup can reap them.
    anyhow::ensure!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0);
    let root = tempfile::Builder::new()
        .prefix("db-m3-timeout-")
        .tempdir()?;
    let path = root.path().to_path_buf();
    let result = case_output(
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "dragonball_platform::dragonball_platform_timeout_reaps_native_fixtures",
                "--nocapture",
            ])
            .env("AENV_M3_TIMEOUT_ROOT", &path),
        Duration::from_secs(10),
    )
    .await;
    let pids: Vec<i32> = serde_json::from_slice(
        &std::fs::read(path.join("ready.json"))
            .context("native fixtures must be ready before the forced timeout")?,
    )?;
    let survivors: Vec<_> = pids
        .iter()
        .copied()
        .filter(|pid| Path::new(&format!("/proc/{pid}")).exists())
        .collect();
    // Clean only the exact processes created by this regression, including on RED.
    for pid in &survivors {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
            libc::waitpid(*pid, std::ptr::null_mut(), 0);
        }
    }
    root.close()?;
    anyhow::ensure!(result.is_err(), "forced case deadline must expire");
    anyhow::ensure!(!path.exists(), "fixture directory remains after cleanup");
    anyhow::ensure!(
        survivors.is_empty(),
        "native fixtures survived case timeout: {survivors:?}"
    );
    Ok(())
}

// Observe the real transport boundary; all successful transfers still use Iroh.
struct ObservedPeer {
    inner: Arc<dyn P2pTransport>,
    hints: Vec<P2pArtifactProviderHint>,
    commit: Option<(Arc<dyn Repository>, String)>,
    publications: AtomicUsize,
    downloads: AtomicUsize,
    fail_publish: AtomicBool,
}

impl ObservedPeer {
    async fn new(app: &AppConfig, root: &Path, name: &str) -> Result<Self> {
        let mut config = app.clone();
        config.p2p.enabled = true;
        config.p2p.store_dir = root.join(name);
        config.p2p.listen_addr = "127.0.0.1:0".into();
        config.p2p.lookup_timeout_ms = 500;
        config.p2p.fetch_timeout_ms = 30000;
        config.node_identity.node_id = Some(name.into());
        Ok(Self {
            inner: transport_from_config(
                &config,
                &NodeIdentity::from_config(&config.node_identity),
            )
            .await?,
            hints: vec![],
            commit: None,
            publications: AtomicUsize::new(0),
            downloads: AtomicUsize::new(0),
            fail_publish: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl P2pTransport for ObservedPeer {
    async fn lookup_with_hints(
        &self,
        key: &P2pArtifactKey,
        hints: &[P2pArtifactProviderHint],
    ) -> P2pResult<Option<P2pArtifactDescriptor>> {
        self.inner
            .lookup_with_hints(key, if hints.is_empty() { &self.hints } else { hints })
            .await
    }
    async fn fetch_with_options(
        &self,
        descriptor: &P2pArtifactDescriptor,
        destination: &Path,
        options: P2pFetchOptions,
    ) -> P2pResult<u64> {
        let size = self
            .inner
            .fetch_with_options(descriptor, destination, options)
            .await?;
        self.downloads.fetch_add(1, Ordering::SeqCst);
        Ok(size)
    }
    async fn fetch_bytes_with_options(
        &self,
        descriptor: &P2pArtifactDescriptor,
        options: P2pFetchOptions,
    ) -> P2pResult<bytes::Bytes> {
        let bytes = self
            .inner
            .fetch_bytes_with_options(descriptor, options)
            .await?;
        self.downloads.fetch_add(1, Ordering::SeqCst);
        Ok(bytes)
    }
    async fn fetch_byte_range(
        &self,
        descriptor: &P2pArtifactDescriptor,
        offset: u64,
        len: usize,
    ) -> P2pResult<P2pByteStream> {
        self.inner.fetch_byte_range(descriptor, offset, len).await
    }
    async fn publish(&self, request: &P2pPublishRequest) -> P2pResult<()> {
        if let Some((repository, id)) = &self.commit {
            let record = repository.get(id).await.map_err(anyhow::Error::from)?;
            assert!(
                record.is_some_and(|r| r.committed.is_some()),
                "peer publication preceded durable commit"
            );
        }
        self.publications.fetch_add(1, Ordering::SeqCst);
        if self.fail_publish.load(Ordering::SeqCst) {
            return Err(P2pError::TransportDisabled);
        }
        self.inner.publish(request).await
    }
    async fn unpublish(&self, key: &P2pArtifactKey) -> P2pResult<bool> {
        self.inner.unpublish(key).await
    }
    fn local_endpoint(&self) -> Option<P2pEndpoint> {
        self.inner.local_endpoint()
    }
    async fn shutdown(&self) -> P2pResult<()> {
        self.inner.shutdown().await
    }
}

#[tokio::test]
#[ignore = "real KVM/ublk, local S3 and two real peer endpoints"]
async fn dragonball_platform_oss_p2p_commit_and_fallback() -> Result<()> {
    use sha2::{Digest, Sha256};
    if subprocess_case("dragonball_platform_oss_p2p_commit_and_fallback").await? {
        return Ok(());
    }
    let root = tempfile::Builder::new().prefix("db-m3-peer-").tempdir()?;
    let mut s3 = LocalS3::start(root.path()).await?;
    let (app, assets) = configure(root.path(), Some(&s3.config), None).await?;
    let factory = factory(&app, &assets)?;
    let spec = spec(&assets)?;
    let initial = launch(&assets)?;
    let mut source = factory.build_fresh(spec.clone(), initial.clone())?;
    source.start().await?;
    assert_eq!(
        source
            .run_command("/bin/sh", &["-ec", "printf peer-origin >/dev/shm/m3-peer"])
            .await?
            .exit_code,
        0
    );
    let captured = source.snapshot().await?;
    let expected_state = std::fs::read(&captured.manifest().vm_state.path)?;
    let metadata = publish_metadata(initial.sandbox_id, &spec, &assets, &captured)?;
    let (repository, resolver) = OssBackend::new_with_publish_compression(
        &s3.config,
        root.path().join("producer"),
        &SnapshotPublishCompressionConfig {
            enabled: false,
            ..Default::default()
        },
    )?
    .into_parts();
    let mut provider = ObservedPeer::new(&app, root.path(), "provider").await?;
    provider.commit = Some((repository.clone(), metadata.id.to_string()));
    let provider = Arc::new(provider);
    let producer = SnapshotManager::from_parts(repository, resolver, Some(provider.clone()));
    let stored = agentenv::sandbox::publish_captured(&producer, metadata, captured).await?;
    assert!(provider.publications.load(Ordering::SeqCst) >= 3);
    source.stop().await?;
    drop(source);
    let mut consumer = ObservedPeer::new(&app, root.path(), "consumer").await?;
    consumer.hints.push(P2pArtifactProviderHint {
        node_id: Some("provider".into()),
        endpoint: provider.local_endpoint(),
    });
    let consumer = Arc::new(consumer);
    let state_key = format!("snapshot/v1/artifacts/{}/vm_state.bin", stored.id);
    let descriptor = consumer
        .lookup(&state_key)
        .await?
        .context("peer VM state")?;
    assert_eq!(
        consumer.fetch_bytes(&descriptor).await?.as_ref(),
        expected_state
    );
    let manifest_key = format!(
        "snapshot/v1/artifacts/{}/firecracker-manifest.json",
        stored.id
    );
    let descriptor = consumer
        .lookup(&manifest_key)
        .await?
        .context("peer manifest")?;
    let manifest: agentenv::sandbox::SandboxSnapshotManifest =
        serde_json::from_slice(&consumer.fetch_bytes(&descriptor).await?)?;
    assert_eq!(manifest.backend, "dragonball");
    for layer in &stored
        .committed
        .as_ref()
        .context("committed")?
        .memory_layers
    {
        let key = format!("overlaybd-layer/v1/{}", layer.digest);
        let descriptor = consumer.lookup(&key).await?.context("peer memory layer")?;
        let destination = root.path().join("downloaded-layer");
        assert_eq!(consumer.fetch(&descriptor, &destination).await?, layer.size);
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(std::fs::read(&destination)?)),
            layer.digest
        );
    }
    let before = consumer.downloads.load(Ordering::SeqCst);
    let (repository, resolver) = build_snapshot_backend(Some(consumer.clone()))?;
    let manager = SnapshotManager::from_parts(repository, resolver, None);
    let runnable = manager
        .load_runnable(stored.id.to_string())
        .await?
        .context("peer resolved snapshot")?;
    assert_eq!(
        std::fs::read(&runnable.manifest().vm_state.path)?,
        expected_state
    );
    assert!(
        consumer.downloads.load(Ordering::SeqCst) > before,
        "resolver did not consult the real peer"
    );
    drop(runnable);
    drop(manager);
    provider.shutdown().await?;
    consumer.shutdown().await?;
    // A different consumer has neither downloaded blobs nor cached descriptors.
    let mut cold = ObservedPeer::new(&app, root.path(), "cold-consumer").await?;
    cold.hints.clone_from(&consumer.hints);
    let consumer = Arc::new(cold);
    // Only this subprocess's derived cache is removed. Durable OSS and peer data remain.
    assert!(app.snapshot.local_cache_path.starts_with(root.path()));
    std::fs::remove_dir_all(&app.snapshot.local_cache_path)?;
    let before = consumer.downloads.load(Ordering::SeqCst);
    let (repository, resolver) = build_snapshot_backend(Some(consumer.clone()))?;
    let manager = SnapshotManager::from_parts(repository, resolver, None);
    let runnable = manager
        .load_runnable(stored.id.to_string())
        .await?
        .context("origin fallback")?;
    assert_eq!(
        consumer.downloads.load(Ordering::SeqCst),
        before,
        "closed provider unexpectedly served data"
    );
    let child_launch = launch(&assets)?;
    let token = child_launch.envd_access_token.clone();
    let mut child = factory.build_from_snapshot(&runnable, child_launch)?;
    child.start().await?;
    assert_eq!(
        fork_command(
            child.as_ref(),
            token.as_ref(),
            "test \"$(cat /dev/shm/m3-peer)\" = peer-origin"
        )
        .await?
        .exit_code,
        0
    );
    let captured = child.snapshot().await?;
    let metadata = publish_metadata(initial.sandbox_id, &spec, &assets, &captured)?;
    let (repository, resolver) = build_snapshot_backend(None)?;
    let mut failing = ObservedPeer::new(&app, root.path(), "failing-provider").await?;
    failing.commit = Some((repository.clone(), metadata.id.to_string()));
    failing.fail_publish.store(true, Ordering::SeqCst);
    let failing = Arc::new(failing);
    let manager = SnapshotManager::from_parts(repository, resolver, Some(failing.clone()));
    let retained = agentenv::sandbox::publish_captured(&manager, metadata, captured).await?;
    assert!(failing.publications.load(Ordering::SeqCst) >= 2);
    assert!(
        oss_manager(&s3.config, root.path().join("final-empty-cache"))?
            .load_runnable(retained.id.to_string())
            .await?
            .is_some()
    );
    assert_eq!(
        fork_command(
            child.as_ref(),
            token.as_ref(),
            "test \"$(cat /dev/shm/m3-peer)\" = peer-origin"
        )
        .await?
        .exit_code,
        0
    );
    child.stop().await?;
    failing.shutdown().await?;
    consumer.shutdown().await?;
    s3.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "real KVM/ublk and two real peer endpoints"]
async fn dragonball_platform_posix_missing_artifact_is_not_peer_repaired() -> Result<()> {
    if subprocess_case("dragonball_platform_posix_missing_artifact_is_not_peer_repaired").await? {
        return Ok(());
    }
    let root = tempfile::Builder::new()
        .prefix("db-m3-posix-peer-")
        .tempdir()?;
    let (app, assets) = configure(root.path(), None, None).await?;
    let factory = factory(&app, &assets)?;
    let spec = spec(&assets)?;
    let initial = launch(&assets)?;
    let mut source = factory.build_fresh(spec.clone(), initial.clone())?;
    source.start().await?;
    let captured = source.snapshot().await?;
    let expected = std::fs::read(&captured.manifest().vm_state.path)?;
    let metadata = publish_metadata(initial.sandbox_id, &spec, &assets, &captured)?;
    let (repository, resolver) = build_snapshot_backend(None)?;
    let mut provider = ObservedPeer::new(&app, root.path(), "posix-provider").await?;
    provider.commit = Some((repository.clone(), metadata.id.to_string()));
    let provider = Arc::new(provider);
    let manager = SnapshotManager::from_parts(repository, resolver, Some(provider.clone()));
    let stored = agentenv::sandbox::publish_captured(&manager, metadata, captured).await?;
    source.stop().await?;
    let mut consumer = ObservedPeer::new(&app, root.path(), "posix-consumer").await?;
    consumer.hints.push(P2pArtifactProviderHint {
        node_id: Some("posix-provider".into()),
        endpoint: provider.local_endpoint(),
    });
    let consumer = Arc::new(consumer);
    let key = format!("snapshot/v1/artifacts/{}/vm_state.bin", stored.id);
    let descriptor = consumer.lookup(&key).await?.context("POSIX peer content")?;
    assert_eq!(consumer.fetch_bytes(&descriptor).await?.as_ref(), expected);
    let runnable = manager
        .load_runnable(stored.id.to_string())
        .await?
        .context("POSIX runnable")?;
    let owned_file = runnable.manifest().vm_state.path.clone();
    assert!(owned_file.starts_with(
        &app.backend
            .posix_fs
            .as_ref()
            .context("POSIX configuration")?
            .snapshot_store
    ));
    assert!(owned_file.starts_with(root.path()));
    drop(runnable);
    std::fs::remove_file(&owned_file)?;
    let before = consumer.downloads.load(Ordering::SeqCst);
    let (repository, resolver) = build_snapshot_backend(Some(consumer.clone()))?;
    let manager = SnapshotManager::from_parts(repository, resolver, None);
    assert!(manager.load_runnable(stored.id.to_string()).await.is_err());
    assert!(
        !owned_file.exists(),
        "POSIX durable file was repaired from a peer"
    );
    assert_eq!(consumer.downloads.load(Ordering::SeqCst), before);
    assert_eq!(
        consumer.fetch_bytes(&descriptor).await?.as_ref(),
        expected,
        "peer content should still exist"
    );
    consumer.shutdown().await?;
    provider.shutdown().await?;
    Ok(())
}

struct LocalS3 {
    process: tokio::process::Child,
    config: OssBackendConfig,
}

struct LocalRegistry {
    process: tokio::process::Child,
    client: reqwest::Client,
    address: String,
    user: String,
    password: String,
    docker_config: PathBuf,
}

impl LocalRegistry {
    async fn start(root: &Path) -> Result<Self> {
        use base64::Engine;
        let certificate = root.join("registry-ca.pem");
        let key = root.join("registry-key.pem");
        let output = tokio::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "fixture certificate generation failed"
        );
        std::env::set_var("SSL_CERT_FILE", &certificate);
        let user = format!("m3{}", uuid::Uuid::now_v7().simple());
        let password = uuid::Uuid::now_v7().to_string();
        let output = tokio::process::Command::new("htpasswd")
            .args(["-Bbn", &user, &password])
            .output()
            .await?;
        anyhow::ensure!(output.status.success(), "fixture htpasswd failed");
        let auth_path = root.join("htpasswd");
        std::fs::write(&auth_path, output.stdout)?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        drop(listener);
        let registry_config = root.join("registry.yml");
        // JSON is also valid YAML and avoids quoting platform paths by hand.
        std::fs::write(
            &registry_config,
            serde_json::to_vec(&json!({
                "version": 0.1, "log": {"level": "error"},
                "storage": {"filesystem": {"rootdirectory": root.join("registry-data")}},
                "http": {"addr": address, "tls": {"certificate": certificate, "key": key}},
                "auth": {"htpasswd": {"realm": "M3 fixture", "path": auth_path}}
            }))?,
        )?;
        let log = std::fs::File::create(root.join("registry.log"))?;
        let mut process = tokio::process::Command::new(
            std::env::var_os("AENV_M3_REGISTRY_SERVER")
                .context("explicit registry fixture binary")?,
        )
        .arg("serve")
        .arg(&registry_config)
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()?;
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(
                &certificate,
            )?)?)
            .build()?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                anyhow::ensure!(
                    process.try_wait()?.is_none(),
                    "registry exited; see registry.log"
                );
                if client
                    .get(format!("https://{address}/v2/"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let docker_config = root.join("docker-config");
        std::fs::create_dir(&docker_config)?;
        std::fs::write(
            docker_config.join("config.json"),
            serde_json::to_vec(
                &json!({"auths": {address.clone(): {"auth": base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))}}}),
            )?,
        )?;
        std::env::set_var("DOCKER_CONFIG", &docker_config);
        std::env::remove_var("DOCKER_AUTH_CONFIG");
        let regctl_config = root.join("regctl.json");
        std::fs::write(
            &regctl_config,
            serde_json::to_vec(
                &json!({"version": 1, "incDockerCred": true, "hosts": {address.clone(): {"tls": "enabled", "regCert": std::fs::read_to_string(&certificate)?}}}),
            )?,
        )?;
        std::env::set_var("REGCTL_CONFIG", regctl_config);
        Ok(Self {
            process,
            client,
            address,
            user,
            password,
            docker_config,
        })
    }

    async fn upload_blob(&self, repository: &str, path: &Path, digest: &str) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "https://{}/v2/{repository}/blobs/uploads/",
                self.address
            ))
            .basic_auth(&self.user, Some(&self.password))
            .send()
            .await?
            .error_for_status()?;
        let location = response
            .headers()
            .get("location")
            .context("blob upload location")?
            .to_str()?;
        let mut url = reqwest::Url::parse(&format!("https://{}", self.address))?.join(location)?;
        url.query_pairs_mut().append_pair("digest", digest);
        // Stream large immutable lowers without buffering a complete rootfs in RAM.
        self.client
            .put(url)
            .basic_auth(&self.user, Some(&self.password))
            .header("content-type", "application/octet-stream")
            .header("content-length", std::fs::metadata(path)?.len())
            .body(tokio::fs::File::open(path).await?)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "real KVM/ublk and authenticated TLS OCI registry"]
async fn dragonball_platform_private_registry_template() -> Result<()> {
    use agentenv::{
        image::ImageResolver,
        sandbox::{SandboxLaunchConfig, TemplateSandboxFactory},
        template::TemplateBuildSpec,
    };
    use sha2::{Digest, Sha256};
    if subprocess_case("dragonball_platform_private_registry_template").await? {
        return Ok(());
    }
    let root = tempfile::Builder::new()
        .prefix("db-m3-registry-")
        .tempdir()?;
    let mut registry = LocalRegistry::start(root.path()).await?;
    let (app, assets) = configure(root.path(), None, None).await?;
    let repository = format!("m3/{}", uuid::Uuid::now_v7().simple());
    let image_ref = format!("{}/{repository}:native", registry.address);
    let local: Value = serde_json::from_slice(&std::fs::read(asset(&assets, "image_path")?)?)?;
    let mut layers = vec![];
    for lower in local["lowers"]
        .as_array()
        .context("prepared native lowers")?
    {
        let path = Path::new(lower["file"].as_str().context("prepared lower path")?);
        let digest = lower["digest"].as_str().context("lower digest")?;
        registry.upload_blob(&repository, path, digest).await?;
        layers.push(json!({"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest, "size": std::fs::metadata(path)?.len(), "annotations": {"containerd.io/snapshot/overlaybd/blob-digest": digest}}));
    }
    let config = serde_json::to_vec(
        &json!({"architecture": "amd64", "os": "linux", "config": assets["image_configs"][0]["config"], "rootfs": {"type": "layers", "diff_ids": layers.iter().map(|l| l["digest"].clone()).collect::<Vec<_>>()}}),
    )?;
    let config_digest = format!("sha256:{:x}", Sha256::digest(&config));
    let config_path = root.path().join("oci-config.json");
    std::fs::write(&config_path, &config)?;
    registry
        .upload_blob(&repository, &config_path, &config_digest)
        .await?;
    let manifest = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()}, "layers": layers});
    registry
        .client
        .put(format!(
            "https://{}/v2/{repository}/manifests/native",
            registry.address
        ))
        .basic_auth(&registry.user, Some(&registry.password))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(serde_json::to_vec(&manifest)?)
        .send()
        .await?
        .error_for_status()?;
    let credentials = std::fs::read(registry.docker_config.join("config.json"))?;
    std::fs::write(
        registry.docker_config.join("config.json"),
        b"{\"auths\":{}}",
    )?;
    assert!(
        ImageResolver::new(&app).resolve(&image_ref).await.is_err(),
        "private image resolved without credentials"
    );
    std::fs::write(registry.docker_config.join("config.json"), credentials)?;
    let resolved = ImageResolver::new(&app).resolve(&image_ref).await?;
    let config: Value = serde_json::from_slice(&std::fs::read(&resolved.overlaybd_config_path)?)?;
    assert_eq!(
        config["repoBlobUrl"],
        format!("https://{}/v2/{repository}/blobs", registry.address)
    );
    for lower in config["lowers"].as_array().context("remote lowers")? {
        assert!(
            lower.get("file").is_none(),
            "native registry layer eagerly materialized"
        );
        assert!(lower["digest"].as_str().is_some());
    }
    let (builder, manager, _) = crate::common::snapshot_test_parts(&root.path().join("templates"));
    let build = TemplateBuildSpec::new()
        .from_overlaybd_config(&resolved.overlaybd_config_path)
        .resources(1, 1024)
        .run("printf private-registry >/var/tmp/m3-registry-marker");
    let stored = builder.build_and_publish(&manager, build).await?;
    let serialized = serde_json::to_string(&stored)?;
    assert!(!serialized.contains(&registry.password));
    assert!(!serialized.contains(&registry.user));
    let runnable = manager
        .load_runnable(stored.id.to_string())
        .await?
        .context("private template")?;
    let mut guest = factory(&app, &assets)?
        .build_template_from_snapshot(&runnable, SandboxLaunchConfig::default())?;
    guest.start().await?;
    let result = guest
        .run_command(
            "/agentenv/bin/busybox",
            &["cat", "/var/tmp/m3-registry-marker"],
        )
        .await?;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "private-registry");
    guest.stop().await?;
    registry.process.kill().await?;
    registry.process.wait().await?;
    Ok(())
}

impl LocalS3 {
    async fn start(root: &Path) -> Result<Self> {
        let binary = std::env::var_os("AENV_M3_S3_SERVER").context("explicit S3 fixture binary")?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let user = format!("m3{}", uuid::Uuid::now_v7().simple());
        let password = uuid::Uuid::now_v7().to_string();
        let log = std::fs::File::create(root.join("s3.log"))?;
        let mut process = tokio::process::Command::new(binary)
            .args([
                "server",
                "--address",
                &address.to_string(),
                "--console-address",
                "127.0.0.1:0",
            ])
            .arg(root.join("s3-data"))
            .env("MINIO_ROOT_USER", &user)
            .env("MINIO_ROOT_PASSWORD", &password)
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        let endpoint = format!("http://{address}");
        let client = reqwest::Client::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                anyhow::ensure!(
                    process.try_wait()?.is_none(),
                    "S3 server exited; see s3.log"
                );
                if client
                    .get(format!("{endpoint}/minio/health/ready"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        // Exercise the actual S3 API, rather than creating bucket directories.
        let result = tokio::process::Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--max-time",
                "10",
                "--aws-sigv4",
                "aws:amz:us-east-1:s3",
                "--user",
            ])
            .arg(format!("{user}:{password}"))
            .args(["--request", "PUT", &format!("{endpoint}/m3-test")])
            .output()
            .await?;
        anyhow::ensure!(
            result.status.success(),
            "S3 create bucket: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(Self {
            process,
            config: OssBackendConfig {
                endpoint,
                bucket: "m3-test".into(),
                prefix: Some("m3".into()),
                credential_process: None,
                access_key_id: Some(user),
                access_key_secret: Some(password),
                security_token: None,
                region: Some("us-east-1".into()),
                addressing_style: None,
                cache_max_size_gb: Some(1),
                timeout_secs: None,
            },
        })
    }

    async fn stop(&mut self) -> Result<()> {
        self.process.kill().await?;
        self.process.wait().await?;
        Ok(())
    }
}

async fn configure(
    root: &Path,
    oss: Option<&OssBackendConfig>,
    extension: Option<&str>,
) -> Result<(AppConfig, Value)> {
    let original = std::env::var_os("AENV_CONFIG_PATH").context("isolated baseline config")?;
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(original)?)?;
    std::env::set_var("AENV_HOME_PATH", root.join("home"));
    std::env::set_var("AENV_RUNTIME_PATH", root.join("run"));
    config["snapshot"]["memory_startup_pack"]["enabled"] = false.into();
    if let Some(oss) = oss {
        config["snapshot"]
            .as_table_mut()
            .context("snapshot fixture table")?
            .insert("repository_backend".into(), "oss".into());
        config.as_table_mut().unwrap().insert(
            "backend".into(),
            toml::Value::try_from(json!({"oss": {
                "endpoint": oss.endpoint, "bucket": oss.bucket,
                "prefix": oss.prefix.as_deref().context("fixture prefix")?,
                "access_key_id": oss.access_key_id.as_deref().context("fixture key")?,
                "access_key_secret": oss.access_key_secret.as_deref().context("fixture secret")?,
                "region": "us-east-1", "cache_max_size_gb": 1,
            }}))?,
        );
    }
    if let Some(url) = extension {
        std::env::set_var("AENV_CUSTOM_EXTENSION_URL", url);
    }
    let path = root.join("config.toml");
    std::fs::write(&path, toml::to_string(&config)?)?;
    std::env::set_var("AENV_CONFIG_PATH", &path);
    let app = ConfigManager::new_from_path(&path)?.config().clone();
    agentenv::setup::ensure_environment(&app, None).await?;
    fixture().await
}

fn factory(app: &AppConfig, assets: &Value) -> Result<DragonballSandboxFactory> {
    let policy = DragonballRuntimePolicy::from_app_config(
        app,
        asset(assets, "runner_path")?,
        asset(assets, "kernel_path")?,
    )?
    .with_memory_mode(DragonballMemoryMode::SharedUblkIncremental);
    DragonballSandboxFactory::new(policy)
}

fn oss_manager(config: &OssBackendConfig, cache: PathBuf) -> Result<SnapshotManager> {
    let (repository, resolver) = OssBackend::new_with_publish_compression(
        config,
        cache,
        &SnapshotPublishCompressionConfig {
            enabled: false,
            ..Default::default()
        },
    )?
    .into_parts();
    Ok(SnapshotManager::from_parts(repository, resolver, None))
}

fn runtime_entries(app: &AppConfig) -> Result<HashSet<PathBuf>> {
    Ok(std::fs::read_dir(&app.runtime_path)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?)
}

fn owned_ram_devices(runner: &Path) -> Result<Vec<PathBuf>> {
    let mut devices = vec![];
    for entry in std::fs::read_dir("/proc")? {
        let path = entry?.path();
        if std::fs::read_link(path.join("exe")).ok().as_deref() != Some(runner) {
            continue;
        }
        let status = std::fs::read_to_string(path.join("status"))?;
        if !status
            .lines()
            .any(|l| l == format!("PPid:\t{}", std::process::id()))
        {
            continue;
        }
        let maps = std::fs::read_to_string(path.join("maps"))?;
        let ram = maps
            .lines()
            .filter_map(|l| {
                let f = l.split_whitespace().collect::<Vec<_>>();
                (f.len() >= 6 && f[1] == "rw-p" && f[5].starts_with("/dev/ublkb"))
                    .then(|| PathBuf::from(f[5]))
            })
            .collect::<HashSet<_>>();
        assert_eq!(ram.len(), 1, "restored guest must have private ublk RAM");
        devices.extend(ram);
    }
    Ok(devices)
}

#[tokio::test]
#[ignore = "real KVM/ublk and explicitly supplied local S3 server"]
async fn dragonball_platform_oss_capture_restore_preserves_identity() -> Result<()> {
    if subprocess_case("dragonball_platform_oss_capture_restore_preserves_identity").await? {
        return Ok(());
    }
    let root = tempfile::Builder::new().prefix("db-m3-oss-").tempdir()?;
    let mut s3 = LocalS3::start(root.path()).await?;
    let (app, assets) = configure(root.path(), Some(&s3.config), None).await?;
    let factory = factory(&app, &assets)?;
    let spec = spec(&assets)?;
    let initial = launch(&assets)?;
    let mut source = factory.build_fresh(spec.clone(), initial.clone())?;
    source.start().await?;
    assert_eq!(
        source
            .run_command(
                "/bin/sh",
                &[
                    "-ec",
                    "printf oss-disk >/var/tmp/m3-marker; printf oss-ram >/dev/shm/m3-marker; sync"
                ]
            )
            .await?
            .exit_code,
        0
    );
    let capture = source.snapshot().await?;
    let capture_root = capture
        .manifest()
        .vm_state
        .path
        .parent()
        .context("capture root")?
        .to_path_buf();
    let identity = capture.manifest().restore_identity.clone();
    let producer = oss_manager(&s3.config, root.path().join("producer"))?;
    let stored = agentenv::sandbox::publish_captured(
        &producer,
        publish_metadata(initial.sandbox_id, &spec, &assets, &capture)?,
        capture,
    )
    .await?;
    source.stop().await?;
    drop(source);
    drop(producer);
    assert!(
        !capture_root.exists(),
        "committed OSS snapshot retained source capture"
    );
    let cache = root.path().join("empty-consumer");
    assert!(!cache.exists());
    let consumer = oss_manager(&s3.config, cache)?;
    let runnable = consumer
        .load_runnable(stored.id.to_string())
        .await?
        .context("OSS runnable")?;
    assert_eq!(runnable.manifest().restore_identity, identity);
    let serialized = serde_json::to_string(runnable.record())?;
    assert!(!serialized.contains(s3.config.access_key_secret.as_ref().unwrap()));
    let mut children = vec![];
    for _ in 0..2 {
        let mut launch = launch(&assets)?;
        launch.snapshot_id = stored.id.to_string();
        let id = launch.sandbox_id;
        let token = launch.envd_access_token.clone();
        let mut child = factory.build_from_snapshot(&runnable, launch)?;
        child.start().await?;
        let result = fork_command(child.as_ref(), token.as_ref(), &format!("test \"$(cat /var/tmp/m3-marker)\" = oss-disk; test \"$(cat /dev/shm/m3-marker)\" = oss-ram; test \"$E2B_SANDBOX_ID\" = '{id}'")).await?;
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        children.push(child);
    }
    let devices = owned_ram_devices(&asset(&assets, "runner_path")?)?;
    assert_eq!(devices.len(), 2);
    assert_eq!(
        devices[0], devices[1],
        "siblings did not share immutable RAM"
    );
    for child in &mut children {
        child.stop().await?;
    }
    // A changed opaque ABI must be refused before runtime allocation. Only the
    // consumer cache is touched; the actual committed OSS record remains intact.
    let state_path = &runnable.manifest().vm_state.path;
    let original = std::fs::read(state_path)?;
    let mut invalid: Value = serde_json::from_slice(&original)?;
    invalid["identity"]["restore_abi_id"] = "unqualified-m3-abi".into();
    std::fs::write(state_path, serde_json::to_vec(&invalid)?)?;
    let before = runtime_entries(&app)?;
    assert!(factory
        .build_from_snapshot(&runnable, launch(&assets)?)
        .is_err());
    assert_eq!(before, runtime_entries(&app)?);
    std::fs::write(state_path, original)?;
    let third = oss_manager(&s3.config, root.path().join("another-empty-cache"))?;
    assert!(
        third.load_runnable(stored.id.to_string()).await?.is_some(),
        "ABI rejection removed durable catalog"
    );
    consumer.delete(stored.id.to_string()).await?;
    s3.stop().await?;
    Ok(())
}

#[derive(Default)]
struct HookState {
    requests: Mutex<Vec<(String, Value)>>,
}

fn full_params() -> Value {
    json!({"approved": [7, null, {"nested": false}], "replacement": true})
}

async fn hook(
    State(state): State<Arc<HookState>>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let path = uri.path().to_owned();
    state
        .requests
        .lock()
        .unwrap()
        .push((path.clone(), body.clone()));
    if path.ends_with("/stop")
        || body["patch"]["fail"] == true
        || body["customExtensionParams"]["failStart"] == true
    {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    let response = if path.ends_with("/patch-params") {
        json!({"customExtensionParams": full_params()})
    } else {
        json!({})
    };
    (StatusCode::OK, Json(response))
}

fn create_request(assets: &Value, params: Value) -> Result<CreateSandboxRequest> {
    let spec = spec(assets)?;
    let launch = launch(assets)?;
    Ok(CreateSandboxRequest {
        source: SandboxLaunchSource::Image {
            image_ref: launch.snapshot_id,
            overlaybd_config_path: spec.image_config_path,
            context: Box::new(spec.context),
            resources: Some(spec.resources),
            extra_drives: spec.extra_drives,
            extra_boot_args: None,
            image_configs: Box::new(serde_json::from_value(assets["image_configs"].clone())?),
        },
        extra_drives: vec![],
        extra_drives_in_snapshot: false,
        timeout: Some(Duration::from_secs(600)),
        timeout_action: SandboxTimeoutAction::Pause,
        auto_resume: false,
        user_metadata: None,
        env_vars: None,
        network_policy: launch.network.context("network policy")?,
        secure: true,
        custom_extension_params: Some(serde_json::from_value(params)?),
        volume_mounts: HashMap::new(),
    })
}

#[tokio::test]
#[ignore = "real KVM/ublk and extension HTTP lifecycle/patch contracts"]
async fn dragonball_platform_extension_patch_roundtrip() -> Result<()> {
    if subprocess_case("dragonball_platform_extension_patch_roundtrip").await? {
        return Ok(());
    }
    let root = tempfile::Builder::new()
        .prefix("db-m3-extension-")
        .tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let hooks = Arc::new(HookState::default());
    let router = Router::new().fallback(hook).with_state(hooks.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let (app, assets) = configure(root.path(), None, Some(&url)).await?;
    let factory = factory(&app, &assets)?;
    let orchestrator = Orchestrator::new(
        InMemoryMetadataStore::new(),
        factory.clone(),
        FileBackedSandboxPersister::new(root.path().join("paused"), VirtualizationMode::Kvm),
    )
    .await?;
    assert!(orchestrator
        .create_sandbox(create_request(&assets, json!({"failStart":true}))?)
        .await
        .is_err());
    let initial = json!({"original": [1,2], "obsolete": true});
    let metadata = orchestrator
        .create_sandbox(create_request(&assets, initial)?)
        .await?;
    let id = metadata.id;
    let patch = json!({"operations": [{"set": [null, {"key":"value"}]}], "obsolete": null});
    let updated = orchestrator
        .patch_sandbox_custom_extension_params(id, serde_json::from_value(patch.clone())?)
        .await?;
    assert_eq!(serde_json::to_value(&updated)?, full_params());
    assert!(orchestrator
        .patch_sandbox_custom_extension_params(id, serde_json::from_value(json!({"fail":true}))?)
        .await
        .is_err());
    assert_eq!(
        orchestrator
            .get_sandbox(&id)
            .await?
            .context("metadata")?
            .custom_extension_params,
        updated
    );
    let captured = orchestrator.capture_snapshot(id).await?;
    assert_eq!(captured.metadata.custom_extension_params, updated);
    let manager = super::dragonball_support::posix_manager(&root.path().join("repository"))?;
    let mut publication =
        publish_metadata(id, &spec(&assets)?, &assets, &captured.captured_snapshot)?;
    publication.custom_extension_params = captured.metadata.custom_extension_params;
    let record =
        agentenv::sandbox::publish_captured(&manager, publication, captured.captured_snapshot)
            .await?;
    let runnable = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("extension snapshot")?;
    assert_eq!(runnable.committed().custom_extension_params, updated);
    let mut restored = factory.build_from_snapshot(&runnable, launch(&assets)?)?;
    restored.start().await?;
    restored.stop().await?;
    orchestrator.pause_sandbox(id).await?;
    orchestrator
        .resume_sandbox(id, NewTimeout::UseExisting)
        .await?;
    assert_eq!(
        orchestrator
            .get_sandbox(&id)
            .await?
            .context("resumed metadata")?
            .custom_extension_params,
        updated
    );
    for child in orchestrator
        .fork_sandbox(id, 1, NewTimeout::UseExisting)
        .await?
    {
        let child = child?;
        assert_eq!(child.custom_extension_params, updated);
        orchestrator.delete_sandbox(child.id).await?;
    }
    orchestrator.delete_sandbox(id).await?;
    orchestrator.shutdown().await?;
    let requests = hooks.requests.lock().unwrap().clone();
    let patches = requests
        .iter()
        .filter(|(p, _)| p.ends_with("/patch-params"))
        .collect::<Vec<_>>();
    assert_eq!(patches.len(), 2);
    assert_eq!(
        patches[0].1["patch"], patch,
        "opaque patch was merged or transformed"
    );
    let starts = requests
        .iter()
        .filter(|(p, _)| p.ends_with("/start-fresh") || p.ends_with("/start-resume"))
        .collect::<Vec<_>>();
    assert_eq!(
        starts.len(),
        5,
        "failed fresh, fresh, snapshot, resume, fork"
    );
    let mut instances = HashSet::new();
    for (path, start) in starts {
        let instance = start["sandboxInstanceId"]
            .as_str()
            .context("instance identity")?;
        assert!(instances.insert(instance.to_owned()));
        let stops = requests
            .iter()
            .filter(|(p, b)| {
                p.ends_with("/stop")
                    && b["sandboxInstanceId"] == instance
                    && b["sandboxId"] == start["sandboxId"]
            })
            .count();
        assert_eq!(stops, 1, "runtime start/stop not paired despite stop 503");
        if path.ends_with("/start-resume") {
            assert_eq!(start["customExtensionParams"], full_params());
        }
    }
    server.abort();
    Ok(())
}
