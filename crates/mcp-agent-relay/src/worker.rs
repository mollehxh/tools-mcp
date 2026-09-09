use crate::protocol::{ConnectionId, compatible_registration};
use crate::protocol::{
    InvocationId, RELAY_PROTOCOL, RelayFrame, RelayPayload, payload_digest, valid_call_identity,
};
use mcp_agent_tool_contracts::{BackendError, CallContext, ToolBackend};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPeer {
    pub owner_id: String,
    pub device_id: String,
    pub certificate_fingerprint: String,
    pub platform: crate::Platform,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub generation: u64,
    pub ledger_capacity: usize,
    pub max_clock_skew: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            generation: 1,
            ledger_capacity: 1_024,
            max_clock_skew: Duration::from_secs(5),
        }
    }
}

pub struct RelayWorker<B> {
    peer: AuthenticatedPeer,
    backend: Arc<B>,
    config: WorkerConfig,
    state: Arc<Mutex<State>>,
}

impl<B> Clone for RelayWorker<B> {
    fn clone(&self) -> Self {
        Self {
            peer: self.peer.clone(),
            backend: Arc::clone(&self.backend),
            config: self.config.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

#[derive(Default)]
struct State {
    next_sequence: u64,
    connection_id: Option<ConnectionId>,
    active_generation: Option<u64>,
    registered: bool,
    seen: HashMap<InvocationId, String>,
    cancellations: HashMap<InvocationId, CancellationToken>,
}

impl<B: ToolBackend> RelayWorker<B> {
    #[must_use]
    pub fn new(peer: AuthenticatedPeer, backend: Arc<B>, config: WorkerConfig) -> Self {
        Self {
            peer,
            backend,
            config,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    #[must_use]
    pub fn peer(&self) -> &AuthenticatedPeer {
        &self.peer
    }

    pub(crate) fn activate_outbound(&self, connection_id: ConnectionId, generation: u64) -> bool {
        let mut state = self.lock_state();
        if generation == 0 || state.registered || state.connection_id.is_some() {
            return false;
        }
        state.connection_id = Some(connection_id);
        state.active_generation = Some(generation);
        state.next_sequence = 1;
        state.registered = true;
        true
    }

    pub(crate) fn cancel_active_calls(&self) {
        for cancellation in self.lock_state().cancellations.values() {
            cancellation.cancel();
        }
    }

    pub(crate) fn deactivate_outbound(&self) {
        let mut state = self.lock_state();
        for cancellation in state.cancellations.values() {
            cancellation.cancel();
        }
        state.cancellations.clear();
        state.connection_id = None;
        state.active_generation = None;
        state.next_sequence = 0;
        state.registered = false;
    }

    pub(crate) fn heartbeat_frame(&self, monotonic_millis: u64) -> Option<RelayFrame> {
        let state = self.lock_state();
        Some(RelayFrame {
            protocol: RELAY_PROTOCOL,
            connection_id: state.connection_id.clone()?,
            generation: state.active_generation?,
            sequence: 0,
            payload: RelayPayload::Heartbeat { monotonic_millis },
        })
    }

    pub async fn handle(&self, frame: RelayFrame) -> Option<RelayFrame> {
        let expected_generation = self
            .lock_state()
            .active_generation
            .unwrap_or(self.config.generation);
        if frame.protocol != RELAY_PROTOCOL || frame.generation != expected_generation {
            return Some(error_frame(
                &frame,
                None,
                "wrong_generation",
                "relay generation is not current",
                false,
            ));
        }
        {
            let mut state = self.lock_state();
            if state
                .connection_id
                .as_ref()
                .is_some_and(|connection_id| connection_id != &frame.connection_id)
            {
                return Some(error_frame(
                    &frame,
                    None,
                    "wrong_connection",
                    "relay connection identity changed",
                    false,
                ));
            }
            if frame.sequence != state.next_sequence {
                return Some(error_frame(
                    &frame,
                    None,
                    "out_of_order",
                    "relay sequence is not current",
                    false,
                ));
            }
            state.next_sequence = state.next_sequence.saturating_add(1);
        }
        match frame.payload.clone() {
            RelayPayload::Register { registration } => {
                Some(self.handle_registration(&frame, &registration))
            }
            RelayPayload::Call {
                invocation_id,
                deadline_unix_millis,
                payload_digest: claimed_digest,
                identity,
                request,
            } => Some(if self.lock_state().registered {
                self.handle_call(
                    &frame,
                    invocation_id,
                    deadline_unix_millis,
                    claimed_digest,
                    identity,
                    request,
                )
                .await
            } else {
                error_frame(
                    &frame,
                    Some(invocation_id),
                    "not_registered",
                    "worker must register before dispatch",
                    false,
                )
            }),
            RelayPayload::Cancel { invocation_id } => {
                if let Some(token) = self.lock_state().cancellations.get(&invocation_id).cloned() {
                    token.cancel();
                }
                None
            }
            RelayPayload::Heartbeat { monotonic_millis } => Some(response_frame(
                &frame,
                RelayPayload::Heartbeat { monotonic_millis },
            )),
            RelayPayload::Shutdown { .. } | RelayPayload::Revoked { .. } => {
                for token in self.lock_state().cancellations.values() {
                    token.cancel();
                }
                None
            }
            _ => Some(error_frame(
                &frame,
                None,
                "unexpected_frame",
                "worker received an invalid frame direction",
                false,
            )),
        }
    }

    fn handle_registration(
        &self,
        frame: &RelayFrame,
        registration: &crate::protocol::Register,
    ) -> RelayFrame {
        let valid = compatible_registration(registration, self.peer.platform);
        if !valid {
            return error_frame(
                frame,
                None,
                "incompatible_registration",
                "worker registration contract is incompatible",
                false,
            );
        }
        let mut state = self.lock_state();
        if state.registered {
            return error_frame(
                frame,
                None,
                "duplicate_registration",
                "worker connection is already registered",
                false,
            );
        }
        state.connection_id = Some(frame.connection_id.clone());
        state.registered = true;
        response_frame(frame, RelayPayload::Registered)
    }

    async fn handle_call(
        &self,
        frame: &RelayFrame,
        invocation_id: InvocationId,
        deadline_unix_millis: u64,
        claimed_digest: String,
        identity: mcp_agent_tool_contracts::CallIdentity,
        request: mcp_agent_tool_contracts::ToolRequest,
    ) -> RelayFrame {
        if claimed_digest != payload_digest(&request) {
            return error_frame(
                frame,
                Some(invocation_id),
                "payload_digest_mismatch",
                "relay payload digest is invalid",
                false,
            );
        }
        if expired(deadline_unix_millis, self.config.max_clock_skew) {
            return error_frame(
                frame,
                Some(invocation_id),
                "deadline_exceeded",
                "relay call deadline elapsed",
                false,
            );
        }
        if !valid_call_identity(&identity) {
            return error_frame(
                frame,
                Some(invocation_id),
                "invalid_call_identity",
                "relay call identity is invalid",
                false,
            );
        }
        let cancellation = CancellationToken::new();
        {
            let mut state = self.lock_state();
            if state.seen.contains_key(&invocation_id) {
                return error_frame(
                    frame,
                    Some(invocation_id),
                    "duplicate_invocation",
                    "relay invocation was already admitted",
                    false,
                );
            }
            if state.seen.len() >= self.config.ledger_capacity {
                return error_frame(
                    frame,
                    Some(invocation_id),
                    "disposition_ledger_full",
                    "relay disposition ledger is full; reconnect before dispatch",
                    false,
                );
            }
            state.seen.insert(invocation_id.clone(), claimed_digest);
            state
                .cancellations
                .insert(invocation_id.clone(), cancellation.clone());
        }
        let result = self
            .backend
            .call(
                CallContext::new(cancellation, None).with_identity(identity),
                request,
            )
            .await;
        self.lock_state().cancellations.remove(&invocation_id);
        match result {
            Ok(output) => response_frame(
                frame,
                RelayPayload::Result {
                    invocation_id,
                    output,
                },
            ),
            Err(error) => backend_error_frame(frame, invocation_id, &error),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn expired(deadline_unix_millis: u64, skew: Duration) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    now > u128::from(deadline_unix_millis).saturating_add(skew.as_millis())
}

fn response_frame(request: &RelayFrame, payload: RelayPayload) -> RelayFrame {
    RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id: request.connection_id.clone(),
        generation: request.generation,
        sequence: request.sequence,
        payload,
    }
}

fn error_frame(
    request: &RelayFrame,
    invocation_id: Option<InvocationId>,
    code: &str,
    message: &str,
    dispatched: bool,
) -> RelayFrame {
    response_frame(
        request,
        RelayPayload::Error {
            invocation_id,
            code: code.to_owned(),
            message: message.to_owned(),
            dispatched: Some(dispatched),
        },
    )
}

fn backend_error_frame(
    request: &RelayFrame,
    invocation_id: InvocationId,
    error: &BackendError,
) -> RelayFrame {
    error_frame(
        request,
        Some(invocation_id),
        error.code,
        &error.message,
        true,
    )
}
