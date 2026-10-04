//! Generation-owned remote RAM reads. HVA maps are transient, never artifacts.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentenv_vmm_protocol::{runtime::*, runtime_memory::*};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use overlaybd::virtual_file::VirtualFile;
use tokio::sync::Notify;

use super::{process::OwnedRunnerChild, runtime_client::RuntimeClient};

#[derive(Default)]
struct Reads {
    closed: bool,
    active: usize,
}

struct MemoryPin {
    client: Arc<tokio::sync::Mutex<RuntimeClient>>,
    process: OwnedRunnerChild,
    pid: u32,
    token: CaptureToken,
    map: OnceLock<FrozenMemoryMap>,
    reads: Mutex<Reads>,
    drained: Notify,
    #[cfg(test)]
    before_read: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(super) struct FrozenMemoryReadLease {
    pin: Arc<MemoryPin>,
    released: bool,
}

struct ActiveRead(Arc<MemoryPin>);
impl Drop for ActiveRead {
    fn drop(&mut self) {
        let mut reads = self.0.reads.lock().unwrap();
        reads.active -= 1;
        if reads.active == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

impl MemoryPin {
    async fn release(&self) -> Result<()> {
        self.reads.lock().unwrap().closed = true;
        loop {
            let drained = self.drained.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            if self.reads.lock().unwrap().active == 0 {
                break;
            }
            drained.await;
        }
        let mut client = self.client.lock().await;
        client
            .request(
                RuntimeCommand::ReleaseMemoryMap {
                    token: self.token.clone(),
                },
                Duration::from_secs(5),
            )
            .await?;
        client.finish_memory_lease();
        Ok(())
    }

    async fn read(self: &Arc<Self>, offset: u64, length: usize) -> Result<Bytes> {
        let map = self.map.get().context("memory map not assembled")?;
        let size = map.regions.last().map_or(0, |r| r.image_offset + r.length);
        ensure!(
            offset
                .checked_add(length as u64)
                .is_some_and(|end| end <= size),
            "read outside RAM image"
        );
        {
            let mut reads = self.reads.lock().unwrap();
            ensure!(!reads.closed, "memory read lease is closed");
            reads.active += 1;
        }
        let read = ActiveRead(self.clone());
        // Both buffer and pin move into the blocking closure. Dropping its join
        // future cannot release either while process_vm_readv is still running.
        tokio::task::spawn_blocking(move || {
            let pin = &read.0;
            #[cfg(test)]
            if let Some(before_read) = &pin.before_read {
                before_read();
            }
            let mut bytes = vec![0; length];
            let mut cursor = offset;
            let mut written = 0;
            for region in &pin.map.get().unwrap().regions {
                if cursor >= region.image_offset + region.length {
                    continue;
                }
                if written == length {
                    break;
                }
                let relative = cursor - region.image_offset;
                let count = (region.length - relative).min((length - written) as u64) as usize;
                pin.process.read_exact_remote(
                    pin.pid,
                    region.host_base + relative,
                    &mut bytes[written..written + count],
                )?;
                written += count;
                cursor += count as u64;
            }
            ensure!(written == length, "short mapped memory read");
            Ok(Bytes::from(bytes))
        })
        .await
        .context("memory reader panicked")?
    }
}

impl FrozenMemoryReadLease {
    pub(super) async fn acquire(
        client: Arc<tokio::sync::Mutex<RuntimeClient>>,
        process: OwnedRunnerChild,
        report: &CaptureReport,
        layout_id: String,
    ) -> Result<Self> {
        let pid = process.id().context("runner already reaped")?;
        let mut assembler = MemoryMapAssembler::new(
            report.token.clone(),
            layout_id,
            report.memory_layout.clone(),
        )
        .map_err(anyhow::Error::msg)?;
        {
            let mut client = client.lock().await;
            client.bind_process(pid)?;
            // Claim before constructing the cleanup owner: rejection must not
            // release another lease's pin for this same generation.
            client.claim_memory_lease()?;
        }
        let lease = Self {
            released: false,
            pin: Arc::new(MemoryPin {
                client,
                process,
                pid,
                token: report.token.clone(),
                map: OnceLock::new(),
                reads: Mutex::new(Reads::default()),
                drained: Notify::new(),
                #[cfg(test)]
                before_read: None,
            }),
        };
        let mut cursor = 0;
        loop {
            let reply = lease
                .pin
                .client
                .lock()
                .await
                .request(
                    RuntimeCommand::ExportMemoryMap {
                        token: report.token.clone(),
                        cursor,
                    },
                    Duration::from_secs(30),
                )
                .await?;
            let RuntimeReply::MemoryMap(chunk) = reply else {
                anyhow::bail!("missing memory map page")
            };
            let next = chunk.next_cursor;
            if let Some(map) = assembler.push(chunk).map_err(anyhow::Error::msg)? {
                lease
                    .pin
                    .map
                    .set(map)
                    .map_err(|_| anyhow::anyhow!("memory map already set"))?;
                return Ok(lease);
            }
            cursor = next.context("incomplete memory map")?;
        }
    }

    pub(super) fn reader(&self) -> Arc<dyn VirtualFile> {
        Arc::new(MemoryReader(self.pin.clone()))
    }

    pub(super) fn map(&self) -> &FrozenMemoryMap {
        self.pin.map.get().expect("acquired memory map")
    }

    pub(super) async fn release(mut self) -> Result<()> {
        self.pin.release().await?;
        self.released = true;
        Ok(())
    }
}

impl Drop for FrozenMemoryReadLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.pin.reads.lock().unwrap().closed = true;
        let pin = self.pin.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = pin.release().await {
                    tracing::error!(%error, "Dragonball memory pin release failed; terminating runner");
                    if let Err(cleanup) = pin.process.terminate_and_wait(Duration::from_secs(10)).await {
                        tracing::error!(%cleanup, "Dragonball reader teardown unconfirmed; resource owner must retain leases");
                    }
                }
            });
        }
        // Outside a runtime no reads can be newly scheduled. Existing blocking
        // reads still own the child and pin; resource teardown cannot reap them.
    }
}

