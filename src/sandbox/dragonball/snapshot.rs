use agentenv_vmm_protocol::runtime::RuntimeSnapshotEnvelope;
use anyhow::{ensure, Result};
use std::{
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path},
};

pub fn read_envelope(path: &Path) -> Result<RuntimeSnapshotEnvelope> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
        "state path must be absolute and nonescaping"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "M1 state must be a regular file");
    ensure!(metadata.len() <= 16 * 1024 * 1024, "state exceeds 16 MiB");
    let mut bytes = vec![];
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024 * 1024, "state exceeds 16 MiB");
    let envelope: RuntimeSnapshotEnvelope = serde_json::from_slice(&bytes)?;
    envelope
        .validate_state_topology()
        .map_err(anyhow::Error::msg)?;
    Ok(envelope)
}

pub fn read_for_restore(
    reader: &super::super::restore_identity::RestoreReader,
    record: &crate::snapshot::SnapshotRecord,
    manifest: &crate::sandbox::SandboxSnapshotManifest,
) -> Result<RuntimeSnapshotEnvelope> {
    let envelope = read_envelope(&manifest.vm_state.path)?;
    super::super::restore_identity::validate_artifact_identity(
        reader, record, manifest, &envelope,
    )?;
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn restore_identity_envelope_reader_refuses_symlink_before_decode() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        source.write_all(b"{}").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(source.path(), &link).unwrap();
        let error = read_envelope(&link).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(|error| error.raw_os_error()),
            Some(libc::ELOOP)
        );
    }

    #[test]
    fn restore_identity_envelope_reader_bounds_state_before_decode() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(b"{}").unwrap();
        source.as_file().set_len(16 * 1024 * 1024 + 1).unwrap();
        let error = read_envelope(source.path()).unwrap_err();
        assert!(
            error.to_string().contains("16 MiB"),
            "wrong rejection: {error}"
        );
    }

    #[test]
    fn restore_identity_envelope_reader_refuses_parent_escape() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        let mut source = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        source.write_all(b"{}").unwrap();
        let path = directory
            .path()
            .join("nested/..")
            .join(source.path().file_name().unwrap());
        let error = read_envelope(&path).unwrap_err();
        assert!(
            error.to_string().contains("nonescaping"),
            "wrong rejection: {error}"
        );
    }
}
