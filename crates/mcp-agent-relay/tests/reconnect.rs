use futures_util::{SinkExt, StreamExt};
use mcp_agent_relay::{
    AuthenticatedPeer, ERROR_CONTRACT_VERSION, GatewaySocketConfig, Platform, RELAY_PROTOCOL,
    RELAY_SUBPROTOCOL, RESULT_CONTRACT_VERSION, ReconnectConfig, Register, RelayFrame,
    RelayPayload, RelayWorker, WorkerConfig, accept_gateway_authenticated, decode_frame,
    encode_frame, new_connection_id, run_reconnecting_outbound_worker, tool_schema_digest,
};
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, SkillListOutput, ToolBackend, ToolOutput, ToolRequest,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

const ALLOWED_MANIFEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

struct QuiescingBackend {
    accepted: tokio::sync::Notify,
    cleaned: AtomicBool,
}

impl ToolBackend for QuiescingBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        self.accepted.notify_one();
        Box::pin(async move {
            context.cancellation.cancelled().await;
            self.cleaned.store(true, Ordering::SeqCst);
            Err(BackendError::new("request_cancelled", "cancelled"))
        })
    }
}

fn skill_list_request() -> ToolRequest {
    ToolRequest::SkillsList(mcp_agent_tool_contracts::SkillListInput {
        scope: mcp_agent_tool_contracts::SkillScope::System,
        cursor: None,
    })
}

fn peer() -> AuthenticatedPeer {
    AuthenticatedPeer {
        owner_id: "owner".to_owned(),
        device_id: "device".to_owned(),
        certificate_fingerprint: "sha256:test".to_owned(),
        platform: Platform::Macos,
    }
}

fn registration() -> Register {
    Register {
        min_protocol: RELAY_PROTOCOL,
        max_protocol: RELAY_PROTOCOL,
        tool_schema_digest: tool_schema_digest(),
        result_contract_version: RESULT_CONTRACT_VERSION,
        error_contract_version: ERROR_CONTRACT_VERSION,
        system_skill_manifest_digest: ALLOWED_MANIFEST.to_owned(),
        platform: Platform::Macos,
        workspace_id: "workspace-test".to_owned(),
        containment_posture: "macos-seatbelt-verified".to_owned(),
        launch_instance_id: "immutable-launch".to_owned(),
        connection_epoch: 1,
    }
}

fn gateway_config() -> GatewaySocketConfig {
    GatewaySocketConfig::new([ALLOWED_MANIFEST.to_owned()]).unwrap()
}

async fn connect_ws(
    address: std::net::SocketAddr,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::Error,
> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    let mut request = format!("ws://{address}/relay").into_client_request()?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
    tokio_tungstenite::client_async(request, stream)
        .await
        .map(|(socket, _)| socket)
}

#[tokio::test]
async fn configured_manifest_allowlist_rejects_a_different_valid_digest() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_gateway_authenticated(
            stream,
            peer(),
            7,
            gateway_config(),
            CancellationToken::new(),
        )
        .await
    });
    let mut socket = connect_ws(address).await.unwrap();
    let mut registration = registration();
    registration.system_skill_manifest_digest = "f".repeat(64);
    let frame = RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id: new_connection_id(),
        generation: 0,
        sequence: 0,
        payload: RelayPayload::Register { registration },
    };
    socket
        .send(Message::Binary(encode_frame(&frame).unwrap().into()))
        .await
        .unwrap();

    assert!(matches!(
        server.await.unwrap(),
        Err(mcp_agent_relay::GatewaySocketError::IncompatibleRegistration)
    ));
}

#[tokio::test]
async fn registration_is_bounded_by_a_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = gateway_config().with_registration_timeout(Duration::from_millis(20));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_gateway_authenticated(stream, peer(), 7, config, CancellationToken::new()).await
    });
    let _socket = connect_ws(address).await.unwrap();
    assert!(matches!(
        server.await.unwrap(),
        Err(mcp_agent_relay::GatewaySocketError::RegistrationTimeout)
    ));
}

#[tokio::test]
async fn reconnect_keeps_launch_identity_increments_epoch_and_quiesces_attempts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let second_registered = Arc::new(tokio::sync::Notify::new());
    let quiescing_backend = Arc::new(QuiescingBackend {
        accepted: tokio::sync::Notify::new(),
        cleaned: AtomicBool::new(false),
    });
    let server_observed = Arc::clone(&observed);
    let server_second = Arc::clone(&second_registered);
    let server_backend = Arc::clone(&quiescing_backend);
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let (registration, remote) = accept_gateway_authenticated(
                stream,
                peer(),
                7 + attempt,
                gateway_config(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            server_observed.lock().unwrap().push((
                registration.launch_instance_id,
                registration.connection_epoch,
            ));
            if attempt == 0 {
                let call = tokio::spawn({
                    let remote = Arc::clone(&remote);
                    async move {
                        remote
                            .call(
                                CallContext::new(CancellationToken::new(), None),
                                skill_list_request(),
                            )
                            .await
                    }
                });
                tokio::time::timeout(Duration::from_secs(1), server_backend.accepted.notified())
                    .await
                    .unwrap();
                assert!(remote.shutdown("inject reconnect").await);
                let _ = call.await.unwrap();
            } else {
                assert!(
                    server_backend.cleaned.load(Ordering::SeqCst),
                    "the prior attempt must clean active work before reconnect"
                );
                server_second.notify_one();
                remote.wait_closed().await;
            }
        }
    });

    let cancellation = CancellationToken::new();
    let worker = RelayWorker::new(peer(), quiescing_backend, WorkerConfig::default());
    let client = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            run_reconnecting_outbound_worker(
                worker,
                registration(),
                ReconnectConfig::default()
                    .with_connect_timeout(Duration::from_secs(1))
                    .with_backoff(Duration::from_millis(1), Duration::from_millis(5)),
                cancellation,
                move || connect_ws(address),
            )
            .await
        }
    });

    tokio::time::timeout(Duration::from_secs(2), second_registered.notified())
        .await
        .unwrap();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), client)
        .await
        .expect("reconnecting client did not shut down deterministically")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("server-side socket tasks did not close")
        .unwrap();
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            ("immutable-launch".to_owned(), 1),
            ("immutable-launch".to_owned(), 2),
        ]
    );
}

