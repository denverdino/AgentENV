use crate::sandbox::metadata::SandboxMetadataDocument;
use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use rand::{rngs::SysRng, TryRng};
use sha2::Sha256;
use std::{
    fs::File,
    io::{Read, Write},
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) const METADATA_ADDRESS: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
pub(super) const METADATA_PORT: u16 = 15001;
const MAX_DOCUMENT_BYTES: usize = 1_048_576;
const MAX_TOKEN_TTL: u32 = 21_600;
const MAX_REQUEST_BYTES: usize = 8192;

struct MetadataState {
    instance: String,
    document: serde_json::Value,
    key: [u8; 32],
    epoch: Instant,
    recorded_requests: Vec<serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
enum MetadataError {
    #[error("invalid or expired metadata token")]
    Unauthorized,
    #[error("metadata resource not found")]
    NotFound,
    #[error("metadata value cannot be represented as plain text")]
    UnsupportedValueType,
}

impl MetadataState {
    fn new(
        instance: &str,
        document: SandboxMetadataDocument,
        now: Instant,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !instance.is_empty(),
            "metadata runtime instance ID is empty"
        );
        let mut key = [0; 32];
        SysRng
            .try_fill_bytes(&mut key)
            .context("generate metadata authority key")?;
        Ok(Self {
            instance: instance.into(),
            document: checked_document(document)?,
            key,
            epoch: now,
            recorded_requests: Vec::new(),
        })
    }
    fn issue_token(&self, ttl: u32, now: Instant) -> anyhow::Result<String> {
        anyhow::ensure!(
            (1..=MAX_TOKEN_TTL).contains(&ttl),
            "invalid metadata token TTL"
        );
        let expires = u64::try_from(now.saturating_duration_since(self.epoch).as_millis())?
            .checked_add(u64::from(ttl) * 1000)
            .context("metadata token expiry overflow")?;
        let mut payload = [0; 24];
        payload[..8].copy_from_slice(&expires.to_be_bytes());
        SysRng
            .try_fill_bytes(&mut payload[8..])
            .context("generate metadata token nonce")?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)?;
        mac.update(self.instance.as_bytes());
        mac.update(&payload);
        let mut token = payload.to_vec();
        token.extend_from_slice(&mac.finalize().into_bytes());
        Ok(URL_SAFE_NO_PAD.encode(token))
    }
    fn valid_token(&self, token: &str, now: Instant) -> bool {
        // Fixed-size, stateless authority: no guest-controlled token cache.
        if token.len() != 75 {
            return false;
        }
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(token) else {
            return false;
        };
        if bytes.len() != 56 {
            return false;
        }
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.key) else {
            return false;
        };
        mac.update(self.instance.as_bytes());
        mac.update(&bytes[..24]);
        if mac.verify_slice(&bytes[24..]).is_err() {
            return false;
        }
        let expires = u64::from_be_bytes(bytes[..8].try_into().expect("checked token length"));
        now.saturating_duration_since(self.epoch).as_millis() < u128::from(expires)
    }
    fn get(
        &self,
        token: &str,
        path: &str,
        json: bool,
        now: Instant,
    ) -> Result<String, MetadataError> {
        if !self.valid_token(token, now) {
            return Err(MetadataError::Unauthorized);
        }
        let mut path = path.to_string();
        while path.contains("//") {
            path = path.replace("//", "/");
        }
        let path = path.strip_suffix('/').unwrap_or(&path);
        let value = self.document.pointer(path).ok_or(MetadataError::NotFound)?;
        if json {
            return Ok(value.to_string());
        }
        match value {
            serde_json::Value::String(value) => Ok(value.clone()),
            serde_json::Value::Object(fields) => Ok(fields
                .iter()
                .map(|(key, value)| {
                    if value.is_object() {
                        format!("{key}/")
                    } else {
                        key.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")),
            _ => Err(MetadataError::UnsupportedValueType),
        }
    }
    #[cfg(test)]
    fn replace(&mut self, instance: &str, doc: SandboxMetadataDocument) -> anyhow::Result<()> {
        anyhow::ensure!(
            !instance.is_empty(),
            "metadata runtime instance ID is empty"
        );
        let document = checked_document(doc)?;
        if instance != self.instance {
            let mut key = [0; 32];
            SysRng.try_fill_bytes(&mut key)?;
            self.key = key;
            self.instance = instance.into();
        }
        self.document = document;
        Ok(())
    }
}

fn checked_document(document: SandboxMetadataDocument) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        serde_json::to_vec(&document)?.len() <= MAX_DOCUMENT_BYTES,
        "metadata document exceeds 1 MiB"
    );
    Ok(serde_json::to_value(document)?)
}

pub(crate) struct MetadataListener {
    state: Arc<Mutex<MetadataState>>,
    shutdown: UnixStream,
    completed: tokio::sync::oneshot::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl MetadataListener {
    pub(crate) fn recorded_requests(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata state poisoned"))?
            .recorded_requests
            .clone())
    }
    pub(crate) fn start(
        netns: &Path,
        instance: &str,
        document: SandboxMetadataDocument,
    ) -> anyhow::Result<Self> {
        let netns = File::open(netns).context("open metadata namespace")?;
        let state = MetadataState::new(instance, document, Instant::now())?;
        Self::start_with_setup(state, move || {
            crate::privileges::run_with_scoped_capabilities(
                &[crate::privileges::CAP_SYS_ADMIN],
                || {
                    nix::sched::setns(netns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)?;
                    // The socket retains its namespace when returned to the
                    // unprivileged worker. No capability is added for port 80:
                    // the namespace DNAT rule maps guest HTTP to this private port.
                    let listener = TcpListener::bind((METADATA_ADDRESS, METADATA_PORT))?;
                    Ok((listener, Box::new(netns) as Box<dyn Send>))
                },
            )
        })
    }
    fn start_with_setup<F>(state: MetadataState, setup: F) -> anyhow::Result<Self>
    where
        F: FnOnce() -> anyhow::Result<(TcpListener, Box<dyn Send>)> + Send + 'static,
    {
        let state = Arc::new(Mutex::new(state));
        // Socketpair wake-up works across network namespaces and cannot fill
        // up: closing the write half leaves a persistent readable EOF.
        let (shutdown, shutdown_rx) = UnixStream::pair()?;
        let (completed_tx, completed) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let shared_state = state.clone();
        let worker = thread::Builder::new()
            .name("aenv-metadata".into())
            .spawn(move || {
                let (listener, _namespace_reference) = match setup() {
                    Ok(resources) => resources,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                if let Err(error) = listener.set_nonblocking(true) {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                let mut descriptors = [
                    libc::pollfd {
                        fd: listener.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: shutdown_rx.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                loop {
                    // Only this worker accepts. Keep the listener nonblocking
                    // so a spurious readiness notification cannot delay stop.
                    if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        tracing::warn!(%error, "metadata listener wait failed");
                        break;
                    }
                    if descriptors[1].revents != 0 {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = serve_connection(stream, &shared_state);
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue
                        }
                        Err(error) => {
                            tracing::warn!(%error, "metadata listener failed");
                            break;
                        }
                    }
                }
                let _ = completed_tx.send(());
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                state,
                shutdown,
                completed,
                worker: Some(worker),
            }),
            ready => {
                let _ = worker.join();
                anyhow::bail!("metadata listener startup failed: {ready:?}")
            }
        }
    }
    // M0 restores create a fresh listener; in-place reuse is an M1 interface.
    #[cfg(test)]
    pub(crate) fn replace(
        &self,
        instance: &str,
        document: SandboxMetadataDocument,
    ) -> anyhow::Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata state poisoned"))?
            .replace(instance, document)
    }
    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        let _ = self.shutdown.shutdown(Shutdown::Write);
        // Keep the join owned by self across awaits: cancellation falls back
        // to Drop joining it before the namespace lease can be returned.
        if self.worker.is_some() {
            let _ = (&mut self.completed).await;
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("metadata worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for MetadataListener {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown(Shutdown::Write);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct MetadataReply {
    status: u16,
    body: String,
    content_type: &'static str,
    ttl_header: Option<(&'static str, u32)>,
}

impl MetadataReply {
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "text/plain",
            ttl_header: None,
        }
    }
}

fn handle_http(state: &MetadataState, bytes: &[u8], now: Instant) -> MetadataReply {
    if bytes.len() > MAX_REQUEST_BYTES {
        return MetadataReply::text(413, "metadata request too large");
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    let Ok(httparse::Status::Complete(length)) = request.parse(bytes) else {
        return MetadataReply::text(400, "invalid metadata HTTP request");
    };
    if length != bytes.len()
        || request
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
    {
        return MetadataReply::text(400, "metadata request body/pipelining is unsupported");
    }
    let mut seen = std::collections::HashSet::new();
    if request
        .headers
        .iter()
        .any(|h| !seen.insert(h.name.to_ascii_lowercase()))
    {
        return MetadataReply::text(400, "duplicate metadata header");
    }
    let header = |names: &[&str]| {
        request
            .headers
            .iter()
            .find(|h| names.iter().any(|n| h.name.eq_ignore_ascii_case(n)))
            .and_then(|h| std::str::from_utf8(h.value).ok())
    };
    if header(&["content-length"]).is_some_and(|length| length != "0") {
        return MetadataReply::text(400, "metadata request body is unsupported");
    }
    let path = request.path.unwrap_or("");
    match request.method {
        Some("PUT") => {
            if header(&["x-forwarded-for"]).is_some() {
                return MetadataReply::text(400, "forwarded metadata token request is forbidden");
            }
            if path.replace("//", "/") != "/latest/api/token" {
                return MetadataReply::text(404, "metadata resource not found");
            }
            // These aliases already exist in the pinned Firecracker contract;
            // they are not an independent AWS API extension.
            let ttl_name = if header(&["x-metadata-token-ttl-seconds"]).is_some() {
                "x-metadata-token-ttl-seconds"
            } else {
                "x-aws-ec2-metadata-token-ttl-seconds"
            };
            let Some(ttl) = header(&[ttl_name])
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|ttl| (1..=MAX_TOKEN_TTL).contains(ttl))
            else {
                return MetadataReply::text(400, "metadata token TTL must be 1..21600 seconds");
            };
            match state.issue_token(ttl, now) {
                Ok(token) => MetadataReply {
                    status: 200,
                    body: token,
                    content_type: "text/plain",
                    ttl_header: Some((ttl_name, ttl)),
                },
                Err(_) => MetadataReply::text(500, "metadata token generation failed"),
            }
        }
        Some("GET") => {
            let token = header(&["x-metadata-token", "x-aws-ec2-metadata-token"]).unwrap_or("");
            let json = header(&["accept"]).is_some_and(|accept| accept == "application/json");
            match state.get(token, path, json, now) {
                Ok(body) => MetadataReply {
                    status: 200,
                    body,
                    content_type: if json {
                        "application/json"
                    } else {
                        "text/plain"
                    },
                    ttl_header: None,
                },
                Err(MetadataError::Unauthorized) => {
                    MetadataReply::text(401, "invalid or expired metadata token")
                }
                Err(MetadataError::NotFound) => {
                    MetadataReply::text(404, "metadata resource not found")
                }
                Err(MetadataError::UnsupportedValueType) => {
                    MetadataReply::text(501, "metadata value cannot be represented as plain text")
                }
            }
        }
        _ => MetadataReply::text(405, "only GET and PUT are allowed"),
    }
}

fn serve_connection(mut stream: TcpStream, state: &Mutex<MetadataState>) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut buffer = Vec::new();
    let mut chunk = [0; 1024];
    while buffer.len() <= MAX_REQUEST_BYTES && !buffer.windows(4).any(|s| s == b"\r\n\r\n") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "metadata request deadline exceeded");
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
    let reply = {
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata state poisoned"))?;
        let reply = handle_http(&state, &buffer, Instant::now());
        if state.recorded_requests.len() < 64 {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut request = httparse::Request::new(&mut headers);
            if matches!(request.parse(&buffer), Ok(httparse::Status::Complete(_))) {
                let headers: serde_json::Map<String, serde_json::Value> = request
                    .headers
                    .iter()
                    .filter_map(|header| {
                        let name = header.name.to_ascii_lowercase();
                        let value = match name.as_str() {
                            "x-metadata-token" | "x-aws-ec2-metadata-token" => "<redacted>",
                            "x-metadata-token-ttl-seconds"
                            | "x-aws-ec2-metadata-token-ttl-seconds"
                            | "accept" => std::str::from_utf8(header.value).ok()?,
                            _ => return None,
                        };
                        Some((name, serde_json::Value::String(value.into())))
                    })
                    .collect();
                state.recorded_requests.push(serde_json::json!({"method": request.method, "path": request.path, "headers": headers, "status": reply.status}));
            }
        }
        reply
    };
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len(),
        reply.content_type
    );
    if let Some((name, ttl)) = reply.ttl_header {
        headers.push_str(&format!("{name}: {ttl}\r\n"));
    }
    if reply.status == 405 {
        headers.push_str("Allow: GET, PUT\r\n");
    }
    headers.push_str("\r\n");
    for bytes in [headers.as_bytes(), reply.body.as_bytes()] {
        let mut unwritten = bytes;
        while !unwritten.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "metadata response deadline exceeded");
            stream.set_write_timeout(Some(remaining))?;
            let count = stream.write(unwritten)?;
            anyhow::ensure!(count != 0, "metadata connection closed during response");
            unwritten = &unwritten[count..];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SandboxId;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn document(template: &str) -> SandboxMetadataDocument {
        SandboxMetadataDocument::new(SandboxId::from_uuid(uuid::Uuid::nil()), template)
    }

    #[test]
    fn cached_token_is_rejected_after_instance_replacement() {
        let now = Instant::now();
        let mut state = MetadataState::new("runtime-1", document("template-1"), now).unwrap();
        let old = state.issue_token(60, now).unwrap();
        assert_eq!(state.get(&old, "/envID", false, now).unwrap(), "template-1");
        state.replace("runtime-2", document("template-2")).unwrap();
        assert!(state.get(&old, "/envID", false, now).is_err());
        let new = state.issue_token(60, now).unwrap();
        assert_ne!(old, new);
        assert_eq!(state.get(&new, "/envID", false, now).unwrap(), "template-2");
    }

    #[test]
    fn expired_token_is_rejected() {
        let now = Instant::now();
        let state = MetadataState::new("runtime-1", document("template-1"), now).unwrap();
        let token = state.issue_token(2, now).unwrap();
        assert!(state.get(&token, "/", true, now).is_ok());
        assert!(state
            .get(&token, "/", true, now + std::time::Duration::from_secs(3))
            .is_err());
        assert!(state.issue_token(0, now).is_err());
        assert!(state.issue_token(21601, now).is_err());
    }

    #[tokio::test]
    async fn listener_stop_releases_namespace_reference() {
        struct Lease(Arc<AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let lease = Lease(released.clone());
        let state =
            MetadataState::new("runtime-1", document("template-1"), Instant::now()).unwrap();
        let mut listener = MetadataListener::start_with_setup(state, move || {
            Ok((TcpListener::bind("127.0.0.1:0")?, Box::new(lease)))
        })
        .unwrap();
        assert!(
            !released.load(Ordering::Acquire),
            "listener dropped namespace while still running"
        );
        listener.stop().await.unwrap();
        assert!(released.load(Ordering::Acquire));
        listener.stop().await.unwrap();
    }

    #[test]
    fn dropping_idle_listener_wakes_worker_and_releases_lease() {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let lease = Arc::new(());
        let released = Arc::downgrade(&lease);
        let state =
            MetadataState::new("runtime-1", document("template-1"), Instant::now()).unwrap();
        let listener =
            MetadataListener::start_with_setup(state, move || Ok((socket, Box::new(lease))))
                .unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(listener);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("idle metadata worker did not wake on drop");
        // Inspect this worker's lifetime: another parallel test may already
        // have rebound the port by the time Drop's join returns.
        assert!(released.upgrade().is_none());
    }

    #[test]
    fn exact_guest_source_requests_and_auth_errors() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/dragonball-m0/metadata.json"
        ))
        .unwrap();
        let now = Instant::now();
        let state = MetadataState::new("runtime-1", document("template-1"), now).unwrap();
        let token_reply = handle_http(
            &state,
            fixture["token_request"].as_str().unwrap().as_bytes(),
            now,
        );
        assert_eq!(token_reply.status, 200);
        let request = fixture["document_request"]
            .as_str()
            .unwrap()
            .replace("{TOKEN}", &token_reply.body);
        let reply = handle_http(&state, request.as_bytes(), now);
        assert_eq!(reply.status, 200);
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(json["envID"], "template-1");
        assert_eq!(
            handle_http(&state, b"GET / HTTP/1.1\r\nHost: metadata\r\n\r\n", now).status,
            401
        );
        assert_eq!(
            handle_http(&state, request.as_bytes(), now + Duration::from_secs(61)).status,
            401
        );
    }

    #[test]
    fn invalid_token_ttl_forwarded_requests_and_oversized_documents_are_rejected() {
        let now = Instant::now();
        let mut state = MetadataState::new("runtime-1", document("template-1"), now).unwrap();
        for ttl in ["0", "21601", "invalid"] {
            let request = format!(
                "PUT /latest/api/token HTTP/1.1\r\nX-metadata-token-ttl-seconds: {ttl}\r\n\r\n"
            );
            assert_eq!(handle_http(&state, request.as_bytes(), now).status, 400);
        }
        let request = b"PUT /latest/api/token HTTP/1.1\r\nX-metadata-token-ttl-seconds: 60\r\nX-Forwarded-For: 1.2.3.4\r\n\r\n";
        assert_eq!(handle_http(&state, request, now).status, 400);
        let old_token = state.issue_token(60, now).unwrap();
        let huge = document("too-large").with_extra(serde_json::Map::from_iter([(
            "large".into(),
            serde_json::json!("x".repeat(MAX_DOCUMENT_BYTES)),
        )]));
        assert!(state.replace("runtime-2", huge).is_err());
        assert_eq!(
            state.get(&old_token, "/envID", false, now).unwrap(),
            "template-1"
        );
        assert_eq!(
            handle_http(
                &state,
                b"GET / HTTP/1.1\r\nX-metadata-token: forged\r\n\r\n",
                now
            )
            .status,
            401
        );
    }

    #[tokio::test]
    async fn listener_serves_real_tcp_and_cancels_incomplete_connections() {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let state =
            MetadataState::new("runtime-1", document("template-1"), Instant::now()).unwrap();
        let mut listener =
            MetadataListener::start_with_setup(state, move || Ok((socket, Box::new(())))).unwrap();
        let response = |request: &[u8]| {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream.write_all(request).unwrap();
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            body
        };
        let token =
            response(b"PUT /latest/api/token HTTP/1.1\r\nX-metadata-token-ttl-seconds: 60\r\n\r\n");
        assert!(token.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(token.contains("x-metadata-token-ttl-seconds: 60\r\n"));
        let token = token.split_once("\r\n\r\n").unwrap().1;
        let get = format!("GET /envID HTTP/1.1\r\nX-metadata-token: {token}\r\n\r\n");
        assert!(response(get.as_bytes()).ends_with("template-1"));
        let requests = listener.recorded_requests().unwrap();
        assert_eq!(requests.len(), 2, "actual TCP requests must be recorded");
        let recording = serde_json::to_string(&requests).unwrap();
        assert!(
            !recording.contains(token),
            "metadata credential leaked in evidence"
        );
        assert_eq!(requests[0]["method"], "PUT");
        assert_eq!(requests[1]["headers"]["x-metadata-token"], "<redacted>");
        listener
            .replace("runtime-2", document("template-2"))
            .unwrap();
        assert!(response(get.as_bytes()).starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        let mut stalled = TcpStream::connect(address).unwrap();
        stalled.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        let start = Instant::now();
        listener.stop().await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(TcpStream::connect(address).is_err());
    }

    #[test]
    fn slow_drip_cannot_extend_listener_shutdown_deadline() {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let state = Mutex::new(
            MetadataState::new("runtime-1", document("template-1"), Instant::now()).unwrap(),
        );
        let worker = thread::spawn(move || {
            let (stream, _) = socket.accept().unwrap();
            let start = Instant::now();
            let _ = serve_connection(stream, &state);
            start.elapsed()
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"G").unwrap();
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(60));
            if client.write_all(b"x").is_err() {
                break;
            }
        }
        assert!(
            worker.join().unwrap() < Duration::from_millis(500),
            "per-read timeout permits unbounded drip traffic"
        );
    }
}
