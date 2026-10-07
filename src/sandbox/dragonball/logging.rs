//! Bounded diagnostic tails for the production Dragonball runner.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{ChildStderr, ChildStdout},
    task::JoinHandle,
};

const MAX_LOG_BYTES: usize = 128 * 1024;
const RETAIN_LOG_BYTES: usize = 64 * 1024;

struct BoundedLog {
    file: File,
    length: usize,
}

impl BoundedLog {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.length + bytes.len() > MAX_LOG_BYTES {
            // Compact only after another 64 KiB, rather than rewriting the tail
            // for every small console write. The live file never exceeds 128 KiB.
            let mut tail = vec![0; RETAIN_LOG_BYTES];
            self.file
                .seek(SeekFrom::Start((self.length - RETAIN_LOG_BYTES) as u64))?;
            self.file.read_exact(&mut tail)?;
            self.file.rewind()?;
            self.file.write_all(&tail)?;
            self.file.set_len(RETAIN_LOG_BYTES as u64)?;
            self.length = RETAIN_LOG_BYTES;
        }
        self.file.write_all(bytes)?;
        self.length += bytes.len();
        Ok(())
    }
}

struct OutputLog {
    name: &'static str,
    writer: Arc<Mutex<Option<BoundedLog>>>,
    task: Option<JoinHandle<()>>,
}

impl OutputLog {
    fn new(directory: &Path, name: &'static str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join(name))?;
        Ok(Self {
            name,
            writer: Arc::new(Mutex::new(Some(BoundedLog { file, length: 0 }))),
            task: None,
        })
    }

    fn start(&mut self, mut pipe: impl AsyncRead + Unpin + Send + 'static) {
        let writer = self.writer.clone();
        let name = self.name;
        self.task = Some(tokio::spawn(async move {
            let mut buffer = [0; 8192];
            loop {
                let length = match pipe.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(length) => length,
                    Err(error) => {
                        tracing::warn!(%error, name, "cannot drain runner output");
                        break;
                    }
                };
                let mut writer = writer.lock().unwrap();
                if let Some(log) = writer.as_mut() {
                    if let Err(error) = log.append(&buffer[..length]) {
                        tracing::warn!(%error, name, "cannot retain runner output; continuing drain");
                        // A full disk must not fill the pipe and block the VMM.
                        *writer = None;
                    }
                }
            }
        }));
    }

    async fn finish(&mut self) {
        if let Some(task) = self.task.as_mut() {
            // A descendant could still hold the pipe open after runner exit.
            // Borrowing the handle keeps cancellation owned by this guard.
            match tokio::time::timeout(Duration::from_secs(1), &mut *task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, name = self.name, "runner log drain failed");
                }
                Err(_) => {
                    tracing::warn!(name = self.name, "runner log drain timed out after exit");
                    task.abort();
                    let _ = task.await;
                }
            }
            self.task = None;
        }
    }
}

impl Drop for OutputLog {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        // Drop cannot await cancellation. Close under the writer lock so an
        // in-flight compact finishes before archiving/removing the directory,
        // and an already-polled task cannot write afterward or reopen a path.
        self.writer.lock().unwrap().take();
    }
}

pub(super) struct RunnerLogs([OutputLog; 2]);

impl RunnerLogs {
    pub(super) fn new(directory: &Path) -> std::io::Result<Self> {
        Ok(Self([
            OutputLog::new(directory, "serial.log")?,
            OutputLog::new(directory, "runner-error.log")?,
        ]))
    }

    pub(super) fn start(&mut self, stdout: ChildStdout, stderr: ChildStderr) {
        self.0[0].start(stdout);
        self.0[1].start(stderr);
    }

    pub(super) async fn finish(&mut self) {
        for output in &mut self.0 {
            output.finish().await;
        }
    }
}

/// Bound readiness errors even if an old or externally replaced log is large.
pub(super) fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(RETAIN_LOG_BYTES as u64),
    ))?;
    let mut bytes = Vec::with_capacity(RETAIN_LOG_BYTES);
    file.take(RETAIN_LOG_BYTES as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn diagnostic_read_bounds_large_sparse_logs_and_keeps_non_utf8_tail() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("serial.log");
        let mut file = File::create(&path).unwrap();
        file.seek(SeekFrom::Start(64 * 1024 * 1024)).unwrap();
        file.write_all(b"\xfflast failure").unwrap();
        let tail = read_tail(&path).unwrap();
        assert_eq!(tail.len(), 64 * 1024 + 2);
        assert!(tail.ends_with("\u{fffd}last failure"));
    }

    #[tokio::test]
    async fn failed_log_write_keeps_draining_the_runner_pipe() {
        let work = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/full", work.path().join("serial.log")).unwrap();
        let mut log = OutputLog::new(work.path(), "serial.log").unwrap();
        let (reader, mut writer) = tokio::io::duplex(32);
        log.start(reader);
        tokio::time::timeout(Duration::from_secs(2), async {
            writer.write_all(&vec![b'x'; 256 * 1024]).await.unwrap();
            writer.shutdown().await.unwrap();
            log.finish().await;
        })
        .await
        .expect("diagnostic disk errors must not block the runner output pipe");
        assert!(log.writer.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn dropping_log_owner_closes_writer_before_directory_cleanup() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().to_owned();
        let mut log = OutputLog::new(work.path(), "serial.log").unwrap();
        let state = log.writer.clone();
        let (reader, mut writer) = tokio::io::duplex(32);
        log.start(reader);
        let task = log.task.as_ref().unwrap().abort_handle();
        writer.write_all(b"pending output").await.unwrap();
        // No yield: exercise a drain task that has not yet started.
        drop(log);
        assert!(state.lock().unwrap().is_none());
        drop(work);
        assert!(!path.exists());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(writer.write_all(b"after cleanup").await.is_err());
        assert!(!path.exists());
    }
}
