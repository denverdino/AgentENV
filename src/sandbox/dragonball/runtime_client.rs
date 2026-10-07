use agentenv_vmm_protocol::{
    decode_runtime_response, encode_runtime_request, runtime::*, validate_runtime_response,
    MAX_FRAME_BODY,
};
use anyhow::{Context, Result};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[derive(Debug, thiserror::Error)]
#[error("runtime {code}: {message}")]
struct RuntimeRemoteError {
    code: String,
    message: String,
}

pub struct RuntimeClient {
    socket: PathBuf,
    instance: String,
    next_id: u64,
    outcome_unknown: bool,
    expected_pid: Option<u32>,
    memory_lease_active: bool,
}
impl RuntimeClient {
    pub fn new(socket: PathBuf, instance: String) -> Self {
        Self {
            socket,
            instance,
            next_id: 1,
            outcome_unknown: false,
            expected_pid: None,
            memory_lease_active: false,
        }
    }
    pub(super) fn bind_process(&mut self, pid: u32) -> Result<()> {
        anyhow::ensure!(
            self.expected_pid.is_none_or(|expected| expected == pid),
            "runtime process binding changed"
        );
        self.expected_pid = Some(pid);
        Ok(())
    }

    pub(super) fn claim_memory_lease(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.memory_lease_active,
            "runtime already has a memory read lease"
        );
        self.memory_lease_active = true;
        Ok(())
    }

    pub(super) fn finish_memory_lease(&mut self) {
        self.memory_lease_active = false;
    }

    pub(super) async fn begin_capture(
        &mut self,
    ) -> crate::sandbox::SandboxCaptureResult<CaptureReport> {
        use crate::sandbox::SandboxCaptureError;
        match self
            .request(RuntimeCommand::BeginCapture, Duration::from_secs(30))
            .await
        {
            Ok(RuntimeReply::Held(report)) => Ok(report),
            Ok(_) => Err(SandboxCaptureError::terminal(anyhow::anyhow!(
                "missing held capture proof"
            ))),
            Err(error) => {
                let rejected = error
                    .downcast_ref::<RuntimeRemoteError>()
                    .is_some_and(|error| error.code == "snapshot_preflight_error");
                // A lost response is not a rejection. Only the runner's known-error
                // contract, followed by a positive state check, permits recovery.
                if rejected
                    && matches!(
                        self.request(RuntimeCommand::Status, Duration::from_secs(5))
                            .await,
                        Ok(RuntimeReply::Status(RuntimeStatus {
                            phase: RuntimePhase::Running,
                            capture: None,
                            memory_map_pinned: false,
                        }))
                    )
                {
                    Err(SandboxCaptureError::recoverable(error))
                } else {
                    Err(SandboxCaptureError::terminal(error))
                }
            }
        }
    }
    pub async fn request(
        &mut self,
        command: RuntimeCommand,
        budget: Duration,
    ) -> Result<RuntimeReply> {
        anyhow::ensure!(
            !self.outcome_unknown
                || matches!(
                    command,
                    RuntimeCommand::Status
                        | RuntimeCommand::EndCapture { .. }
                        | RuntimeCommand::ReleaseMemoryMap { .. }
                        | RuntimeCommand::Stop
                ),
            "runtime outcome unknown; query, confirm release with the same token, or teardown"
        );
        let mutating = !matches!(command, RuntimeCommand::Hello | RuntimeCommand::Status);
        let was_unknown = self.outcome_unknown;
        let request = RuntimeRequest {
            mode: "runtime".into(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            request_id: self.next_id,
            sandbox_instance_id: self.instance.clone(),
            budget_ms: budget.as_millis().try_into()?,
            command,
        };
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("runtime request ID exhausted")?;
        let frame = encode_runtime_request(&request)?;
        // Arm before the first await: dropping this future is not remote cancellation.
        if mutating {
            self.outcome_unknown = true;
        }
        let response = tokio::time::timeout(budget, async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            anyhow::ensure!(
                stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
                "runtime socket peer UID mismatch"
            );
            if let Some(pid) = self.expected_pid {
                anyhow::ensure!(
                    stream.peer_cred()?.pid() == Some(i32::try_from(pid)?),
                    "runtime socket peer PID mismatch"
                );
            }
            stream.write_all(&frame).await?;
            let mut header = [0; 4];
            stream.read_exact(&mut header).await?;
            let size = u32::from_be_bytes(header) as usize;
            anyhow::ensure!(
                size <= MAX_FRAME_BODY,
                "runtime response exceeds frame limit"
            );
            let mut bytes = vec![0; 4 + size];
            bytes[..4].copy_from_slice(&header);
            stream.read_exact(&mut bytes[4..]).await?;
            let response = decode_runtime_response(&bytes)?;
            validate_runtime_response(&request, &response)?;
            Ok::<_, anyhow::Error>(response)
        })
        .await
        .context("runtime request deadline exceeded")??;
        match response.result {
            Ok(reply) => {
                if mutating {
                    self.outcome_unknown = false;
                }
                Ok(reply)
            }
            Err(error) => {
                if mutating && error.code != "vmm_error" {
                    self.outcome_unknown = was_unknown;
                }
                Err(RuntimeRemoteError {
                    code: error.code,
                    message: error.message,
                }
                .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentenv_vmm_protocol::{decode_runtime_request, encode_runtime_response};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    #[tokio::test]
    async fn capture_rejection_requires_positive_running_confirmation() {
        for (code, phase, pinned, recoverable) in [
            (
                "snapshot_preflight_error",
                Some(RuntimePhase::Running),
                false,
                true,
            ),
            (
                "snapshot_preflight_error",
                Some(RuntimePhase::Running),
                true,
                false,
            ),
            (
                "snapshot_preflight_error",
                Some(RuntimePhase::Failed),
                false,
                false,
            ),
            ("snapshot_preflight_error", None, false, false),
            ("vmm_error", Some(RuntimePhase::Running), false, false),
            ("invalid_phase", Some(RuntimePhase::Running), false, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("runtime.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (mut peer, _) = listener.accept().await.unwrap();
                let request = read_request(&mut peer).await;
                assert_eq!(request.command, RuntimeCommand::BeginCapture);
                let response = RuntimeResponse {
                    mode: "runtime".into(),
                    protocol_version: RUNTIME_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    sandbox_instance_id: request.sandbox_instance_id.clone(),
                    result: Err(RuntimeError {
                        code: code.into(),
                        message: "capture refused".into(),
                    }),
                };
                peer.write_all(&encode_runtime_response(&response).unwrap())
                    .await
                    .unwrap();
                if code == "snapshot_preflight_error" {
                    let (mut peer, _) = listener.accept().await.unwrap();
                    let status = read_request(&mut peer).await;
                    assert_eq!(status.command, RuntimeCommand::Status);
                    let Some(phase) = phase else {
                        return;
                    };
                    peer.write_all(
                        &encode_runtime_response(&RuntimeResponse {
                            request_id: status.request_id,
                            result: Ok(RuntimeReply::Status(RuntimeStatus {
                                phase,
                                capture: None,
                                memory_map_pinned: pinned,
                            })),
                            ..response
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                }
            });
            let mut client = RuntimeClient::new(socket, "instance".into());
            let error = client.begin_capture().await.unwrap_err();
            assert_eq!(!error.is_terminal(), recoverable, "{code}: {error}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn lost_capture_reply_remains_terminal_and_prevents_retry() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            assert_eq!(
                read_request(&mut peer).await.command,
                RuntimeCommand::BeginCapture
            );
            // EOF after receipt does not prove that the runner rejected capture.
        });
        let mut client = RuntimeClient::new(socket, "instance".into());
        assert!(client.begin_capture().await.unwrap_err().is_terminal());
        assert!(client.outcome_unknown);
        assert!(client.begin_capture().await.unwrap_err().is_terminal());
        server.await.unwrap();
    }

    #[test]
    fn m2_runtime_client_cannot_acquire_another_live_memory_lease() {
        let mut client = RuntimeClient::new("/unused".into(), "instance".into());
        client.claim_memory_lease().unwrap();
        assert!(
            client.claim_memory_lease().is_err(),
            "a failed second acquire must not own/release the first pin"
        );
        client.finish_memory_lease();
        client.claim_memory_lease().unwrap();
    }

    #[tokio::test]
    async fn m2_runtime_socket_rejects_wrong_process_before_sending() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        });
        let mut client = RuntimeClient::new(path, "instance".into());
        client.bind_process(std::process::id() + 1).unwrap();
        assert!(client.bind_process(std::process::id()).is_err());
        let error = client
            .request(RuntimeCommand::Hello, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("peer PID mismatch"), "{error}");
        server.await.unwrap();
    }

    async fn read_request(stream: &mut tokio::net::UnixStream) -> RuntimeRequest {
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        let mut frame = vec![0; 4 + u32::from_be_bytes(header) as usize];
        frame[..4].copy_from_slice(&header);
        stream.read_exact(&mut frame[4..]).await.unwrap();
        decode_runtime_request(&frame).unwrap()
    }

    #[tokio::test]
    async fn runtime_timeout_does_not_reissue_export() {
        check_export_uncertainty(false).await;
    }

    #[tokio::test]
    async fn runtime_caller_cancellation_does_not_reissue_export() {
        check_export_uncertainty(true).await;
    }

    async fn check_export_uncertainty(cancel: bool) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let export = read_request(&mut first).await;
            assert!(matches!(
                export.command,
                RuntimeCommand::ExportHeldSnapshot { .. }
            ));
            sent.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let status = read_request(&mut second).await;
            assert_eq!(
                status.command,
                RuntimeCommand::Status,
                "the next wire request must query, not export again"
            );
            second
                .write_all(
                    &encode_runtime_response(&RuntimeResponse {
                        mode: "runtime".into(),
                        protocol_version: RUNTIME_PROTOCOL_VERSION,
                        request_id: status.request_id,
                        sandbox_instance_id: status.sandbox_instance_id,
                        result: Ok(RuntimeReply::Status(RuntimeStatus {
                            memory_map_pinned: false,
                            phase: RuntimePhase::Held,
                            capture: Some(CaptureReport {
                                token: CaptureToken {
                                    runner_instance_id: "runner".into(),
                                    generation: 1,
                                },
                                memory_layout: MemoryLayout {
                                    page_size: 4096,
                                    image_size: 4096,
                                    regions: vec![MemoryRegion {
                                        guest_base: 0,
                                        image_offset: 0,
                                        length: 4096,
                                    }],
                                },
                                vcpu_ids: vec![0],
                                worker_acks: vec![
                                    WorkerAck {
                                        device_id: "balloon:memory".into(),
                                        generation: 1,
                                        pending_io: 0,
                                        memory_writers: 0,
                                        flush_completed: false,
                                    },
                                    WorkerAck {
                                        device_id: "block:root/q0".into(),
                                        generation: 1,
                                        pending_io: 0,
                                        memory_writers: 0,
                                        flush_completed: true,
                                    },
                                    WorkerAck {
                                        device_id: "net:eth0".into(),
                                        generation: 1,
                                        pending_io: 0,
                                        memory_writers: 0,
                                        flush_completed: false,
                                    },
                                ],
                            }),
                        })),
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let mut client = RuntimeClient::new(path, "instance".into());
        let export = RuntimeCommand::ExportHeldSnapshot {
            token: CaptureToken {
                runner_instance_id: "runner".into(),
                generation: 1,
            },
            state_path: "/work/state".into(),
            memory_path: "/work/memory".into(),
        };
        if cancel {
            let mut pending =
                Box::pin(client.request(export.clone(), std::time::Duration::from_secs(1)));
            tokio::select! {
                result = &mut pending => panic!("request finished before cancellation: {result:?}"),
                ack = received => ack.unwrap(),
            }
            drop(pending);
        } else {
            assert!(client
                .request(export.clone(), std::time::Duration::from_millis(20))
                .await
                .is_err());
            received.await.unwrap();
        }
        assert!(client
            .request(export, std::time::Duration::from_millis(200))
            .await
            .is_err());
        let reply = client
            .request(RuntimeCommand::Status, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(reply, RuntimeReply::Status(_)));
        server.await.unwrap();
    }
}
