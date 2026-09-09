use futures_util::{SinkExt, StreamExt};
use mcp_agent_relay::{
    AuthenticatedPeer, ConnectionId, ERROR_CONTRACT_VERSION, GatewaySocketConfig, MAX_FRAME_BYTES,
    Platform, RELAY_PROTOCOL, RELAY_SUBPROTOCOL, RESULT_CONTRACT_VERSION, Register, RelayFrame,
    RelayPayload, RelayWorker, WorkerConfig, accept_gateway_authenticated, decode_frame,
    encode_frame, new_connection_id, new_invocation_id, payload_digest, run_outbound_worker,
    serve_authenticated, tool_schema_digest,
};
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, CallIdentity, SkillListInput, SkillListOutput,
    SkillScope, ToolBackend, ToolOutput, ToolRequest,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

struct CountingBackend {
    calls: AtomicUsize,
}

struct IdentityBackend {
    identity: std::sync::Mutex<Option<CallIdentity>>,
}

impl ToolBackend for IdentityBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        *self.identity.lock().unwrap() = context.identity;
        Box::pin(async {
            Ok(ToolOutput::SkillsList(SkillListOutput {
                skills: Vec::new(),
                warnings: Vec::new(),
                next_cursor: None,
            }))
        })
    }
}

impl ToolBackend for CountingBackend {
    fn call(&self, _context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ToolOutput::SkillsList(SkillListOutput {
                skills: Vec::new(),
                warnings: Vec::new(),
                next_cursor: None,
            }))
        })
    }
}

struct CancelBackend;

impl ToolBackend for CancelBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            context.cancellation.cancelled().await;
            Err(BackendError::new("request_cancelled", "cancelled"))
        })
    }
}

struct BlockingBackend {
    calls: AtomicUsize,
    accepted: Notify,
}

impl ToolBackend for BlockingBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.accepted.notify_one();
        Box::pin(async move {
            context.cancellation.cancelled().await;
            Err(BackendError::new("request_cancelled", "cancelled"))
        })
    }
}

fn request() -> ToolRequest {
    ToolRequest::SkillsList(SkillListInput {
        scope: SkillScope::System,
        cursor: None,
    })
}

fn call_identity() -> CallIdentity {
    CallIdentity {
        principal_fingerprint: "principal-test".to_owned(),
        session_fingerprint: "session-test".to_owned(),
    }
}

fn call_frame(
    connection_id: ConnectionId,
    sequence: u64,
    invocation_id: mcp_agent_relay::InvocationId,
) -> RelayFrame {
    let request = request();
    RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id,
        generation: 7,
        sequence,
        payload: RelayPayload::Call {
            invocation_id,
            deadline_unix_millis: u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
                + 30_000,
            payload_digest: payload_digest(&request),
            identity: call_identity(),
            request,
        },
    }
}

#[tokio::test]
async fn authenticated_call_identity_reaches_the_worker_backend() {
    let backend = Arc::new(IdentityBackend {
        identity: std::sync::Mutex::new(None),
    });
    let worker = RelayWorker::new(
        peer(),
        Arc::clone(&backend),
        WorkerConfig {
            generation: 7,
            ..WorkerConfig::default()
        },
    );
    let connection_id = new_connection_id();
    worker
        .handle(register_frame(connection_id.clone()))
        .await
        .unwrap();
    worker
        .handle(call_frame(connection_id, 1, new_invocation_id()))
        .await
        .unwrap();

    assert_eq!(*backend.identity.lock().unwrap(), Some(call_identity()));
}

fn register_frame(connection_id: ConnectionId) -> RelayFrame {
    RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id,
        generation: 7,
        sequence: 0,
        payload: RelayPayload::Register {
            registration: registration(),
        },
    }
}

fn registration() -> Register {
    Register {
        min_protocol: RELAY_PROTOCOL,
        max_protocol: RELAY_PROTOCOL,
        tool_schema_digest: tool_schema_digest(),
        result_contract_version: RESULT_CONTRACT_VERSION,
        error_contract_version: ERROR_CONTRACT_VERSION,
        system_skill_manifest_digest: "0".repeat(64),
        platform: Platform::Macos,
        workspace_id: "workspace-test".to_owned(),
        containment_posture: "macos-seatbelt-verified".to_owned(),
        launch_instance_id: "launch-a".to_owned(),
        connection_epoch: 1,
    }
}

fn peer() -> AuthenticatedPeer {
    AuthenticatedPeer {
        owner_id: "owner".to_owned(),
        device_id: "device".to_owned(),
        certificate_fingerprint: "sha256:test".to_owned(),
        platform: Platform::Macos,
    }
}

fn gateway_config() -> GatewaySocketConfig {
    GatewaySocketConfig::new(["0".repeat(64)]).unwrap()
}

