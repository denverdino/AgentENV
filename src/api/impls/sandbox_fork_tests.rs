use super::*;
use crate::snapshot::SnapshotVolume;
use std::sync::Arc;
use std::{
    future::{poll_fn, Future},
    task::Poll,
};

async fn fixture() -> anyhow::Result<(tempfile::TempDir, ApiImpl, SandboxMetadata)> {
    use crate::{
        api_key::ApiKey,
        cfg::AppConfig,
        image::ImageResolver,
        orchestrator::{FileBackedSandboxPersister, InMemoryMetadataStore, Orchestrator},
        snapshot::{
            repository::backends::{PosixFsBackend, PosixFsBackendConfig},
            SnapshotManager,
        },
        template::TemplateBuilder,
    };
    let root = tempfile::tempdir()?;
    let backend = PosixFsBackend::new(PosixFsBackendConfig {
        root: root.path().join("repository"),
        cache_root: Some(root.path().join("cache")),
        runtime_cache_root: None,
    })?;
    let snapshots = Arc::new(SnapshotManager::from_parts(
        backend.repository(),
        backend.runtime_resolver(),
        None,
    ));
    let orchestrator = Orchestrator::new(
        InMemoryMetadataStore::new(),
        crate::sandbox::selected::test_factory(),
        FileBackedSandboxPersister::new_for_test(root.path().join("paused")),
    )
    .await?;
    let volumes = Arc::new(
        VolumeManager::open_with_repository(root.path().join("volumes"), backend.repository())
            .await?,
    );
    let api = ApiImpl::new(
        orchestrator,
        snapshots,
        Arc::new(TemplateBuilder::new()),
        Arc::new(ImageResolver::new(&AppConfig::default())),
        volumes,
        None,
        Vec::new(),
        ApiKey::new("fork-volume-test-api-key-0123456789")?,
    );
    // Real POSIX publication/reservation and filesystem copies; the layer's
    // contents are never opened by a VMM in this ownership-only regression.
    let layer = root.path().join("base.commit");
    std::fs::write(&layer, b"immutable ownership fixture")?;
    let config = root.path().join("source.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&overlaybd::config::ImageConfig {
            lowers: vec![overlaybd::config::LayerConfig {
                file: layer.to_string_lossy().into_owned(),
                ..Default::default()
            }],
            ..Default::default()
        })?,
    )?;
    let mut source = SandboxMetadata::default();
    for (name, mount, mode) in [
        ("exclusive", "/data", VolumeMode::Exclusive),
        ("shared", "/shared", VolumeMode::ReadOnly),
    ] {
        let record = api
            .volume_manager
            .create(name.into(), mode, None, Some(config.clone()), 64)
            .await?;
        api.volume_manager
            .reserve(&record.id, &source.id.to_string())
            .await?;
        source.volume_mounts.insert(mount.into(), record.id);
    }
    Ok((root, api, source))
}

#[tokio::test]
async fn incompatible_snapshot_does_not_create_captured_volumes() -> anyhow::Result<()> {
    incompatible_snapshot_before_volumes(false).await
}

#[tokio::test]
async fn incompatible_snapshot_does_not_reserve_requested_volumes() -> anyhow::Result<()> {
    incompatible_snapshot_before_volumes(true).await
}

