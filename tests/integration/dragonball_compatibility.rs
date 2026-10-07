//! Opt-in persisted-identity compatibility validation; excluded from daily suites.
use super::dragonball_support::{asset, fixture, launch, posix_manager, publish_metadata, spec};
use agentenv::sandbox::{
    dragonball::{DragonballRuntimePolicy, DragonballSandboxFactory},
    SandboxBackend, SandboxBackendFactory,
};
use anyhow::{Context, Result};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "real KVM/ublk published and paused snapshots reject the previous Kata pin without leases"]
async fn dragonball_compatibility_previous_kata_snapshots_rejected_without_leases() -> Result<()> {
    use agentenv::snapshot::SNAPSHOT_ARTIFACT_LAYOUT;
    use agentenv_vmm_protocol::runtime::RuntimeSnapshotEnvelope;

    let (mut app, assets) = fixture().await?;
    let root = tempfile::tempdir()?;
    app.runtime_path = root.path().join("runtime");
    let factory = DragonballSandboxFactory::new(DragonballRuntimePolicy::from_app_config(
        &app,
        asset(&assets, "runner_path")?,
        asset(&assets, "kernel_path")?,
    )?)?;
    let spec = spec(&assets)?;
    let launch = launch(&assets)?;
    let mut source = factory.build_fresh(spec.clone(), launch.clone())?;
    source.start().await?;
    let captured = source.snapshot().await?;
    let manager = posix_manager(root.path())?;
    let mut record = manager
        .publish_captured(
            publish_metadata(launch.sandbox_id, &spec, &assets, &captured)?,
            captured,
        )
        .await?;
    let pause_root = root.path().join("paused");
    let paused = source.pause(Some(&pause_root)).await?;
    let mut paused_record = paused.encode()?;
    paused.persistence_committed();
    source.stop().await?;
    drop(source);

    // Both genuine captures pass current-reader preflight before changing only
    // the revision and revision-qualified ABI. This is an identity regression,
    // not a claim that an old binary produced these guest-state bytes.
    let current = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("published snapshot")?;
    drop(factory.build_from_snapshot(&current, launch.clone())?);
    drop(factory.decode_paused_state(pause_root.clone(), paused_record.clone())?);

    let runtime_entries = || -> std::io::Result<std::collections::HashSet<PathBuf>> {
        std::fs::read_dir(&app.runtime_path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    };
    let before = runtime_entries()?;
    let previous = "c77812d669737c11cb16321827acc1c79bf065f3";
    let mut old_identity = current
        .manifest()
        .restore_identity
        .clone()
        .context("published restore identity")?;
    old_identity.dragonball_revision = previous.into();
    old_identity.restore_abi_id = format!("dragonball-held-cow-v2@{previous}");
    let committed = record.committed.as_mut().context("committed snapshot")?;
    committed.runtime_versions.vmm_version = Some(previous.into());
    committed.runtime_versions.restore_identity = Some(old_identity.clone());
    let record_path = root
        .path()
        .join("repository/catalog/records")
        .join(format!("{}.json", record.id));
    let manifest_path = root
        .path()
        .join("repository/snapshots")
        .join(record.id.to_string())
        .join(SNAPSHOT_ARTIFACT_LAYOUT.firecracker_manifest);
    let mut manifest: agentenv::sandbox::SandboxSnapshotManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    manifest.restore_identity = Some(old_identity.clone());
    let state_path = current.manifest().vm_state.path.clone();
    let mut envelope: RuntimeSnapshotEnvelope =
        serde_json::from_slice(&std::fs::read(&state_path)?)?;
    envelope.identity = old_identity.clone();
    let published_files = [
        (record_path, serde_json::to_vec(&record)?),
        (manifest_path, serde_json::to_vec(&manifest)?),
        (state_path, serde_json::to_vec(&envelope)?),
    ];
    drop(current);
    for (path, bytes) in &published_files {
        std::fs::write(path, bytes)?;
    }
    let old = manager
        .load_runnable(record.id.to_string())
        .await?
        .context("previous-pin record must remain loadable")?;
    assert_eq!(
        old.manifest().restore_identity.as_ref(),
        Some(&old_identity)
    );
    let published_result = factory.build_from_snapshot(&old, launch);
    let published_rejected = published_result.is_err();
    if let Err(error) = &published_result {
        assert!(format!("{error:#}").contains("unsupported or incomplete restore identity"));
    }
    drop(published_result);
    assert_eq!(
        runtime_entries()?,
        before,
        "published preflight allocated runtime leases"
    );
    for (path, bytes) in &published_files {
        assert_eq!(
            std::fs::read(path)?,
            *bytes,
            "published rejection changed {path:?}"
        );
    }

    let paused_state_path = PathBuf::from(
        paused_record["vm_state_path"]
            .as_str()
            .context("paused state path")?,
    );
    let mut paused_envelope: RuntimeSnapshotEnvelope =
        serde_json::from_slice(&std::fs::read(&paused_state_path)?)?;
    paused_envelope.identity = old_identity.clone();
    paused_record["manifest"]["restoreIdentity"] = serde_json::to_value(&old_identity)?;
    let paused_record_path = pause_root.join("paused-record.json");
    let paused_files = [
        (paused_state_path, serde_json::to_vec(&paused_envelope)?),
        (
            paused_record_path.clone(),
            serde_json::to_vec(&paused_record)?,
        ),
    ];
    for (path, bytes) in &paused_files {
        std::fs::write(path, bytes)?;
    }
    let paused_result = factory.decode_paused_state(
        pause_root,
        serde_json::from_slice(&std::fs::read(paused_record_path)?)?,
    );
    let paused_rejected = paused_result.is_err();
    if let Err(error) = &paused_result {
        assert!(format!("{error:#}").contains("unsupported or incomplete restore identity"));
    }
    drop(paused_result);
    assert_eq!(
        runtime_entries()?,
        before,
        "paused preflight allocated runtime leases"
    );
    for (path, bytes) in &paused_files {
        assert_eq!(
            std::fs::read(path)?,
            *bytes,
            "paused rejection changed {path:?}"
        );
    }
    assert!(manager.get(record.id.to_string()).await?.is_some());
    drop(old);
    drop(paused);
    manager.delete(record.id.to_string()).await?;
    assert!(
        published_rejected,
        "previous-pin published snapshot was accepted"
    );
    assert!(paused_rejected, "previous-pin paused snapshot was accepted");
    Ok(())
}
