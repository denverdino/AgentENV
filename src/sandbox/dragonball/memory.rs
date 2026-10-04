use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agentenv_vmm_protocol::{runtime::MemoryLayout, runtime_memory::FrozenMemoryMap};
use anyhow::{ensure, Result};
use overlaybd::virtual_file::VirtualFile;

use super::capture::DragonballCaptureArtifacts;
use crate::sandbox::memory_snapshot::{write_memory_overlaybd, MemorySegment};
use crate::sandbox::storage_snapshot::build_mem_snapshot_image_config;
use crate::sandbox::ublk::{
    OverlaybdCompactOutput, SharedReadOnlyDevice, UblkCreateSpec, UblkDeviceManager,
};

pub(super) async fn write_incremental_memory(
    map: &FrozenMemoryMap,
    reader: Arc<dyn VirtualFile>,
    base: Option<&DragonballMemoryBase>,
    output: &Path,
    mode: OverlaybdCompactOutput,
) -> Result<PathBuf> {
    if let Some(base) = base {
        ensure!(
            map.layout_id == base.artifacts.envelope.identity.memory_layout_id,
            "incremental memory layout identity conflict"
        );
    }
    let started = std::time::Instant::now();
    let segments = capture_memory_segments(
        map,
        base.map(|base| &base.artifacts.envelope.memory_layout),
        reader.as_ref(),
    )
    .await?;
    let scan_ms = started.elapsed().as_millis();
    let layout = map.validate().map_err(anyhow::Error::msg)?;
    let payload_bytes: u64 = segments.iter().map(|segment| segment.length).sum();
    let write_started = std::time::Instant::now();
    let layer = write_memory_overlaybd(reader, &segments, layout.image_size, output, mode).await?;
    let write_ms = write_started.elapsed().as_millis();
    let config = build_mem_snapshot_image_config(
        base.map(|base| base.artifacts.manifest.memory.image_config_path.as_path()),
        &layer,
        output,
        mode,
    )
    .await?;
    let path = output.join("memory-image.json");
    super::capture::write_new_json(&path, &config)?;
    tracing::info!(
        incremental = base.is_some(),
        memory_bytes = layout.image_size,
        payload_bytes,
        scan_ms,
        write_ms,
        total_ms = started.elapsed().as_millis(),
        "Dragonball memory capture completed"
    );
    Ok(path)
}

async fn capture_memory_segments(
    map: &FrozenMemoryMap,
    base: Option<&MemoryLayout>,
    reader: &dyn VirtualFile,
) -> Result<Vec<MemorySegment>> {
    let segments = memory_segments(map, base)?;
    // Only a standalone full layer has an implicit zero base. In a delta,
    // explicit zero pages must continue masking nonzero ancestral content.
    if base.is_some() {
        return Ok(segments);
    }
    // The caller retains a frozen read lease through BOTH this bounded scan
    // and the writer's reread. Do not advance the capture's dirty baseline.
    let mut sparse: Vec<MemorySegment> = Vec::new();
    for segment in segments {
        let mut offset = 0;
        while offset < segment.length {
            let length = (segment.length - offset).min(1024 * 1024) as usize;
            let bytes = reader
                .read_at(segment.source_offset + offset, length)
                .await?;
            ensure!(bytes.len() == length, "short frozen RAM scan");
            for (index, page) in bytes.as_chunks::<4096>().0.iter().enumerate() {
                if page.iter().any(|byte| *byte != 0) {
                    let relative = offset + (index * 4096) as u64;
                    let image_offset = segment.image_offset + relative;
                    let source_offset = segment.source_offset + relative;
                    if let Some(last) = sparse.last_mut().filter(|last| {
                        last.image_offset + last.length == image_offset
                            && last.source_offset + last.length == source_offset
                    }) {
                        last.length += 4096;
                    } else {
                        sparse.push(MemorySegment {
                            image_offset,
                            source_offset,
                            length: 4096,
                        });
                    }
                }
            }
            offset += length as u64;
        }
    }
    Ok(sparse)
}

fn memory_segments(
    map: &FrozenMemoryMap,
    base: Option<&MemoryLayout>,
) -> Result<Vec<MemorySegment>> {
    let layout = map.validate().map_err(anyhow::Error::msg)?;
    if let Some(base) = base {
        ensure!(&layout == base, "incremental memory base/layout conflict");
        // Zeros belong to the dirty set: write explicit bytes, not holes that
        // would inherit the nonzero ancestral page.
        return Ok(map
            .changed_ranges
            .iter()
            .map(|range| MemorySegment {
                image_offset: range.image_offset,
                source_offset: range.image_offset,
                length: range.length,
            })
            .collect());
    }
    Ok(map
        .regions
        .iter()
        .map(|region| MemorySegment {
            image_offset: region.image_offset,
            source_offset: region.image_offset,
            length: region.length,
        })
        .collect())
}

/// The immutable base selected at load, never advanced after capture/publication.
pub(super) struct DragonballMemoryBase {
    pub(super) artifacts: Arc<DragonballCaptureArtifacts>,
    pub(super) device: SharedReadOnlyDevice,
}

