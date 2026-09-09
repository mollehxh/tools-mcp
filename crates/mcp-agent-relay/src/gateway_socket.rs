//! Gateway-side relay admission and a remote [`ToolBackend`] implementation.

use crate::protocol::compatible_registration;
use crate::socket::require_subprotocol;
use crate::{
    AuthenticatedPeer, ConnectionId, InvocationId, RELAY_PROTOCOL, Register, RelayFrame,
    RelayPayload, decode_frame, encode_frame, new_invocation_id, payload_digest,
};
use futures_util::{SinkExt, StreamExt};
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, CallIdentity, ToolBackend, ToolOutput, ToolRequest,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;

const WRITER_QUEUE_CAPACITY: usize = 32;
const MAX_PENDING_CALLS: usize = 64;

/// Per-connection relay admission and resource policy.
#[derive(Clone, Debug)]
pub struct GatewaySocketConfig {
    allowed_system_skill_manifest_digests: Arc<HashSet<String>>,
    handshake_timeout: Duration,
    registration_timeout: Duration,
    writer_queue_capacity: usize,
    max_pending_calls: usize,
}

impl GatewaySocketConfig {
    /// Builds a fail-closed policy from the exact system-skill manifests this gateway serves.
    ///
    /// # Errors
    ///
    /// Returns an error when the allowlist is empty or contains a non-SHA-256 digest.
    pub fn new(
        digests: impl IntoIterator<Item = String>,
    ) -> Result<Self, GatewaySocketConfigError> {
        let digests = digests.into_iter().collect::<HashSet<_>>();
        if digests.is_empty() {
            return Err(GatewaySocketConfigError::EmptyManifestAllowlist);
        }
        if digests.iter().any(|digest| !valid_sha256_digest(digest)) {
            return Err(GatewaySocketConfigError::InvalidManifestDigest);
        }
        Ok(Self {
            allowed_system_skill_manifest_digests: Arc::new(digests),
            handshake_timeout: Duration::from_secs(5),
            registration_timeout: Duration::from_secs(5),
            writer_queue_capacity: WRITER_QUEUE_CAPACITY,
            max_pending_calls: MAX_PENDING_CALLS,
        })
    }

    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_registration_timeout(mut self, timeout: Duration) -> Self {
        self.registration_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_writer_queue_capacity(mut self, capacity: usize) -> Self {
        self.writer_queue_capacity = capacity.max(1);
        self
    }

    #[must_use]
    pub fn with_max_pending_calls(mut self, capacity: usize) -> Self {
        self.max_pending_calls = capacity.max(1);
        self
    }

    fn accepts_manifest(&self, digest: &str) -> bool {
        self.allowed_system_skill_manifest_digests.contains(digest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GatewaySocketConfigError {
    #[error("relay system-skill manifest allowlist must not be empty")]
    EmptyManifestAllowlist,
    #[error("relay system-skill manifest allowlist contains an invalid SHA-256 digest")]
    InvalidManifestDigest,
}

type PendingResult = Result<ToolOutput, BackendError>;

struct Outbound {
    frame: RelayFrame,
    written: oneshot::Sender<bool>,
}

struct ReaderState {
    writer: mpsc::Sender<Outbound>,
    pending: Arc<Mutex<HashMap<InvocationId, oneshot::Sender<PendingResult>>>>,
    expected_connection: ConnectionId,
    generation: u64,
    connection_lost: CancellationToken,
    heartbeat_counter: Arc<AtomicU64>,
    heartbeat_notify: Arc<Notify>,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewaySocketError {
    #[error("relay websocket handshake timed out")]
    HandshakeTimeout,
    #[error("relay websocket handshake failed")]
    Handshake(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("relay registration frame is missing or malformed")]
    InvalidRegistration,
    #[error("relay worker registration is incompatible")]
    IncompatibleRegistration,
    #[error("relay websocket failed during registration")]
    RegistrationSocket(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("relay registration timed out")]
    RegistrationTimeout,
}

/// A gateway backend whose calls cross one authenticated, bounded relay connection.
pub struct RemoteBackend {
    peer: AuthenticatedPeer,
    connection_id: ConnectionId,
    generation: u64,
    next_sequence: AtomicU64,
    writer: mpsc::Sender<Outbound>,
    pending: Arc<Mutex<HashMap<InvocationId, oneshot::Sender<PendingResult>>>>,
    connection_lost: CancellationToken,
    heartbeat_counter: Arc<AtomicU64>,
    heartbeat_notify: Arc<Notify>,
    closed: watch::Receiver<bool>,
    max_pending_calls: usize,
}

impl RemoteBackend {
    #[must_use]
    pub fn peer(&self) -> &AuthenticatedPeer {
        &self.peer
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn connection_lost(&self) -> CancellationToken {
        self.connection_lost.clone()
    }

    #[must_use]
    pub fn heartbeat_count(&self) -> u64 {
        self.heartbeat_counter.load(Ordering::Acquire)
    }

    pub async fn wait_for_heartbeat_after(&self, observed: u64) -> Option<u64> {
        loop {
            let notified = self.heartbeat_notify.notified();
            let current = self.heartbeat_count();
            if current > observed {
                return Some(current);
            }
            tokio::select! {
                () = self.connection_lost.cancelled() => return None,
                () = notified => {}
            }
        }
    }

    /// Waits until both socket halves and their pending calls have terminated.
    pub async fn wait_closed(&self) {
        let mut closed = self.closed.clone();
        while !*closed.borrow() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }

    /// Sends a terminal revocation frame once, then closes and awaits the socket tasks.
    pub async fn revoke(&self, reason: impl Into<String>) -> bool {
        self.close_with(RelayPayload::Revoked {
            reason: reason.into(),
        })
        .await
    }

    /// Sends a graceful shutdown frame once, then closes and awaits the socket tasks.
    pub async fn shutdown(&self, reason: impl Into<String>) -> bool {
        self.close_with(RelayPayload::Shutdown {
            reason: reason.into(),
        })
        .await
    }

    async fn close_with(&self, payload: RelayPayload) -> bool {
        if self.connection_lost.is_cancelled() {
            self.wait_closed().await;
            return false;
        }
        let (written, receipt) = oneshot::channel();
        let queued = self
            .writer
            .try_send(Outbound {
                frame: self.next_frame(payload),
                written,
            })
            .is_ok();
        let sent = if queued {
            tokio::time::timeout(std::time::Duration::from_secs(1), receipt)
                .await
                .is_ok_and(|result| result == Ok(true))
        } else {
            false
        };
        self.connection_lost.cancel();
        self.wait_closed().await;
        sent
    }

    fn lock_pending(
        &self,
    ) -> MutexGuard<'_, HashMap<InvocationId, oneshot::Sender<PendingResult>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_frame(&self, payload: RelayPayload) -> RelayFrame {
        RelayFrame {
            protocol: RELAY_PROTOCOL,
            connection_id: self.connection_id.clone(),
            generation: self.generation,
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
            payload,
        }
    }
}

impl ToolBackend for RemoteBackend {
    fn call(&self, context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(BackendError::new(
                    "cancelled",
                    "relay call was cancelled before dispatch",
                ));
            }
            if self.connection_lost.is_cancelled() {
                return Err(not_dispatched("relay connection is unavailable"));
            }
            let invocation_id = new_invocation_id();
            let (result_sender, result_receiver) = oneshot::channel();
            {
                let mut pending = self.lock_pending();
                if pending.len() >= self.max_pending_calls {
                    return Err(not_dispatched("relay pending-call budget is exhausted"));
                }
                pending.insert(invocation_id.clone(), result_sender);
            }
            let deadline_unix_millis = deadline_unix_millis(context.deadline);
            let identity = context.identity.unwrap_or_else(|| CallIdentity {
                principal_fingerprint: "local-anonymous".to_owned(),
                session_fingerprint: "local-default".to_owned(),
            });
            let frame = self.next_frame(RelayPayload::Call {
                invocation_id: invocation_id.clone(),
                deadline_unix_millis,
                payload_digest: payload_digest(&request),
                identity,
                request,
            });
            let (written_sender, written_receiver) = oneshot::channel();
            if self
                .writer
                .try_send(Outbound {
                    frame,
                    written: written_sender,
                })
                .is_err()
            {
                self.lock_pending().remove(&invocation_id);
                return Err(not_dispatched("relay writer queue is unavailable"));
            }
            if written_receiver.await != Ok(true) {
                self.lock_pending().remove(&invocation_id);
                return Err(outcome_unknown(
                    "relay frame outcome is unknown after writer handoff",
                ));
            }
            tokio::select! {
                biased;
                result = result_receiver => result.unwrap_or_else(|_| Err(outcome_unknown("relay result was lost"))),
                () = context.cancellation.cancelled() => {
                    self.lock_pending().remove(&invocation_id);
                    let (written, _) = oneshot::channel();
                    let _ = self.writer.try_send(Outbound {
                        frame: self.next_frame(RelayPayload::Cancel { invocation_id }),
                        written,
                    });
                    Err(BackendError::new("cancelled", "relay call was cancelled"))
                }
                () = self.connection_lost.cancelled() => {
                    self.lock_pending().remove(&invocation_id);
                    Err(outcome_unknown("relay disconnected after dispatch"))
                }
            }
        })
    }
}

/// Accepts a WebSocket from an already-mTLS-authenticated worker and publishes its backend.
///
/// # Errors
///
/// Returns an error unless the first frame is a compatible registration for the generation.
pub async fn accept_gateway_authenticated<S>(
    stream: S,
    peer: AuthenticatedPeer,
    generation: u64,
    config: GatewaySocketConfig,
    cancellation: CancellationToken,
) -> Result<(Register, Arc<RemoteBackend>), GatewaySocketError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(crate::MAX_FRAME_BYTES))
        .max_frame_size(Some(crate::MAX_FRAME_BYTES));
    let mut socket = tokio::time::timeout(
        config.handshake_timeout,
        accept_hdr_async_with_config(stream, require_subprotocol, Some(websocket_config)),
    )
    .await
    .map_err(|_| GatewaySocketError::HandshakeTimeout)?
    .map_err(GatewaySocketError::Handshake)?;
    let message = tokio::time::timeout(config.registration_timeout, socket.next())
        .await
        .map_err(|_| GatewaySocketError::RegistrationTimeout)?
        .ok_or(GatewaySocketError::InvalidRegistration)?
        .map_err(GatewaySocketError::RegistrationSocket)?;
    let bytes = message_bytes(message).ok_or(GatewaySocketError::InvalidRegistration)?;
    let frame = decode_frame(&bytes).map_err(|_| GatewaySocketError::InvalidRegistration)?;
    let RelayPayload::Register { registration } = frame.payload.clone() else {
        return Err(GatewaySocketError::InvalidRegistration);
    };
    if frame.generation != 0
        || generation == 0
        || frame.sequence != 0
        || !compatible_registration(&registration, peer.platform)
        || !config.accepts_manifest(&registration.system_skill_manifest_digest)
    {
        return Err(GatewaySocketError::IncompatibleRegistration);
    }
    let registered = RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id: frame.connection_id.clone(),
        generation,
        sequence: 0,
        payload: RelayPayload::Registered,
    };
    socket
        .send(Message::Binary(
            encode_frame(&registered)
                .map_err(|_| GatewaySocketError::InvalidRegistration)?
                .into(),
        ))
        .await
        .map_err(GatewaySocketError::RegistrationSocket)?;

    let (sink, source) = socket.split();
    let (writer, outbound) = mpsc::channel::<Outbound>(config.writer_queue_capacity);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let connection_lost = cancellation.child_token();
    let heartbeat_counter = Arc::new(AtomicU64::new(0));
    let heartbeat_notify = Arc::new(Notify::new());
    let (closed_sender, closed) = watch::channel(false);
    let remote = Arc::new(RemoteBackend {
        peer,
        connection_id: frame.connection_id.clone(),
        generation,
        next_sequence: AtomicU64::new(1),
        writer,
        pending: Arc::clone(&pending),
        connection_lost: connection_lost.clone(),
        heartbeat_counter: Arc::clone(&heartbeat_counter),
        heartbeat_notify: Arc::clone(&heartbeat_notify),
        closed,
        max_pending_calls: config.max_pending_calls,
    });

    let writer_task = spawn_writer(sink, outbound, connection_lost.clone());
    let expected_connection = frame.connection_id;
    let reader_task = spawn_reader(
        source,
        ReaderState {
            writer: remote.writer.clone(),
            pending: Arc::clone(&pending),
            expected_connection,
            generation,
            connection_lost,
            heartbeat_counter,
            heartbeat_notify,
        },
    );
    tokio::spawn(async move {
        let _ = tokio::join!(writer_task, reader_task);
        let _ = closed_sender.send(true);
    });
    Ok((registration, remote))
}

