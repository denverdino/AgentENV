use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use overlaybd::backend::local::LocalFile;
use overlaybd::index::{Segment, SegmentMapping};
use overlaybd::index_file::compact_to;
use overlaybd::virtual_file::VirtualFile;

use super::ublk::{create_commit_args, OverlaybdCompactOutput};

#[derive(Clone, Copy, Debug)]
pub(crate) struct MemorySegment {
    pub image_offset: u64,
    pub source_offset: u64,
    pub length: u64,
}

#[cfg(any(feature = "hypervisor-firecracker", test))]
pub(crate) async fn write_memory_overlaybd(
    source: Arc<dyn VirtualFile>,
    segments: &[MemorySegment],
    virtual_size: u64,
    at: &Path,
    mode: OverlaybdCompactOutput,
) -> Result<PathBuf> {
    let mappings = segment_mappings(segments, virtual_size)?;
    tokio::fs::create_dir_all(at).await?;
    let output = at.join("overlaybd.commit");
    publish_memory_overlaybd_layer(&[source], &mappings, virtual_size, &output, mode, 32).await?;
    Ok(output)
}

/// Detect zeros while copying Dragonball RAM once, without staging another RAM
/// file. Explicit zero records mask ancestral data in incremental layers too.
#[cfg(feature = "hypervisor-dragonball")]
pub(crate) async fn write_sparse_memory_overlaybd(
    source: Arc<dyn VirtualFile>,
    segments: &[MemorySegment],
    virtual_size: u64,
    at: &Path,
    mode: OverlaybdCompactOutput,
) -> Result<PathBuf> {
    let mappings = segment_mappings(segments, virtual_size)?;
    tokio::fs::create_dir_all(at).await?;
    let output = at.join("overlaybd.commit");
    publish_memory_layer(&[source], &mappings, virtual_size, &output, mode, 1, true).await?;
    Ok(output)
}

fn segment_mappings(segments: &[MemorySegment], virtual_size: u64) -> Result<Vec<SegmentMapping>> {
    ensure!(
        virtual_size.is_multiple_of(4096),
        "memory virtual size must be page aligned"
    );
    let mut mappings = Vec::new();
    for segment in segments {
        ensure!(
            segment.length > 0
                && segment.length % 4096 == 0
                && segment.image_offset % 4096 == 0
                && segment.source_offset % 4096 == 0,
            "memory segment must be nonempty and page aligned"
        );
        ensure!(
            segment
                .image_offset
                .checked_add(segment.length)
                .is_some_and(|end| end <= virtual_size),
            "memory segment exceeds virtual size"
        );
        segment
            .source_offset
            .checked_add(segment.length)
            .context("memory source offset overflow")?;
        let mut remaining = segment.length / 512;
        let mut image = segment.image_offset / 512;
        let mut source = segment.source_offset / 512;
        while remaining > 0 {
            let length = remaining.min(u64::from(Segment::MAX_LENGTH)) as u32;
            mappings.push(SegmentMapping::new(image, length, source, false, 0));
            image += u64::from(length);
            source += u64::from(length);
            remaining -= u64::from(length);
        }
    }
    mappings.sort_unstable();
    for pair in mappings.windows(2) {
        ensure!(
            pair[0].offset() + u64::from(pair[0].length()) <= pair[1].offset(),
            "overlapping memory image segments"
        );
    }
    Ok(mappings)
}

#[cfg(any(feature = "hypervisor-firecracker", test))]
pub(crate) async fn publish_memory_overlaybd_layer(
    src_layers: &[Arc<dyn VirtualFile>],
    mappings: &[SegmentMapping],
    virtual_size: u64,
    output_path: &Path,
    mode: OverlaybdCompactOutput,
    concurrency: usize,
) -> Result<()> {
    publish_memory_layer(
        src_layers,
        mappings,
        virtual_size,
        output_path,
        mode,
        concurrency,
        false,
    )
    .await
}