impl std::fmt::Debug for DragonballMemoryBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragonballMemoryBase")
            .field("image", &self.artifacts.manifest.memory.image_config_path)
            .field("device", &self.device.device_path())
            .finish()
    }
}

impl DragonballMemoryBase {
    pub(super) async fn acquire(
        artifacts: Arc<DragonballCaptureArtifacts>,
        global_config: &Path,
    ) -> Result<Arc<Self>> {
        let device = UblkDeviceManager::global()
            .get_or_create_retained_mem(
                &UblkCreateSpec::Overlaybd {
                    image_config: artifacts.manifest.memory.image_config_path.clone(),
                    global_config: global_config.into(),
                },
                artifacts.envelope.memory_layout.image_size,
                artifacts.clone(),
            )
            .await?;
        Ok(Arc::new(Self { artifacts, device }))
    }

    pub(super) fn device_path(&self) -> &Path {
        self.device.device_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentenv_vmm_protocol::{
        runtime::CaptureToken,
        runtime_memory::{FrozenMemoryRegion, MemoryRange},
    };

    fn map() -> FrozenMemoryMap {
        FrozenMemoryMap {
            token: CaptureToken {
                runner_instance_id: "owned".into(),
                generation: 1,
            },
            layout_id: "packed-ram-4k-v2".into(),
            page_size: 4096,
            regions: vec![
                FrozenMemoryRegion {
                    guest_base: 0,
                    host_base: 0x10000,
                    image_offset: 0,
                    length: 8192,
                },
                FrozenMemoryRegion {
                    guest_base: 0x1_0000_0000,
                    host_base: 0x20000,
                    image_offset: 8192,
                    length: 8192,
                },
            ],
            changed_ranges: vec![MemoryRange {
                image_offset: 4096,
                length: 8192,
            }],
            zero_ranges: vec![MemoryRange {
                image_offset: 8192,
                length: 4096,
            }],
        }
    }

    #[test]
    fn m2_delta_uses_only_cumulative_changed_pages_including_zeros() {
        let map = map();
        let layout = map.validate().unwrap();
        let segments = memory_segments(&map, Some(&layout)).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(
            (
                segments[0].image_offset,
                segments[0].source_offset,
                segments[0].length
            ),
            (4096, 4096, 8192)
        );
        let mut mismatch = layout.clone();
        mismatch.regions[1].guest_base += 4096;
        assert!(memory_segments(&map, Some(&mismatch)).is_err());
    }

    #[test]
    fn m2_cold_capture_covers_all_ram_even_without_changed_bits() {
        let mut map = map();
        map.changed_ranges.clear();
        map.zero_ranges.clear();
        let segments = memory_segments(&map, None).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[0].image_offset, segments[0].length), (0, 8192));
        assert_eq!((segments[1].image_offset, segments[1].length), (8192, 8192));
        assert!(memory_segments(&map, Some(&map.validate().unwrap()))
            .unwrap()
            .is_empty());
    }

    struct Pages {
        bytes: Vec<u8>,
        allowed: std::ops::Range<u64>,
    }

    #[async_trait::async_trait]
    impl VirtualFile for Pages {
        async fn read_at(&self, offset: u64, length: usize) -> Result<bytes::Bytes> {
            ensure!(
                offset >= self.allowed.start && offset + length as u64 <= self.allowed.end,
                "writer read an unchanged page"
            );
            Ok(bytes::Bytes::copy_from_slice(
                &self.bytes[offset as usize..offset as usize + length],
            ))
        }
        async fn write_at(&self, _: u64, _: &[u8]) -> Result<usize> {
            anyhow::bail!("read-only")
        }
        async fn size(&self) -> Result<u64> {
            Ok(self.bytes.len() as u64)
        }
    }

    async fn layer(path: &Path) -> Arc<dyn VirtualFile> {
        Arc::new(
            overlaybd::backend::local::LocalFile::builder()
                .write(false)
                .create(false)
                .open(path)
                .unwrap(),
        )
    }

    async fn full_capture_roundtrip(bytes: Vec<u8>, max_layer_bytes: u64) {
        let dir = tempfile::tempdir().unwrap();
        let mut map = map();
        let size = bytes.len() as u64;
        map.regions[0].length = size / 2;
        map.regions[1].image_offset = size / 2;
        map.regions[1].length = size / 2;
        map.regions[1].host_base = 0x1_0000_0000;
        // A first capture must preserve RAM even when no dirty bit describes it.
        map.changed_ranges.clear();
        map.zero_ranges.clear();
        let config = write_incremental_memory(
            &map,
            Arc::new(Pages {
                bytes: bytes.clone(),
                allowed: 0..size,
            }),
            None,
            dir.path(),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap();
        let config: overlaybd::config::ImageConfig =
            serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        assert_eq!(config.lowers.len(), 1);
        let path = Path::new(&config.lowers[0].file);
        let image = overlaybd::index_file::open_file_ro(layer(path).await)
            .await
            .unwrap();
        assert_eq!(image.size().await.unwrap(), size);
        assert_eq!(image.read_at(0, bytes.len()).await.unwrap().as_ref(), bytes);
        assert!(
            std::fs::metadata(path).unwrap().len() <= max_layer_bytes,
            "full capture unnecessarily materialized zero RAM"
        );
    }

    #[tokio::test]
    async fn full_capture_omits_zero_pages_without_losing_untracked_nonzero_ram() {
        let mut bytes = vec![0; 4 * 1024 * 1024];
        // Page, scan-chunk and packed-region boundaries, including a GPA hole.
        for offset in [
            0,
            4095,
            1024 * 1024 - 1,
            1024 * 1024,
            2 * 1024 * 1024 - 1,
            2 * 1024 * 1024,
            bytes.len() - 1,
        ] {
            bytes[offset] = 0x7b;
        }
        full_capture_roundtrip(bytes, 64 * 1024).await;
    }

    #[tokio::test]
    async fn full_capture_all_zero_retains_virtual_size_without_ram_payload() {
        full_capture_roundtrip(vec![0; 4 * 1024 * 1024], 16 * 1024).await;
    }

    #[tokio::test]
    async fn full_capture_dense_memory_roundtrips() {
        full_capture_roundtrip(vec![0x91; 4 * 1024 * 1024], 5 * 1024 * 1024).await;
    }

    struct FailedRead(bool);

    #[async_trait::async_trait]
    impl VirtualFile for FailedRead {
        async fn read_at(&self, _: u64, length: usize) -> Result<bytes::Bytes> {
            ensure!(self.0, "injected RAM read failure");
            Ok(bytes::Bytes::from(vec![0; length - 1]))
        }
        async fn write_at(&self, _: u64, _: &[u8]) -> Result<usize> {
            anyhow::bail!("read-only")
        }
        async fn size(&self) -> Result<u64> {
            Ok(16384)
        }
    }

    #[tokio::test]
    async fn full_capture_rejects_failed_or_short_scan_without_publishing() {
        for short in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let error = write_incremental_memory(
                &map(),
                Arc::new(FailedRead(short)),
                None,
                dir.path(),
                OverlaybdCompactOutput::Raw,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(if short {
                "short frozen RAM scan"
            } else {
                "injected RAM read failure"
            }));
            assert!(!dir.path().join("memory-image.json").exists());
            assert!(!dir.path().join("overlaybd.commit").exists());
        }
    }

    #[tokio::test]
    async fn m2_delta_stack_covers_zero_then_rewrite_without_reading_clean_pages() {
        let dir = tempfile::tempdir().unwrap();
        let map = map();
        let layout = map.validate().unwrap();
        let original = vec![0x41; 16384];
        let ancestor = write_memory_overlaybd(
            Arc::new(Pages {
                bytes: original.clone(),
                allowed: 0..16384,
            }),
            &memory_segments(&map, None).unwrap(),
            16384,
            &dir.path().join("ancestor"),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap();
        let mut empty = map.clone();
        empty.changed_ranges.clear();
        empty.zero_ranges.clear();
        let unchanged_delta = write_memory_overlaybd(
            Arc::new(Pages {
                bytes: vec![],
                allowed: 0..0,
            }),
            &memory_segments(&empty, Some(&layout)).unwrap(),
            16384,
            &dir.path().join("empty"),
            OverlaybdCompactOutput::Raw,
        )
        .await
        .unwrap();
        let unchanged = overlaybd::index_file::open_files_ro(&[
            layer(&ancestor).await,
            layer(&unchanged_delta).await,
        ])
        .await
        .unwrap();
        assert_eq!(
            unchanged.read_at(0, 16384).await.unwrap().as_ref(),
            original.as_slice()
        );
        for (index, value) in [0, 0x7b].into_iter().enumerate() {
            let mut expected = original.clone();
            expected[4096..8192].fill(0x23);
            expected[8192..12288].fill(value);
            let mut current = map.clone();
            current.token.generation += index as u64;
            if value != 0 {
                current.zero_ranges.clear();
            }
            let delta = write_memory_overlaybd(
                Arc::new(Pages {
                    bytes: expected.clone(),
                    allowed: 4096..12288,
                }),
                &capture_memory_segments(
                    &current,
                    Some(&layout),
                    &Pages {
                        bytes: expected.clone(),
                        allowed: 4096..12288,
                    },
                )
                .await
                .unwrap(),
                16384,
                &dir.path().join(format!("delta-{index}")),
                OverlaybdCompactOutput::Raw,
            )
            .await
            .unwrap();
            let stack = overlaybd::index_file::open_files_ro(&[
                layer(&ancestor).await,
                layer(&delta).await,
            ])
            .await
            .unwrap();
            assert_eq!(
                stack.read_at(0, 16384).await.unwrap().as_ref(),
                expected.as_slice()
            );
            let unchanged = overlaybd::index_file::open_file_ro(layer(&ancestor).await)
                .await
                .unwrap();
            assert_eq!(
                unchanged.read_at(0, 16384).await.unwrap().as_ref(),
                original.as_slice()
            );
        }
    }
}