fn spawn_writer<S>(
    mut sink: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut outbound: mpsc::Receiver<Outbound>,
    connection_lost: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let outbound = tokio::select! {
                () = connection_lost.cancelled() => break,
                outbound = outbound.recv() => match outbound {
                    Some(outbound) => outbound,
                    None => break,
                }
            };
            let Ok(encoded) = encode_frame(&outbound.frame) else {
                let _ = outbound.written.send(false);
                continue;
            };
            let sent = sink.send(Message::Binary(encoded.into())).await.is_ok();
            let _ = outbound.written.send(sent);
            if !sent {
                break;
            }
        }
        connection_lost.cancel();
        let _ = sink.close().await;
    })
}

fn spawn_reader<S>(
    mut source: futures_util::stream::SplitStream<WebSocketStream<S>>,
    state: ReaderState,
) -> tokio::task::JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                () = state.connection_lost.cancelled() => break,
                message = source.next() => match message {
                    Some(Ok(message)) => message,
                    _ => break,
                }
            };
            let Some(bytes) = message_bytes(message) else {
                continue;
            };
            let Ok(frame) = decode_frame(&bytes) else {
                break;
            };
            if frame.connection_id != state.expected_connection
                || frame.generation != state.generation
            {
                break;
            }
            let response_identity = (
                frame.connection_id.clone(),
                frame.generation,
                frame.sequence,
            );
            match frame.payload {
                RelayPayload::Result {
                    invocation_id,
                    output,
                } => {
                    if let Some(sender) = lock_map(&state.pending).remove(&invocation_id) {
                        let _ = sender.send(Ok(output));
                    }
                }
                RelayPayload::Error {
                    invocation_id: Some(invocation_id),
                    message,
                    ..
                } => {
                    if let Some(sender) = lock_map(&state.pending).remove(&invocation_id) {
                        let _ = sender.send(Err(BackendError::new("remote_error", message)));
                    }
                }
                RelayPayload::Heartbeat { monotonic_millis } => {
                    state.heartbeat_counter.fetch_add(1, Ordering::Release);
                    state.heartbeat_notify.notify_waiters();
                    let (written, _) = oneshot::channel();
                    let _ = state.writer.try_send(Outbound {
                        frame: RelayFrame {
                            protocol: RELAY_PROTOCOL,
                            connection_id: response_identity.0,
                            generation: response_identity.1,
                            sequence: response_identity.2,
                            payload: RelayPayload::Heartbeat { monotonic_millis },
                        },
                        written,
                    });
                }
                RelayPayload::Registered => {}
                _ => break,
            }
        }
        state.connection_lost.cancel();
        lock_map(&state.pending).clear();
    })
}

