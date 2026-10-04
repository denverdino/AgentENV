use std::io::Read;

use serde::{de::DeserializeOwned, Serialize};

use crate::runtime::{RuntimeRequest, RuntimeResponse, RUNTIME_PROTOCOL_VERSION};
use crate::{RequestEnvelope, ResponseEnvelope, MAX_FRAME_BODY, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("truncated frame")]
    Truncated,
    #[error("frame body exceeds {MAX_FRAME_BODY} bytes: {0}")]
    TooLarge(usize),
    #[error("bytes after frame body")]
    TrailingBytes,
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u16),
    #[error("empty sandbox instance ID")]
    EmptyInstance,
    #[error("not a production runtime frame")]
    RuntimeMode,
    #[error("request budget must be nonzero")]
    ZeroBudget,
    #[error("response does not match request identity")]
    ResponseMismatch,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn encode_frame(value: &RequestEnvelope) -> Result<Vec<u8>, ProtocolError> {
    validate_request(value)?;
    encode(value)
}

pub fn validate_runtime_request(value: &RuntimeRequest) -> Result<(), ProtocolError> {
    validate_runtime_identity(
        &value.mode,
        value.protocol_version,
        &value.sandbox_instance_id,
    )?;
    if value.budget_ms == 0 {
        return Err(ProtocolError::ZeroBudget);
    }
    Ok(())
}

fn validate_runtime_identity(
    mode: &str,
    version: u16,
    instance: &str,
) -> Result<(), ProtocolError> {
    if mode != "runtime" {
        return Err(ProtocolError::RuntimeMode);
    }
    if version != RUNTIME_PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    if instance.is_empty() {
        return Err(ProtocolError::EmptyInstance);
    }
    Ok(())
}

pub fn encode_runtime_request(value: &RuntimeRequest) -> Result<Vec<u8>, ProtocolError> {
    validate_runtime_request(value)?;
    encode(value)
}

pub fn decode_runtime_request(bytes: &[u8]) -> Result<RuntimeRequest, ProtocolError> {
    let value = decode(bytes)?;
    validate_runtime_request(&value)?;
    Ok(value)
}

pub fn read_runtime_request(reader: &mut impl Read) -> Result<RuntimeRequest, ProtocolError> {
    let mut header = [0; 4];
    read_exact(reader, &mut header)?;
    let mut body = vec![0; checked_length(header)?];
    read_exact(reader, &mut body)?;
    let value = serde_json::from_slice(&body)?;
    validate_runtime_request(&value)?;
    Ok(value)
}

pub fn encode_runtime_response(value: &RuntimeResponse) -> Result<Vec<u8>, ProtocolError> {
    validate_runtime_identity(
        &value.mode,
        value.protocol_version,
        &value.sandbox_instance_id,
    )?;
    encode(value)
}

pub fn decode_runtime_response(bytes: &[u8]) -> Result<RuntimeResponse, ProtocolError> {
    let value: RuntimeResponse = decode(bytes)?;
    validate_runtime_identity(
        &value.mode,
        value.protocol_version,
        &value.sandbox_instance_id,
    )?;
    Ok(value)
}