#[test]
fn codec_is_bounded_versioned_and_contract_digest_is_stable() {
    let frame = call_frame(new_connection_id(), 0, new_invocation_id());
    let encoded = encode_frame(&frame).unwrap();
    assert_eq!(decode_frame(&encoded).unwrap(), frame);
    assert_eq!(tool_schema_digest().len(), 64);
    assert!(decode_frame(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    let mut incompatible = frame;
    incompatible.protocol += 1;
    assert!(decode_frame(&serde_json::to_vec(&incompatible).unwrap()).is_err());
}

#[tokio::test]
async fn duplicate_invocation_never_dispatches_twice() {
    let backend = Arc::new(CountingBackend {
        calls: AtomicUsize::new(0),
    });
    let worker = RelayWorker::new(
        peer(),
        Arc::clone(&backend),
        WorkerConfig {
            generation: 7,
            ..WorkerConfig::default()
        },
    );
    let connection_id = new_connection_id();
    assert!(matches!(
        worker
            .handle(register_frame(connection_id.clone()))
            .await
            .unwrap()
            .payload,
        RelayPayload::Registered
    ));
    let invocation = new_invocation_id();
    let first = worker
        .handle(call_frame(connection_id.clone(), 1, invocation.clone()))
        .await
        .unwrap();
    assert!(matches!(first.payload, RelayPayload::Result { .. }));
    let second = worker
        .handle(call_frame(connection_id, 2, invocation))
        .await
        .unwrap();
    assert!(
        matches!(second.payload, RelayPayload::Error { ref code, dispatched: Some(false), .. } if code == "duplicate_invocation")
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn full_disposition_ledger_fails_closed_without_forgetting_duplicates() {
    let backend = Arc::new(CountingBackend {
        calls: AtomicUsize::new(0),
    });
    let worker = RelayWorker::new(
        peer(),
        Arc::clone(&backend),
        WorkerConfig {
            generation: 7,
            ledger_capacity: 1,
            ..WorkerConfig::default()
        },
    );
    let connection_id = new_connection_id();
    worker
        .handle(register_frame(connection_id.clone()))
        .await
        .unwrap();
    let first_invocation = new_invocation_id();
    let first = worker
        .handle(call_frame(
            connection_id.clone(),
            1,
            first_invocation.clone(),
        ))
        .await
        .unwrap();
    assert!(matches!(first.payload, RelayPayload::Result { .. }));

    let full = worker
        .handle(call_frame(connection_id.clone(), 2, new_invocation_id()))
        .await
        .unwrap();
    assert!(
        matches!(full.payload, RelayPayload::Error { ref code, dispatched: Some(false), .. } if code == "disposition_ledger_full")
    );
    let duplicate = worker
        .handle(call_frame(connection_id, 3, first_invocation))
        .await
        .unwrap();
    assert!(
        matches!(duplicate.payload, RelayPayload::Error { ref code, dispatched: Some(false), .. } if code == "duplicate_invocation")
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn incompatible_registration_contracts_never_become_eligible() {
    let mut incompatible = Vec::new();
    let mut protocol = registration();
    protocol.min_protocol = RELAY_PROTOCOL + 1;
    incompatible.push(protocol);
    let mut schema = registration();
    schema.tool_schema_digest = "f".repeat(64);
    incompatible.push(schema);
    let mut result = registration();
    result.result_contract_version += 1;
    incompatible.push(result);
    let mut error = registration();
    error.error_contract_version += 1;
    incompatible.push(error);
    let mut manifest = registration();
    manifest.system_skill_manifest_digest = "invalid".to_owned();
    incompatible.push(manifest);
    let mut platform = registration();
    platform.platform = Platform::Windows;
    platform.containment_posture = "windows-restricted-token-job-verified".to_owned();
    incompatible.push(platform);
    let mut containment = registration();
    containment.containment_posture = "rootless-podman-verified".to_owned();
    incompatible.push(containment);

    for registration in incompatible {
        let worker = RelayWorker::new(
            peer(),
            Arc::new(CountingBackend {
                calls: AtomicUsize::new(0),
            }),
            WorkerConfig {
                generation: 7,
                ..WorkerConfig::default()
            },
        );
        let response = worker
            .handle(RelayFrame {
                protocol: RELAY_PROTOCOL,
                connection_id: new_connection_id(),
                generation: 7,
                sequence: 0,
                payload: RelayPayload::Register { registration },
            })
            .await
            .unwrap();
        assert!(matches!(response.payload, RelayPayload::Error { .. }));
    }
}

#[tokio::test]
async fn stale_modified_expired_and_out_of_order_calls_fail_without_dispatch() {
    let cases = [
        "wrong_generation",
        "modified_digest",
        "invalid_identity",
        "expired",
        "out_of_order",
    ];
    for case in cases {
        let backend = Arc::new(CountingBackend {
            calls: AtomicUsize::new(0),
        });
        let worker = RelayWorker::new(
            peer(),
            Arc::clone(&backend),
            WorkerConfig {
                generation: 7,
                max_clock_skew: Duration::ZERO,
                ..WorkerConfig::default()
            },
        );
        let connection_id = new_connection_id();
        worker
            .handle(register_frame(connection_id.clone()))
            .await
            .unwrap();
        let mut frame = call_frame(connection_id, 1, new_invocation_id());
        match case {
            "wrong_generation" => frame.generation += 1,
            "modified_digest" => {
                let RelayPayload::Call {
                    ref mut payload_digest,
                    ..
                } = frame.payload
                else {
                    unreachable!()
                };
                *payload_digest = "f".repeat(64);
            }
            "invalid_identity" => {
                let RelayPayload::Call {
                    ref mut identity, ..
                } = frame.payload
                else {
                    unreachable!()
                };
                identity.session_fingerprint.clear();
            }
            "expired" => {
                let RelayPayload::Call {
                    ref mut deadline_unix_millis,
                    ..
                } = frame.payload
                else {
                    unreachable!()
                };
                *deadline_unix_millis = 0;
            }
            "out_of_order" => frame.sequence = 2,
            _ => unreachable!(),
        }
        let response = worker.handle(frame).await.unwrap();
        assert!(
            matches!(
                response.payload,
                RelayPayload::Error {
                    dispatched: Some(false),
                    ..
                }
            ),
            "{case} must be rejected before dispatch"
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0, "case={case}");
    }
}

#[tokio::test]
async fn cancel_frame_reaches_the_admitted_backend() {
    let worker = RelayWorker::new(
        peer(),
        Arc::new(CancelBackend),
        WorkerConfig {
            generation: 7,
            ..WorkerConfig::default()
        },
    );
    let connection_id = new_connection_id();
    assert!(matches!(
        worker
            .handle(register_frame(connection_id.clone()))
            .await
            .unwrap()
            .payload,
        RelayPayload::Registered
    ));
    let invocation = new_invocation_id();
    let call = call_frame(connection_id.clone(), 1, invocation.clone());
    let running = tokio::spawn({
        let worker = worker.clone();
        async move { worker.handle(call).await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        worker
            .handle(RelayFrame {
                protocol: RELAY_PROTOCOL,
                connection_id,
                generation: 7,
                sequence: 2,
                payload: RelayPayload::Cancel {
                    invocation_id: invocation
                }
            })
            .await
            .is_none()
    );
    let response = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(response.payload, RelayPayload::Error { ref code, dispatched: Some(true), .. } if code == "request_cancelled")
    );
}

#[tokio::test]
async fn authenticated_loopback_websocket_negotiates_and_registers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let backend = Arc::new(CountingBackend {
                calls: AtomicUsize::new(0),
            });
            let worker = RelayWorker::new(
                peer(),
                backend,
                WorkerConfig {
                    generation: 7,
                    ..WorkerConfig::default()
                },
            );
            serve_authenticated(stream, worker, cancellation).await
        }
    });
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = format!("ws://{address}/relay")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("Sec-WebSocket-Protocol").unwrap(),
        RELAY_SUBPROTOCOL
    );
    let registration = register_frame(new_connection_id());
    socket
        .send(Message::Binary(encode_frame(&registration).unwrap().into()))
        .await
        .unwrap();
    let response = socket.next().await.unwrap().unwrap().into_data();
    assert!(matches!(
        decode_frame(&response).unwrap().payload,
        RelayPayload::Registered
    ));
    socket.close(None).await.unwrap();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn gateway_remote_backend_dispatches_within_idle_budget_and_shuts_down() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, backend) =
                accept_gateway_authenticated(stream, peer(), 7, gateway_config(), cancellation)
                    .await?;
            assert!(backend_sender.send(backend).is_ok());
            Ok::<(), mcp_agent_relay::GatewaySocketError>(())
        }
    });
    let worker_cancellation = cancellation.clone();
    let worker = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut request = format!("ws://{address}/relay")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let RelayPayload::Register { registration } = register_frame(new_connection_id()).payload
        else {
            unreachable!()
        };
        let local = RelayWorker::new(
            peer(),
            Arc::new(CountingBackend {
                calls: AtomicUsize::new(0),
            }),
            WorkerConfig {
                generation: 7,
                ..WorkerConfig::default()
            },
        );
        run_outbound_worker(socket, local, registration, worker_cancellation).await
    });
    let backend = backend_receiver.await.unwrap();
    let mut latency = Vec::new();
    for _ in 0..32 {
        let started = std::time::Instant::now();
        let output = backend
            .call(CallContext::new(CancellationToken::new(), None), request())
            .await
            .unwrap();
        assert!(matches!(output, ToolOutput::SkillsList(_)));
        latency.push(started.elapsed());
    }
    latency.sort_unstable();
    let p95 = latency[(latency.len() * 95).div_ceil(100) - 1];
    assert!(p95 < Duration::from_millis(100), "idle relay p95={p95:?}");
    assert!(backend.shutdown("test shutdown").await);
    tokio::time::timeout(Duration::from_secs(1), backend.wait_closed())
        .await
        .expect("gateway socket tasks did not terminate");
    worker.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_before_writer_handoff_never_dispatches() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, backend) =
            accept_gateway_authenticated(stream, peer(), 7, gateway_config(), server_cancellation)
                .await?;
        assert!(backend_sender.send(backend).is_ok());
        Ok::<(), mcp_agent_relay::GatewaySocketError>(())
    });
    let calls = Arc::new(CountingBackend {
        calls: AtomicUsize::new(0),
    });
    let worker_cancellation = cancellation.clone();
    let worker_calls = Arc::clone(&calls);
    let worker = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut request = format!("ws://{address}/relay")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let local = RelayWorker::new(
            peer(),
            worker_calls,
            WorkerConfig {
                generation: 7,
                ..WorkerConfig::default()
            },
        );
        run_outbound_worker(socket, local, registration(), worker_cancellation).await
    });
    let backend = backend_receiver.await.unwrap();
    let call_cancellation = CancellationToken::new();
    call_cancellation.cancel();
    let error = backend
        .call(CallContext::new(call_cancellation, None), request())
        .await
        .unwrap_err();
    assert_eq!(error.code, "cancelled");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(calls.calls.load(Ordering::SeqCst), 0);

    cancellation.cancel();
    worker.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn loss_before_writer_handoff_is_known_non_dispatch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, backend) =
            accept_gateway_authenticated(stream, peer(), 7, gateway_config(), server_cancellation)
                .await?;
        assert!(backend_sender.send(backend).is_ok());
        Ok::<(), mcp_agent_relay::GatewaySocketError>(())
    });
    let worker_cancellation = cancellation.child_token();
    let stop_worker = worker_cancellation.clone();
    let worker = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut request = format!("ws://{address}/relay")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let local = RelayWorker::new(
            peer(),
            Arc::new(CountingBackend {
                calls: AtomicUsize::new(0),
            }),
            WorkerConfig {
                generation: 7,
                ..WorkerConfig::default()
            },
        );
        run_outbound_worker(socket, local, registration(), worker_cancellation).await
    });
    let backend = backend_receiver.await.unwrap();
    stop_worker.cancel();
    tokio::time::timeout(
        Duration::from_secs(1),
        backend.connection_lost().cancelled(),
    )
    .await
    .unwrap();
    let error = backend
        .call(CallContext::new(CancellationToken::new(), None), request())
        .await
        .unwrap_err();
    assert_eq!(error.code, "not_dispatched");

    cancellation.cancel();
    worker.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn loss_after_worker_acceptance_and_mutation_is_unknown_and_never_replayed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let gateway_cancellation = CancellationToken::new();
    let (backend_sender, backend_receiver) = tokio::sync::oneshot::channel();
    let server_cancellation = gateway_cancellation.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, backend) =
            accept_gateway_authenticated(stream, peer(), 7, gateway_config(), server_cancellation)
                .await?;
        assert!(backend_sender.send(backend).is_ok());
        Ok::<(), mcp_agent_relay::GatewaySocketError>(())
    });
    let worker_cancellation = CancellationToken::new();
    let stop_worker = worker_cancellation.clone();
    let calls = Arc::new(BlockingBackend {
        calls: AtomicUsize::new(0),
        accepted: Notify::new(),
    });
    let worker_calls = Arc::clone(&calls);
    let worker = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut request = format!("ws://{address}/relay")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let local = RelayWorker::new(
            peer(),
            worker_calls,
            WorkerConfig {
                generation: 7,
                ..WorkerConfig::default()
            },
        );
        run_outbound_worker(socket, local, registration(), worker_cancellation).await
    });
    let backend = backend_receiver.await.unwrap();
    let call = tokio::spawn({
        let backend = Arc::clone(&backend);
        async move {
            backend
                .call(CallContext::new(CancellationToken::new(), None), request())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), calls.accepted.notified())
        .await
        .unwrap();
    stop_worker.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "outcome_unknown");
    assert_eq!(calls.calls.load(Ordering::SeqCst), 1);

    gateway_cancellation.cancel();
    backend.wait_closed().await;
    worker.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_manifest_digest_never_becomes_eligible() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_gateway_authenticated(stream, peer(), 7, gateway_config(), cancellation).await
        }
    });
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = format!("ws://{address}/relay")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    let mut registration = registration();
    registration.system_skill_manifest_digest = "not-a-sha256-digest".to_owned();
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