#[tokio::test]
async fn real_socket_cancellation_emits_exactly_one_cancel_frame() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let gateway_cancellation = CancellationToken::new();
    let server_cancellation = gateway_cancellation.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, backend) =
            accept_gateway_authenticated(stream, peer(), 7, gateway_config(), server_cancellation)
                .await
                .unwrap();
        assert!(backend_sender.send(backend).is_ok());
    });
    let call_seen = Arc::new(tokio::sync::Notify::new());
    let cancels = Arc::new(AtomicUsize::new(0));
    let fake_call_seen = Arc::clone(&call_seen);
    let fake_cancels = Arc::clone(&cancels);
    let fake_worker = tokio::spawn(async move {
        let mut socket = connect_ws(address).await.unwrap();
        let connection_id = new_connection_id();
        socket
            .send(Message::Binary(
                encode_frame(&RelayFrame {
                    protocol: RELAY_PROTOCOL,
                    connection_id: connection_id.clone(),
                    generation: 0,
                    sequence: 0,
                    payload: RelayPayload::Register {
                        registration: registration(),
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let registered = socket.next().await.unwrap().unwrap().into_data();
        assert!(matches!(
            decode_frame(&registered).unwrap().payload,
            RelayPayload::Registered
        ));
        while let Some(message) = socket.next().await {
            let frame = decode_frame(&message.unwrap().into_data()).unwrap();
            match frame.payload {
                RelayPayload::Call { .. } => fake_call_seen.notify_one(),
                RelayPayload::Cancel { .. } => {
                    fake_cancels.fetch_add(1, Ordering::SeqCst);
                }
                RelayPayload::Shutdown { .. } => break,
                RelayPayload::Heartbeat { monotonic_millis } => {
                    socket
                        .send(Message::Binary(
                            encode_frame(&RelayFrame {
                                protocol: RELAY_PROTOCOL,
                                connection_id: connection_id.clone(),
                                generation: 7,
                                sequence: frame.sequence,
                                payload: RelayPayload::Heartbeat { monotonic_millis },
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
    });
    let backend = backend_receiver.await.unwrap();
    let call_cancellation = CancellationToken::new();
    let call = tokio::spawn({
        let backend = Arc::clone(&backend);
        let call_cancellation = call_cancellation.clone();
        async move {
            backend
                .call(
                    CallContext::new(call_cancellation, None),
                    skill_list_request(),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), call_seen.notified())
        .await
        .unwrap();
    call_cancellation.cancel();
    assert_eq!(call.await.unwrap().unwrap_err().code, "cancelled");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
    assert!(backend.shutdown("done").await);
    fake_worker.await.unwrap();
    gateway_cancellation.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn result_written_before_socket_loss_is_received_once_as_completed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, backend) = accept_gateway_authenticated(
            stream,
            peer(),
            7,
            gateway_config(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(backend_sender.send(backend).is_ok());
    });
    let fake_worker = tokio::spawn(async move {
        let mut socket = connect_ws(address).await.unwrap();
        let connection_id = new_connection_id();
        socket
            .send(Message::Binary(
                encode_frame(&RelayFrame {
                    protocol: RELAY_PROTOCOL,
                    connection_id: connection_id.clone(),
                    generation: 0,
                    sequence: 0,
                    payload: RelayPayload::Register {
                        registration: registration(),
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let _registered = socket.next().await.unwrap().unwrap();
        let call = decode_frame(&socket.next().await.unwrap().unwrap().into_data()).unwrap();
        let RelayPayload::Call { invocation_id, .. } = call.payload else {
            panic!("gateway did not dispatch a call")
        };
        socket
            .send(Message::Binary(
                encode_frame(&RelayFrame {
                    protocol: RELAY_PROTOCOL,
                    connection_id,
                    generation: 7,
                    sequence: call.sequence,
                    payload: RelayPayload::Result {
                        invocation_id,
                        output: ToolOutput::SkillsList(SkillListOutput {
                            skills: Vec::new(),
                            warnings: Vec::new(),
                            next_cursor: None,
                        }),
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    });
    let backend = backend_receiver.await.unwrap();
    let output = backend
        .call(
            CallContext::new(CancellationToken::new(), None),
            skill_list_request(),
        )
        .await
        .unwrap();
    assert!(matches!(output, ToolOutput::SkillsList(_)));
    fake_worker.await.unwrap();
    backend.wait_closed().await;
    server.await.unwrap();
}