async fn publish_memory_layer(
    src_layers: &[Arc<dyn VirtualFile>],
    mappings: &[SegmentMapping],
    virtual_size: u64,
    output_path: &Path,
    mode: OverlaybdCompactOutput,
    concurrency: usize,
    detect_zeros: bool,
) -> Result<()> {
    let parent = output_path
        .parent()
        .context("memory output has no parent")?;
    // A unique, owned temporary file is removed even if the future is dropped.
    // Adoption must not overwrite an existing durable artifact.
    let staged = tempfile::Builder::new()
        .prefix(".memory-")
        .suffix(".commit.tmp")
        .tempfile_in(parent)?;
    let output_file: Arc<dyn VirtualFile> =
        Arc::new(LocalFile::builder().create(false).open(staged.path())?);
    let commit_args = create_commit_args(output_file.clone(), mode, concurrency).await?;
    let started = std::time::Instant::now();
    if detect_zeros {
        overlaybd::index_file::compact_to_sparse(src_layers, mappings, virtual_size, commit_args)
            .await
    } else {
        compact_to(src_layers, mappings, virtual_size, commit_args).await
    }
    .context("compact memory layer")?;
    let compact_us = started.elapsed().as_micros() as u64;
    let sync_started = std::time::Instant::now();
    output_file
        .sync()
        .await
        .context("sync sealed memory layer")?;
    let sync_us = sync_started.elapsed().as_micros() as u64;
    tokio::fs::hard_link(staged.path(), output_path)
        .await
        .context("adopt sealed memory layer without overwrite")?;
    if detect_zeros {
        tracing::debug!(
            compact_us,
            sync_us,
            total_us = started.elapsed().as_micros() as u64,
            "sparse memory publication timing"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::ensure;
    use async_trait::async_trait;
    use bytes::Bytes;
    use overlaybd::backend::local::LocalFile;

    struct Source {
        data: Vec<u8>,
        short: bool,
        fail: bool,
    }

    #[async_trait]
    impl VirtualFile for Source {
        async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
            if self.fail {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
            }
            let start = usize::try_from(offset)?;
            ensure!(start <= self.data.len(), "out of range");
            let end = start.saturating_add(len).min(self.data.len());
            let end = if self.short && end > start {
                end - 1
            } else {
                end
            };
            Ok(Bytes::copy_from_slice(&self.data[start..end]))
        }
        async fn write_at(&self, _: u64, _: &[u8]) -> Result<usize> {
            anyhow::bail!("read-only")
        }
        async fn size(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
    }

    fn source(size: usize) -> Arc<dyn VirtualFile> {
        Arc::new(Source {
            data: vec![0x7b; size],
            short: false,
            fail: false,
        })
    }

    #[tokio::test]
    async fn full_memory_layer_matches_packed_ram_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // GPA holes do not occupy packed source/image offsets. Include a zero
        // region over a nonzero ancestor to detect missing full-RAM coverage.
        let mut data = vec![0x9a; 8192];
        data[4096..].fill(0);
        let input: Arc<dyn VirtualFile> = Arc::new(Source {
            data: data.clone(),
            short: false,
            fail: false,
        });
        let segments = [
            MemorySegment {
                image_offset: 0,
                source_offset: 0,
                length: 4096,
            },
            MemorySegment {
                image_offset: 4096,
                source_offset: 4096,
                length: 4096,
            },
        ];
        let path = write_memory_overlaybd(
            input,
            &segments,
            8192,
            dir.path(),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap();
        let file: Arc<dyn VirtualFile> = Arc::new(
            LocalFile::builder()
                .write(false)
                .create(false)
                .open(&path)
                .unwrap(),
        );
        let reader = overlaybd::index_file::open_file_ro(file).await.unwrap();
        assert_eq!(reader.size().await.unwrap(), 8192);
        assert_eq!(
            reader.read_at(0, 8192).await.unwrap().as_ref(),
            data.as_slice()
        );
        let ancestor_path = write_memory_overlaybd(
            source(8192),
            &segments,
            8192,
            &dir.path().join("ancestor"),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap();
        let ancestor: Arc<dyn VirtualFile> = Arc::new(
            LocalFile::builder()
                .write(false)
                .create(false)
                .open(ancestor_path)
                .unwrap(),
        );
        let top: Arc<dyn VirtualFile> = Arc::new(
            LocalFile::builder()
                .write(false)
                .create(false)
                .open(&path)
                .unwrap(),
        );
        let stack = overlaybd::index_file::open_files_ro(&[ancestor, top])
            .await
            .unwrap();
        assert_eq!(
            stack.read_at(0, 8192).await.unwrap().as_ref(),
            data.as_slice()
        );
    }

    #[tokio::test]
    async fn full_memory_layer_rejects_bad_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            vec![MemorySegment {
                image_offset: 1,
                source_offset: 0,
                length: 4096,
            }],
            vec![MemorySegment {
                image_offset: 0,
                source_offset: 1,
                length: 4096,
            }],
            vec![MemorySegment {
                image_offset: 0,
                source_offset: 0,
                length: 4095,
            }],
            vec![MemorySegment {
                image_offset: 0,
                source_offset: 0,
                length: 0,
            }],
            vec![MemorySegment {
                image_offset: 8192,
                source_offset: 0,
                length: 4096,
            }],
            vec![
                MemorySegment {
                    image_offset: 0,
                    source_offset: 0,
                    length: 8192,
                },
                MemorySegment {
                    image_offset: 4096,
                    source_offset: 0,
                    length: 4096,
                },
            ],
            vec![MemorySegment {
                image_offset: 0,
                source_offset: u64::MAX - 4095,
                length: 4096,
            }],
        ];
        for (i, segments) in cases.iter().enumerate() {
            assert!(
                write_memory_overlaybd(
                    source(8192),
                    segments,
                    8192,
                    &dir.path().join(i.to_string()),
                    OverlaybdCompactOutput::Raw
                )
                .await
                .is_err(),
                "bad layout {i} accepted"
            );
        }
    }

    #[tokio::test]
    async fn memory_snapshot_short_read_and_io_failure_preserve_final() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("overlaybd.commit");
        std::fs::write(&output, b"durable sentinel").unwrap();
        let segments = [MemorySegment {
            image_offset: 0,
            source_offset: 0,
            length: 4096,
        }];
        for (short, fail) in [(true, false), (false, true)] {
            let input: Arc<dyn VirtualFile> = Arc::new(Source {
                data: vec![0x7b; 4096],
                short,
                fail,
            });
            let error = write_memory_overlaybd(
                input,
                &segments,
                4096,
                dir.path(),
                OverlaybdCompactOutput::Raw,
            )
            .await
            .unwrap_err();
            if short {
                assert!(
                    format!("{error:#}").contains("got 4095"),
                    "expected short read before adoption: {error:#}"
                );
            } else {
                assert!(
                    error.chain().any(|e| e
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(libc::ENOSPC))),
                    "expected input IO failure before adoption: {error:#}"
                );
            }
            assert_eq!(std::fs::read(&output).unwrap(), b"durable sentinel");
            assert!(!output.with_extension("commit.tmp").exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn memory_snapshot_success_cannot_overwrite_durable_final() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("overlaybd.commit");
        std::fs::write(&output, b"durable sentinel").unwrap();
        let segments = [MemorySegment {
            image_offset: 0,
            source_offset: 0,
            length: 4096,
        }];
        assert!(write_memory_overlaybd(
            source(4096),
            &segments,
            4096,
            dir.path(),
            OverlaybdCompactOutput::Raw
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"durable sentinel");
    }

    #[tokio::test]
    #[ignore = "requires mount namespace and tmpfs via capability runner"]
    async fn full_memory_layer_reports_enospc() {
        if std::env::var_os("AENV_MEMORY_SNAPSHOT_ENOSPC_DIR").is_none() {
            // Aggregate privileged suites must prepare their own bounded output
            // filesystem; a subprocess keeps the mount namespace off test threads.
            let at = tempfile::tempdir().unwrap();
            let output = std::process::Command::new("unshare")
                .args(["--mount", "--propagation", "private", "sh", "-ec"])
                .arg("mount -t tmpfs -o size=64k,mode=700 tmpfs \"$2\"; export AENV_MEMORY_SNAPSHOT_ENOSPC_DIR=\"$2\"; exec \"$1\" sandbox::memory_snapshot::tests::full_memory_layer_reports_enospc --exact --ignored --test-threads=1")
                .arg("enospc-fixture")
                .arg(std::env::current_exe().unwrap())
                .arg(at.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "ENOSPC child failed: {output:?}");
            return;
        }
        let at = PathBuf::from(
            std::env::var_os("AENV_MEMORY_SNAPSHOT_ENOSPC_DIR").expect("isolated tmpfs path"),
        );
        assert!(at.is_absolute() && at.is_dir());
        let segments = [MemorySegment {
            image_offset: 0,
            source_offset: 0,
            length: 1024 * 1024,
        }];
        let error = write_memory_overlaybd(
            source(1024 * 1024),
            &segments,
            1024 * 1024,
            &at,
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap_err();
        assert!(
            error.chain().any(|e| e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(libc::ENOSPC))),
            "expected actual output ENOSPC: {error:#}"
        );
        assert!(!at.join("overlaybd.commit").exists());
        assert_eq!(
            std::fs::read_dir(at).unwrap().count(),
            0,
            "partial lower leaked"
        );
    }
}
