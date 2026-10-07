use std::sync::Arc;
#[cfg(feature = "hypervisor-dragonball")]
use tempfile::TempDir;

// A cancelled daemon-release task must retain a still-live device's files.
#[cfg(feature = "hypervisor-dragonball")]
pub(crate) struct RetainedWork(pub(crate) Option<Arc<TempDir>>);
pub(crate) struct RetainedArtifacts(pub(crate) Option<Vec<Arc<dyn std::any::Any + Send + Sync>>>);
impl Drop for RetainedArtifacts {
    fn drop(&mut self) {
        if let Some(artifacts) = self.0.take() {
            std::mem::forget(artifacts);
        }
    }
}
#[cfg(feature = "hypervisor-dragonball")]
impl Drop for RetainedWork {
    fn drop(&mut self) {
        if let Some(work) = self.0.take() {
            std::mem::forget(work);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfirmed_cleanup_keeps_artifacts_but_confirmed_cleanup_releases_them() {
        let retained = Arc::new(tempfile::tempdir().unwrap());
        let path = retained.path().to_owned();
        drop(RetainedArtifacts(Some(vec![retained])));
        assert!(path.exists());
        // Test owns this deliberately leaked sentinel; no daemon uses it.
        std::fs::remove_dir_all(path).unwrap();

        let released = Arc::new(tempfile::tempdir().unwrap());
        let path = released.path().to_owned();
        let mut guard = RetainedArtifacts(Some(vec![released]));
        guard.0.take();
        drop(guard);
        assert!(!path.exists());
    }
}

/// One cleanup operation, shared by repeated stop calls. Errors are cached too:
/// a timed-out daemon request must never be repeated against a recycled device ID.
pub(crate) struct CleanupTask(
    futures::future::Shared<futures::future::BoxFuture<'static, Result<(), Arc<str>>>>,
);

impl CleanupTask {
    pub(crate) fn new(
        operation: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> anyhow::Result<Self> {
        use futures::FutureExt;
        let task = tokio::runtime::Handle::try_current()?.spawn(async move {
            operation.await.map_err(|error| {
                tracing::warn!(%error, "runtime cleanup incomplete; retaining backing artifacts");
                Arc::<str>::from(format!("{error:#}"))
            })
        });
        Ok(Self(
            async move {
                task.await
                    .unwrap_or_else(|error| Err(Arc::from(format!("cleanup task failed: {error}"))))
            }
            .boxed()
            .shared(),
        ))
    }

    pub(crate) async fn wait(&self) -> anyhow::Result<()> {
        self.0
            .clone()
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

#[cfg(test)]
mod cleanup_task_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn cancelled_cleanup_waiter_does_not_repeat_release_of_reassigned_id() {
        for uncertain_result in [false, true] {
            let owner = Arc::new(AtomicUsize::new(1));
            let requests = Arc::new(AtomicUsize::new(0));
            let (received, mut request) = oneshot::channel();
            let (response, response_rx) = oneshot::channel();
            let (completed, completion) = oneshot::channel();
            let operation_owner = owner.clone();
            let operation_requests = requests.clone();
            let cleanup = CleanupTask::new(async move {
                operation_requests.fetch_add(1, Ordering::SeqCst);
                assert_eq!(operation_owner.swap(0, Ordering::SeqCst), 1);
                received.send(()).unwrap();
                response_rx.await?;
                completed.send(()).unwrap();
                anyhow::ensure!(!uncertain_result, "release reply lost");
                Ok(())
            })
            .unwrap();
            // The daemon has processed release, but its reply is delayed.
            {
                let waiter = cleanup.wait();
                tokio::pin!(waiter);
                tokio::select! {
                    _ = &mut request => {},
                    result = &mut waiter => panic!("cleanup returned early: {result:?}"),
                }
            }
            owner.store(2, Ordering::SeqCst); // another VM acquires the same ID
            response.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), completion)
                .await
                .expect("cleanup stopped when its caller was cancelled")
                .unwrap();
            assert_eq!(cleanup.wait().await.is_err(), uncertain_result);
            assert_eq!(cleanup.wait().await.is_err(), uncertain_result);
            drop(cleanup);
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            assert_eq!(owner.load(Ordering::SeqCst), 2);
        }
    }
}

#[cfg(test)]
mod daemon_release_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use uvm_ublk_daemon::{
        protocol::{recv_message, send_message},
        DaemonRequest, DaemonResponse, UblkDaemonClient,
    };

    #[tokio::test]
    async fn delayed_release_reply_and_connection_loss_never_resend_recycled_device_id(
    ) -> anyhow::Result<()> {
        for lost_reply in [false, true] {
            let root = tempfile::tempdir()?;
            let socket = root.path().join("daemon.sock");
            let listener = tokio::net::UnixListener::bind(&socket)?;
            let owner = Arc::new(AtomicUsize::new(1));
            let server_owner = owner.clone();
            let (received, mut request) = oneshot::channel();
            let (reply, reply_rx) = oneshot::channel();
            let (finished, finished_rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                assert!(matches!(
                    recv_message::<DaemonRequest>(&mut stream).await?,
                    Some(DaemonRequest::ReleaseOverlaybd { dev_id: 7 })
                ));
                assert_eq!(server_owner.swap(0, Ordering::SeqCst), 1);
                received.send(()).unwrap();
                reply_rx.await?;
                if !lost_reply {
                    send_message(&mut stream, &DaemonResponse::Released).await?;
                }
                drop(stream);
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = result?;
                        let request = recv_message::<DaemonRequest>(&mut stream).await?;
                        panic!("release was resent after ID reassignment: {request:?}");
                    }
                    _ = finished_rx => {}
                }
                Ok::<_, anyhow::Error>(())
            });
            let client = UblkDaemonClient::new_for_test(socket, false);
            let (completed, completion) = oneshot::channel();
            let cleanup = CleanupTask::new(async move {
                let result = client.release_overlaybd(7).await;
                completed.send(()).unwrap();
                result
            })?;
            {
                let wait = cleanup.wait();
                tokio::pin!(wait);
                tokio::select! {
                    _ = &mut request => {},
                    result = &mut wait => panic!("release returned before daemon reply: {result:?}"),
                }
            }
            owner.store(2, Ordering::SeqCst);
            reply.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), completion).await??;
            assert_eq!(cleanup.wait().await.is_err(), lost_reply);
            assert_eq!(cleanup.wait().await.is_err(), lost_reply);
            drop(cleanup);
            finished.send(()).unwrap();
            server.await??;
            assert_eq!(owner.load(Ordering::SeqCst), 2);
        }
        Ok(())
    }
}
