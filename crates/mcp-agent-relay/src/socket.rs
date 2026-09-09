use crate::{RELAY_SUBPROTOCOL, RelayWorker, decode_frame, encode_frame};
use futures_util::{SinkExt, StreamExt};
use mcp_agent_tool_contracts::ToolBackend;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_util::sync::CancellationToken;

const WRITER_QUEUE_CAPACITY: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("relay peer identity is not authenticated")]
    UnauthenticatedPeer,
    #[error("relay websocket handshake failed")]
    Handshake(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("relay websocket failed")]
    WebSocket(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("relay writer queue is closed")]
    WriterClosed,
}

/// Serves one WebSocket only after the TLS acceptor has supplied an authenticated peer.
///
/// # Errors
///
/// Returns an error for missing peer identity, subprotocol/upgrade failure, socket failure,
/// or writer shutdown.
pub async fn serve_authenticated<S, B>(
    stream: S,
    worker: RelayWorker<B>,
    cancellation: CancellationToken,
) -> Result<(), SocketError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: ToolBackend + 'static,
{
    if worker.peer().owner_id.is_empty()
        || worker.peer().device_id.is_empty()
        || worker.peer().certificate_fingerprint.is_empty()
    {
        return Err(SocketError::UnauthenticatedPeer);
    }
    let socket = accept_hdr_async(stream, require_subprotocol)
        .await
        .map_err(SocketError::Handshake)?;
    let (mut sink, mut source) = socket.split();
    let (writer, mut outbound) = mpsc::channel::<Message>(WRITER_QUEUE_CAPACITY);
    let writer_cancellation = cancellation.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = writer_cancellation.cancelled() => break,
                message = outbound.recv() => match message {
                    Some(message) => sink.send(message).await.map_err(SocketError::WebSocket)?,
                    None => break,
                }
            }
        }
        let _ = sink.close().await;
        Ok::<(), SocketError>(())
    });
    let mut calls = JoinSet::new();
    loop {
        let message = tokio::select! {
            () = cancellation.cancelled() => break,
            message = source.next() => match message {
                Some(Ok(message)) => message,
                Some(Err(error)) => return Err(SocketError::WebSocket(error)),
                None => break,
            }
        };
        let encoded = match message {
            Message::Binary(bytes) => bytes.to_vec(),
            Message::Text(text) => text.as_bytes().to_vec(),
            Message::Ping(bytes) => {
                writer
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| SocketError::WriterClosed)?;
                continue;
            }
            Message::Close(_) => break,
            Message::Pong(_) | Message::Frame(_) => continue,
        };
        let Ok(frame) = decode_frame(&encoded) else {
            break;
        };
        let worker = worker.clone();
        let writer = writer.clone();
        calls.spawn(async move {
            if let Some(response) = worker.handle(frame).await {
                let encoded = encode_frame(&response)?;
                writer
                    .send(Message::Binary(encoded.into()))
                    .await
                    .map_err(|_| SocketError::WriterClosed)?;
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    calls.abort_all();
    while calls.join_next().await.is_some() {}
    drop(writer);
    writer_task.await.map_err(|_| SocketError::WriterClosed)??;
    Ok(())
}

#[allow(clippy::result_large_err)] // tungstenite fixes the callback's response type.
pub(crate) fn require_subprotocol(
    request: &Request,
    mut response: Response,
) -> Result<Response, ErrorResponse> {
    let accepted = request
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim() == RELAY_SUBPROTOCOL)
        });
    if !accepted {
        return Err(tokio_tungstenite::tungstenite::http::Response::builder()
            .status(400)
            .body(Some("required relay subprotocol is missing".to_owned()))
            .expect("static handshake response is valid"));
    }
    response.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        RELAY_SUBPROTOCOL
            .parse()
            .expect("static subprotocol is valid"),
    );
    Ok(response)
}
