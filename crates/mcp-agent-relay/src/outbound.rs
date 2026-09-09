//! Worker-side loop for the required outbound-only local connection.

use crate::{
    RELAY_PROTOCOL, Register, RelayFrame, RelayPayload, RelayWorker, decode_frame, encode_frame,
    new_connection_id,
};
use futures_util::{SinkExt, StreamExt};
use mcp_agent_tool_contracts::ToolBackend;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const WRITER_QUEUE_CAPACITY: usize = 32;

/// Bounded retry policy for one immutable worker launch.
#[derive(Clone, Debug)]
pub struct ReconnectConfig {
    connect_timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl ReconnectConfig {
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    #[must_use]
    pub fn with_backoff(mut self, initial: Duration, maximum: Duration) -> Self {
        self.initial_backoff = initial.max(Duration::from_millis(1));
        self.max_backoff = maximum.max(self.initial_backoff);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutboundWorkerError {
    #[error("relay registration could not be encoded")]
    EncodeRegistration(#[source] crate::RelayError),
    #[error("relay websocket failed")]
    WebSocket(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("relay gateway rejected or malformed registration")]
    RegistrationRejected,
    #[error("relay worker instance was already activated")]
    AlreadyActivated,
    #[error("relay writer queue closed")]
    WriterClosed,
    #[error("relay websocket closed without a terminal frame")]
    ConnectionClosed,
    #[error("relay connection epoch must start above zero")]
    InvalidConnectionEpoch,
    #[error("relay connection epoch was exhausted")]
    ConnectionEpochExhausted,
}

/// Maintains one launch identity across bounded connection attempts until shutdown.
///
/// The registration is cloned per attempt. Its launch identity is immutable while the
/// connection epoch increases monotonically; each completed attempt is fully quiesced before
/// the connector is called again.
///
/// # Errors
///
/// Returns only for an unrecoverable epoch/worker error. Transient connection and socket errors
/// are retried until `cancellation` fires.
pub async fn run_reconnecting_outbound_worker<S, B, C, F, E>(
    worker: RelayWorker<B>,
    registration: Register,
    config: ReconnectConfig,
    cancellation: CancellationToken,
    mut connector: C,
) -> Result<(), OutboundWorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: ToolBackend + 'static,
    C: FnMut() -> F,
    F: Future<Output = Result<WebSocketStream<S>, E>>,
    E: std::fmt::Display,
{
    let launch_instance_id = registration.launch_instance_id.clone();
    if registration.connection_epoch == 0 {
        return Err(OutboundWorkerError::InvalidConnectionEpoch);
    }
    let mut epoch = registration.connection_epoch;
    let mut backoff = config.initial_backoff;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let connected = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = tokio::time::timeout(config.connect_timeout, connector()) => result,
        };
        let socket = match connected {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                eprintln!("tools-mcp relay connection failed: {error}");
                wait_to_retry(&cancellation, backoff).await?;
                backoff = next_backoff(backoff, config.max_backoff);
                continue;
            }
            Err(_) => {
                wait_to_retry(&cancellation, backoff).await?;
                backoff = next_backoff(backoff, config.max_backoff);
                continue;
            }
        };
        let mut attempt_registration = registration.clone();
        attempt_registration
            .launch_instance_id
            .clone_from(&launch_instance_id);
        attempt_registration.connection_epoch = epoch;
        let attempt_cancellation = cancellation.child_token();
        let result = run_outbound_worker(
            socket,
            worker.clone(),
            attempt_registration,
            attempt_cancellation,
        )
        .await;
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if matches!(result, Err(OutboundWorkerError::AlreadyActivated)) {
            return result;
        }
        backoff = if result.is_ok() {
            config.initial_backoff
        } else {
            next_backoff(backoff, config.max_backoff)
        };
        epoch = epoch
            .checked_add(1)
            .ok_or(OutboundWorkerError::ConnectionEpochExhausted)?;
        wait_to_retry(&cancellation, backoff).await?;
    }
}

async fn wait_to_retry(
    cancellation: &CancellationToken,
    delay: Duration,
) -> Result<(), OutboundWorkerError> {
    tokio::select! {
        () = cancellation.cancelled() => Ok(()),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

/// Registers a worker through an already-authenticated outbound WebSocket and serves calls.
///
/// # Errors
///
/// Returns an error on registration rejection, connection loss, or bounded-writer failure.
pub async fn run_outbound_worker<S, B>(
    mut socket: WebSocketStream<S>,
    worker: RelayWorker<B>,
    registration: Register,
    cancellation: CancellationToken,
) -> Result<(), OutboundWorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: ToolBackend + 'static,
{
    let connection_id = new_connection_id();
    let registration_frame = RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id: connection_id.clone(),
        generation: 0,
        sequence: 0,
        payload: RelayPayload::Register { registration },
    };
    socket
        .send(Message::Binary(
            encode_frame(&registration_frame)
                .map_err(OutboundWorkerError::EncodeRegistration)?
                .into(),
        ))
        .await
        .map_err(OutboundWorkerError::WebSocket)?;
    let response = socket
        .next()
        .await
        .ok_or(OutboundWorkerError::RegistrationRejected)?
        .map_err(OutboundWorkerError::WebSocket)?;
    let frame = decode_frame(&response.into_data())
        .map_err(|_| OutboundWorkerError::RegistrationRejected)?;
    if frame.connection_id != connection_id
        || frame.generation == 0
        || frame.sequence != 0
        || !matches!(frame.payload, RelayPayload::Registered)
    {
        return Err(OutboundWorkerError::RegistrationRejected);
    }
    if !worker.activate_outbound(connection_id, frame.generation) {
        return Err(OutboundWorkerError::AlreadyActivated);
    }
    let result = serve_calls(socket, worker.clone(), cancellation).await;
    worker.deactivate_outbound();
    result
}

async fn serve_calls<S, B>(
    socket: WebSocketStream<S>,
    worker: RelayWorker<B>,
    cancellation: CancellationToken,
) -> Result<(), OutboundWorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: ToolBackend + 'static,
{
    let (mut sink, mut source) = socket.split();
    let (writer, mut outbound) = mpsc::channel::<Message>(WRITER_QUEUE_CAPACITY);
    let writer_cancellation = cancellation.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = writer_cancellation.cancelled() => break,
                message = outbound.recv() => match message {
                    Some(message) => sink.send(message).await.map_err(OutboundWorkerError::WebSocket)?,
                    None => break,
                }
            }
        }
        let _ = sink.close().await;
        Ok::<(), OutboundWorkerError>(())
    });
    let heartbeat_task = spawn_heartbeats(writer.clone(), worker.clone(), cancellation.clone());
    let mut calls = JoinSet::new();
    let last_gateway_contact = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let watchdog = spawn_watchdog(Arc::clone(&last_gateway_contact), cancellation.clone());
    let loop_result = loop {
        let message = tokio::select! {
            () = cancellation.cancelled() => break Ok(()),
            message = source.next() => match message {
                Some(Ok(message)) => message,
                Some(Err(error)) => break Err(OutboundWorkerError::WebSocket(error)),
                None if cancellation.is_cancelled() => break Ok(()),
                None => break Err(OutboundWorkerError::ConnectionClosed),
            }
        };
        if message.is_close() {
            break Ok(());
        }
        if message.is_ping() {
            if writer
                .send(Message::Pong(message.into_data()))
                .await
                .is_err()
            {
                break Err(OutboundWorkerError::WriterClosed);
            }
            continue;
        }
        if !(message.is_binary() || message.is_text()) {
            continue;
        }
        let Ok(frame) = decode_frame(&message.into_data()) else {
            break Ok(());
        };
        *last_gateway_contact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
        if matches!(frame.payload, RelayPayload::Heartbeat { .. }) {
            continue;
        }
        if matches!(
            frame.payload,
            RelayPayload::Shutdown { .. } | RelayPayload::Revoked { .. }
        ) {
            let _ = worker.handle(frame).await;
            break Ok(());
        }
        let worker = worker.clone();
        let writer = writer.clone();
        calls.spawn(async move {
            if let Some(response) = worker.handle(frame).await {
                let encoded = encode_frame(&response)?;
                writer
                    .send(Message::Binary(encoded.into()))
                    .await
                    .map_err(|_| OutboundWorkerError::WriterClosed)?;
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
    };
    cancellation.cancel();
    worker.cancel_active_calls();
    if tokio::time::timeout(Duration::from_secs(1), async {
        while calls.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        calls.abort_all();
        while calls.join_next().await.is_some() {}
    }
    drop(writer);
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    watchdog.abort();
    let _ = watchdog.await;
    let writer_result = writer_task
        .await
        .map_err(|_| OutboundWorkerError::WriterClosed)?;
    loop_result?;
    writer_result
}

fn spawn_heartbeats<B: ToolBackend + 'static>(
    writer: mpsc::Sender<Message>,
    worker: RelayWorker<B>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = interval.tick() => {
                    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let Some(frame) = worker.heartbeat_frame(millis) else { break };
                    let Ok(encoded) = encode_frame(&frame) else { break };
                    if writer.send(Message::Binary(encoded.into())).await.is_err() { break }
                }
            }
        }
    })
}

fn spawn_watchdog(
    contact: Arc<std::sync::Mutex<std::time::Instant>>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            if cancellation.is_cancelled() {
                break;
            }
            let elapsed = contact
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .elapsed();
            if elapsed >= std::time::Duration::from_secs(4) {
                cancellation.cancel();
                break;
            }
        }
    })
}