struct MemoryReader(Arc<MemoryPin>);
#[async_trait]
impl VirtualFile for MemoryReader {
    async fn read_at(&self, offset: u64, length: usize) -> Result<Bytes> {
        self.0.read(offset, length).await
    }
    async fn read_at_into(&self, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let bytes = self.0.read(offset, dst.len()).await?;
        dst.copy_from_slice(&bytes);
        Ok(dst.len())
    }
    async fn write_at(&self, _: u64, _: &[u8]) -> Result<usize> {
        anyhow::bail!("RAM read lease is read-only")
    }
    async fn size(&self) -> Result<u64> {
        Ok(self
            .0
            .map
            .get()
            .context("memory map not assembled")?
            .regions
            .last()
            .map_or(0, |r| r.image_offset + r.length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentenv_vmm_protocol::{decode_runtime_request, encode_runtime_response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct OpenOnDrop(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for OpenOnDrop {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }

    #[tokio::test]
    async fn m2_cancelled_reader_retains_pin_until_drain() {
        for explicit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("runtime.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (released, mut release_seen) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = [0; 4];
                stream.read_exact(&mut header).await.unwrap();
                let mut frame = vec![0; 4 + u32::from_be_bytes(header) as usize];
                frame[..4].copy_from_slice(&header);
                stream.read_exact(&mut frame[4..]).await.unwrap();
                let request = decode_runtime_request(&frame).unwrap();
                assert!(matches!(
                    request.command,
                    RuntimeCommand::ReleaseMemoryMap { .. }
                ));
                released.send(()).unwrap();
                stream
                    .write_all(
                        &encode_runtime_response(&RuntimeResponse {
                            mode: "runtime".into(),
                            protocol_version: RUNTIME_PROTOCOL_VERSION,
                            request_id: request.request_id,
                            sandbox_instance_id: request.sandbox_instance_id,
                            result: Ok(RuntimeReply::Done),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            });
            let process = OwnedRunnerChild::from(
                tokio::process::Command::new("/bin/sleep")
                    .arg("60")
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
            let token = CaptureToken {
                runner_instance_id: "runner".into(),
                generation: 1,
            };
            let (entered, ready) = tokio::sync::oneshot::channel();
            let entered = Mutex::new(Some(entered));
            let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let release_gate = OpenOnDrop(gate.clone());
            let pin = Arc::new(MemoryPin {
                client: Arc::new(tokio::sync::Mutex::new(RuntimeClient::new(
                    socket,
                    "test".into(),
                ))),
                pid: process.id().unwrap(),
                process,
                token: token.clone(),
                map: OnceLock::from(FrozenMemoryMap {
                    token,
                    layout_id: "packed-ram-4k-v2".into(),
                    page_size: 4096,
                    regions: vec![FrozenMemoryRegion {
                        guest_base: 0,
                        host_base: 4096,
                        image_offset: 0,
                        length: 4096,
                    }],
                    changed_ranges: vec![],
                    zero_ranges: vec![],
                }),
                reads: Mutex::new(Reads::default()),
                drained: Notify::new(),
                before_read: Some(Arc::new(move || {
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    let mut open = gate.0.lock().unwrap();
                    while !*open {
                        open = gate.1.wait(open).unwrap();
                    }
                })),
            });
            let lease = FrozenMemoryReadLease {
                pin: pin.clone(),
                released: false,
            };
            let reader = lease.reader();
            let task = tokio::spawn(async move { reader.read_at(0, 1).await });
            ready.await.unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(pin.reads.lock().unwrap().active, 1);
            let release_task = if explicit {
                Some(tokio::spawn(lease.release()))
            } else {
                drop(lease);
                None
            };
            let premature =
                tokio::time::timeout(Duration::from_millis(30), &mut release_seen).await;
            assert!(
                premature.is_err(),
                "read future cancellation released a still-active memory pin"
            );
            drop(release_gate);
            tokio::time::timeout(Duration::from_secs(3), release_seen)
                .await
                .unwrap()
                .unwrap();
            if let Some(task) = release_task {
                task.await.unwrap().unwrap();
            }
            server.await.unwrap();
            assert_eq!(pin.reads.lock().unwrap().active, 0);
            assert!(MemoryReader(pin).read_at(0, 1).await.is_err());
        }
    }
}