async fn incompatible_snapshot_before_volumes(requested: bool) -> anyhow::Result<()> {
    let (root, api, source) = fixture().await?;
    let (_, _, mut manifest) = crate::snapshot::mock::write_mock_built_artifacts(
        &root.path().join("incompatible-artifacts"),
    )?;
    manifest.backend = "future-incompatible-vmm".into();
    let volume = api
        .volume_manager
        .get(&source.volume_mounts["/shared"])
        .await?;
    let mut metadata = SnapshotPublishMetadata::mock();
    metadata.volume_snapshots.push(SnapshotVolume {
        mount_path: "/shared".into(),
        mode: volume.mode,
        size_mb: volume.size_mb,
        layers: volume.backing_layers.clone(),
    });
    let record = api
        .snapshot_manager
        .publish(metadata, manifest, None)
        .await?;
    let mut body = models::NewSandbox::new(record.id.to_string());
    if requested {
        body.volume_mounts = Some(vec![models::SandboxVolumeMount::new(
            volume.name.clone(),
            "/shared".into(),
        )]);
    }
    let host = Host::from(http::uri::Authority::from_static("localhost"));
    let cookies = CookieJar::new();
    let claims = super::super::Claims;
    let original = api.volume_manager.list_page(None, 200).await?.records;
    let mut request = Box::pin(api.sandboxes_post(&Method::POST, &host, &cookies, &claims, &body));
    // Observe every async boundary, not just the final catalog after rollback.
    // A transient reservation/allocation is already a failure: cancellation at
    // that boundary must not leave artifacts for an incompatible snapshot.
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let progress = poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
            let volumes = api.volume_manager.list_page(None, 200).await?.records;
            assert_eq!(
                volumes.len(),
                original.len(),
                "incompatible restore created a volume"
            );
            for before in &original {
                let after = volumes.iter().find(|v| v.id == before.id).unwrap();
                assert_eq!(after.reserved_by_sandbox_id, before.reserved_by_sandbox_id);
                assert_eq!(
                    after.read_only_mounts, before.read_only_mounts,
                    "incompatible restore reserved a volume"
                );
            }
            if let Poll::Ready(result) = progress {
                return result.map_err(|()| anyhow::anyhow!("API transport error"));
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert!(
        matches!(response, SandboxesPostResponse::Status400_BadRequest(_)),
        "incompatible restore should fail preflight: {response:?}"
    );
    assert!(
        api.snapshot_manager
            .repository()
            .get(&record.id.to_string())
            .await?
            .is_some(),
        "incompatible snapshot catalog was deleted"
    );
    Ok(())
}

#[tokio::test]
async fn failed_fork_releases_shared_ro_without_deleting_source_or_sibling() -> anyhow::Result<()> {
    let (_root, api, source) = fixture().await?;
    let (specs, allocations) = prepare_volume_fork_specs(&api, &source, 2)
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let failed = specs[0].sandbox_id;
    let successful = specs[1].sandbox_id;
    let failed_exclusive = specs[0].volume_mounts["/data"].clone();
    let successful_exclusive = specs[1].volume_mounts["/data"].clone();
    let failed_allocations = allocations
        .into_iter()
        .filter(|(id, _)| *id == failed)
        .collect();
    cleanup_fork_volume_children(&api.volume_manager, &failed_allocations).await;
    let shared = api
        .volume_manager
        .get(&source.volume_mounts["/shared"])
        .await?;
    let mut owners = shared.read_only_mounts.clone();
    owners.sort();
    let mut expected = vec![source.id.to_string(), successful.to_string()];
    expected.sort();
    assert_eq!(
        owners, expected,
        "failed child retained a shared RO reservation"
    );
    assert!(api.volume_manager.get(&failed_exclusive).await.is_err());
    assert_eq!(
        api.volume_manager
            .get(&successful_exclusive)
            .await?
            .reserved_by_sandbox_id,
        Some(successful.to_string())
    );
    assert_eq!(
        api.volume_manager
            .get(&source.volume_mounts["/data"])
            .await?
            .reserved_by_sandbox_id,
        Some(source.id.to_string())
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_fork_finishes_rollback_after_partial_volume_allocation() -> anyhow::Result<()> {
    cancelled_fork_rolls_back(false).await
}

#[tokio::test]
async fn cancelled_failed_fork_finishes_in_progress_volume_rollback() -> anyhow::Result<()> {
    cancelled_fork_rolls_back(true).await
}

async fn cancelled_fork_rolls_back(during_rollback: bool) -> anyhow::Result<()> {
    let (_root, api, source) = fixture().await?;
    let original: HashSet<_> = source.volume_mounts.values().cloned().collect();
    // No runtime is registered, so the eventual fork fails with NotFound.
    // Cancel only after observing a real new catalog record and reservation.
    let mut request =
        Box::pin(api.fork_with_volume_specs(source.clone(), 100, NewTimeout::UseExisting));
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut peak_children = 0;
        loop {
            let progress = poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
            assert!(
                progress.is_pending(),
                "fork completed before cancellation boundary"
            );
            let records = api.volume_manager.list_page(None, 200).await?.records;
            let children = records
                .iter()
                .filter(|record| !original.contains(&record.id))
                .count();
            peak_children = peak_children.max(children);
            let reached = if during_rollback {
                // A falling catalog count proves fork failed and rollback has
                // started, while remaining children still need their owner.
                children > 0 && children < peak_children
            } else {
                records.iter().any(|record| {
                    !original.contains(&record.id) && record.reserved_by_sandbox_id.is_some()
                })
            };
            if reached {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    drop(request);
    let rollback = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let records = api.volume_manager.list_page(None, 200).await?.records;
            let shared = api
                .volume_manager
                .get(&source.volume_mounts["/shared"])
                .await?;
            if records.iter().all(|record| original.contains(&record.id))
                && shared.read_only_mounts == vec![source.id.to_string()]
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    assert!(
        rollback.is_ok(),
        "cancelled API request orphaned child volume allocations"
    );
    rollback??;
    assert_eq!(
        api.volume_manager
            .get(&source.volume_mounts["/data"])
            .await?
            .reserved_by_sandbox_id,
        Some(source.id.to_string())
    );
    Ok(())
}