pub fn validate_runtime_response(
    request: &RuntimeRequest,
    response: &RuntimeResponse,
) -> Result<(), ProtocolError> {
    validate_runtime_request(request)?;
    validate_runtime_identity(
        &response.mode,
        response.protocol_version,
        &response.sandbox_instance_id,
    )?;
    if request.request_id != response.request_id
        || request.sandbox_instance_id != response.sandbox_instance_id
    {
        return Err(ProtocolError::ResponseMismatch);
    }
    if let Ok(reply) = &response.result {
        use crate::runtime::{RuntimeCommand as Command, RuntimePhase, RuntimeReply as Reply};
        if !matches!(
            (&request.command, reply),
            (Command::Hello, Reply::Hello(_))
                | (Command::Status, Reply::Status(_))
                | (Command::BeginCapture | Command::LoadHeld(_), Reply::Held(_))
                | (Command::ExportMemoryMap { .. }, Reply::MemoryMap(_))
                | (
                    Command::Configure(_)
                        | Command::Start
                        | Command::ExportHeldSnapshot { .. }
                        | Command::ExportHeldState { .. }
                        | Command::ReleaseMemoryMap { .. }
                        | Command::EndCapture { .. }
                        | Command::Resume { .. }
                        | Command::Stop,
                    Reply::Done
                )
        ) {
            return Err(ProtocolError::ResponseMismatch);
        }
        match reply {
            Reply::MemoryMap(chunk) => {
                let Command::ExportMemoryMap { token, cursor } = &request.command else {
                    return Err(ProtocolError::ResponseMismatch);
                };
                if chunk.token != *token
                    || chunk.cursor != *cursor
                    || chunk
                        .changed_ranges
                        .len()
                        .saturating_add(chunk.zero_ranges.len())
                        > crate::runtime_memory::MAX_MEMORY_RANGES_PER_CHUNK
                {
                    return Err(ProtocolError::ResponseMismatch);
                }
            }
            Reply::Held(report) => report
                .validate()
                .map_err(|_| ProtocolError::ResponseMismatch)?,
            Reply::Status(status) => {
                if status.memory_map_pinned
                    && !matches!(
                        status.phase,
                        RuntimePhase::Held | RuntimePhase::LoadedHeld | RuntimePhase::Failed
                    )
                {
                    return Err(ProtocolError::ResponseMismatch);
                }
                if matches!(status.phase, RuntimePhase::Held | RuntimePhase::LoadedHeld)
                    != status.capture.is_some()
                {
                    return Err(ProtocolError::ResponseMismatch);
                }
                if let Some(report) = &status.capture {
                    report
                        .validate()
                        .map_err(|_| ProtocolError::ResponseMismatch)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn decode_request(bytes: &[u8]) -> Result<RequestEnvelope, ProtocolError> {
    let value = decode(bytes)?;
    validate_request(&value)?;
    Ok(value)
}

pub fn encode_response(value: &ResponseEnvelope) -> Result<Vec<u8>, ProtocolError> {
    validate_identity(value.protocol_version, &value.sandbox_instance_id)?;
    encode(value)
}

pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, ProtocolError> {
    let value: ResponseEnvelope = decode(bytes)?;
    validate_identity(value.protocol_version, &value.sandbox_instance_id)?;
    Ok(value)
}

pub fn validate_response(
    request: &RequestEnvelope,
    response: &ResponseEnvelope,
) -> Result<(), ProtocolError> {
    validate_identity(response.protocol_version, &response.sandbox_instance_id)?;
    if response.request_id != request.request_id
        || response.sandbox_instance_id != request.sandbox_instance_id
    {
        return Err(ProtocolError::ResponseMismatch);
    }
    Ok(())
}

pub fn read_request(reader: &mut impl Read) -> Result<RequestEnvelope, ProtocolError> {
    let mut header = [0; 4];
    read_exact(reader, &mut header)?;
    let mut body = vec![0; checked_length(header)?];
    read_exact(reader, &mut body)?;
    let value = serde_json::from_slice(&body)?;
    validate_request(&value)?;
    Ok(value)
}

pub fn validate_request(value: &RequestEnvelope) -> Result<(), ProtocolError> {
    validate_identity(value.protocol_version, &value.sandbox_instance_id)?;
    if value.budget_ms == 0 {
        return Err(ProtocolError::ZeroBudget);
    }
    Ok(())
}

fn validate_identity(version: u16, instance_id: &str) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    if instance_id.is_empty() {
        return Err(ProtocolError::EmptyInstance);
    }
    Ok(())
}

fn checked_length(header: [u8; 4]) -> Result<usize, ProtocolError> {
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_BODY {
        return Err(ProtocolError::TooLarge(length));
    }
    Ok(length)
}

fn read_exact(reader: &mut impl Read, buffer: &mut [u8]) -> Result<(), ProtocolError> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            ProtocolError::Io(error)
        }
    })
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BODY {
        return Err(ProtocolError::TooLarge(body.len()));
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend(body);
    Ok(frame)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtocolError> {
    let header: [u8; 4] = bytes
        .get(..4)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .unwrap();
    let length = checked_length(header)?;
    if bytes.len() > 4 + length {
        return Err(ProtocolError::TrailingBytes);
    }
    let body = bytes.get(4..4 + length).ok_or(ProtocolError::Truncated)?;
    Ok(serde_json::from_slice(body)?)
}