fn message_bytes(message: Message) -> Option<Vec<u8>> {
    match message {
        Message::Binary(bytes) => Some(bytes.to_vec()),
        Message::Text(text) => Some(text.as_bytes().to_vec()),
        _ => None,
    }
}

fn deadline_unix_millis(deadline: Option<std::time::Instant>) -> u64 {
    let remaining = deadline.map_or(std::time::Duration::from_mins(5), |deadline| {
        deadline.saturating_duration_since(std::time::Instant::now())
    });
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(remaining)
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn lock_map<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn not_dispatched(message: &str) -> BackendError {
    BackendError::new("not_dispatched", message)
}

fn outcome_unknown(message: &str) -> BackendError {
    BackendError::new("outcome_unknown", message)
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{RemoteBackend, outcome_unknown};
    use crate::{AuthenticatedPeer, Platform, new_connection_id};
    use mcp_agent_tool_contracts::{
        CallContext, SkillListInput, SkillScope, ToolBackend, ToolRequest,
    };
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Notify, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    fn request() -> ToolRequest {
        ToolRequest::SkillsList(SkillListInput {
            scope: SkillScope::System,
            cursor: None,
        })
    }

    #[tokio::test]
    async fn full_writer_queue_rejects_before_handoff_and_dropped_receipt_is_unknown() {
        let (writer, mut outbound) = mpsc::channel(1);
        let (_, closed) = watch::channel(false);
        let backend = Arc::new(RemoteBackend {
            peer: AuthenticatedPeer {
                owner_id: "owner".to_owned(),
                device_id: "device".to_owned(),
                certificate_fingerprint: "fingerprint".to_owned(),
                platform: Platform::Macos,
            },
            connection_id: new_connection_id(),
            generation: 7,
            next_sequence: AtomicU64::new(1),
            writer,
            pending: Arc::new(Mutex::new(HashMap::new())),
            connection_lost: CancellationToken::new(),
            heartbeat_counter: Arc::new(AtomicU64::new(0)),
            heartbeat_notify: Arc::new(Notify::new()),
            closed,
            max_pending_calls: 64,
        });
        let first = tokio::spawn({
            let backend = Arc::clone(&backend);
            async move {
                backend
                    .call(CallContext::new(CancellationToken::new(), None), request())
                    .await
            }
        });
        while outbound.len() != 1 {
            tokio::task::yield_now().await;
        }
        let full = backend
            .call(CallContext::new(CancellationToken::new(), None), request())
            .await
            .unwrap_err();
        assert_eq!(full.code, "not_dispatched");

        let handed_off = outbound.recv().await.unwrap();
        drop(handed_off.written);
        let ambiguous = first.await.unwrap().unwrap_err();
        assert_eq!(ambiguous.code, outcome_unknown("ignored").code);
    }

    #[tokio::test]
    async fn pending_call_budget_rejects_before_writer_handoff() {
        let (writer, mut outbound) = mpsc::channel(2);
        let (_, closed) = watch::channel(false);
        let backend = Arc::new(RemoteBackend {
            peer: AuthenticatedPeer {
                owner_id: "owner".to_owned(),
                device_id: "device".to_owned(),
                certificate_fingerprint: "fingerprint".to_owned(),
                platform: Platform::Macos,
            },
            connection_id: new_connection_id(),
            generation: 7,
            next_sequence: AtomicU64::new(1),
            writer,
            pending: Arc::new(Mutex::new(HashMap::new())),
            connection_lost: CancellationToken::new(),
            heartbeat_counter: Arc::new(AtomicU64::new(0)),
            heartbeat_notify: Arc::new(Notify::new()),
            closed,
            max_pending_calls: 1,
        });
        let first = tokio::spawn({
            let backend = Arc::clone(&backend);
            async move {
                backend
                    .call(CallContext::new(CancellationToken::new(), None), request())
                    .await
            }
        });
        while outbound.len() != 1 {
            tokio::task::yield_now().await;
        }
        let rejected = backend
            .call(CallContext::new(CancellationToken::new(), None), request())
            .await
            .unwrap_err();
        assert_eq!(rejected.code, "not_dispatched");
        assert_eq!(outbound.len(), 1, "rejected call must not reach the writer");
        let handed_off = outbound.recv().await.unwrap();
        drop(handed_off.written);
        assert_eq!(first.await.unwrap().unwrap_err().code, "outcome_unknown");
    }
}
