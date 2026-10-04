use std::{path::PathBuf, time::Duration};

use agentenv_vmm_protocol::{
    decode_response, encode_frame, validate_response, ProbeCommand, ProbeReply, RequestEnvelope,
    MAX_FRAME_BODY, PROTOCOL_VERSION,
};
use anyhow::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

pub(super) const M0_REVISION: &str = "c77812d669737c11cb16321827acc1c79bf065f3";
pub(super) const M0_PROFILE: &str = "m0-mmio-blk-net";

pub(super) struct ProbeClient {
    pub(super) socket: PathBuf,
    pub(super) instance: String,
    next_id: u64,
}

impl ProbeClient {
    pub(super) fn new(socket: PathBuf, instance: String) -> Self {
        Self {
            socket,
            instance,
            next_id: 1,
        }
    }

    pub(super) async fn request(
        &mut self,
        command: ProbeCommand,
        budget: Duration,
    ) -> Result<ProbeReply> {
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: self.next_id,
            sandbox_instance_id: self.instance.clone(),
            budget_ms: budget.as_millis().try_into()?,
            command,
        };
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("probe request ID exhausted")?;
        let frame = encode_frame(&request)?;
        tokio::time::timeout(budget, async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            stream.write_all(&frame).await?;
            let mut header = [0; 4];
            stream.read_exact(&mut header).await?;
            let size = u32::from_be_bytes(header) as usize;
            anyhow::ensure!(size <= MAX_FRAME_BODY, "probe response exceeds frame limit");
            let mut response_frame = vec![0; 4 + size];
            response_frame[..4].copy_from_slice(&header);
            stream.read_exact(&mut response_frame[4..]).await?;
            let response = decode_response(&response_frame)?;
            validate_response(&request, &response)?;
            response
                .result
                .map_err(|error| anyhow::anyhow!("runner {}: {}", error.code, error.message))
        })
        .await
        .context("probe request deadline exceeded")?
    }
}
