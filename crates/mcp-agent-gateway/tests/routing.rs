use mcp_agent_gateway::{BackendKind, GatewayBackend, Observability, RouteContext, StateStore};
use mcp_agent_tool_contracts::{
    BackendFuture, CallContext, CallIdentity, ExecCommandInput, ExecCommandOutput, ListedSkill,
    SkillAuthority, SkillListInput, SkillListOutput, SkillReadInput, SkillReadOutput, SkillScope,
    SkillSource, SkillSourceKind, ToolBackend, ToolOutput, ToolRequest, WriteStdinInput,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct FakeBackend {
    name: &'static str,
    calls: Mutex<Vec<String>>,
}

struct BlockingBackend {
    name: &'static str,
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct CancellationAwareBackend {
    started: tokio::sync::Notify,
}

impl CancellationAwareBackend {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
        }
    }
}

impl ToolBackend for CancellationAwareBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            self.started.notify_one();
            context.cancellation.cancelled().await;
            Err(mcp_agent_tool_contracts::BackendError::new(
                "cancelled",
                "authority cancellation reached backend",
            ))
        })
    }
}

impl BlockingBackend {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl ToolBackend for BlockingBackend {
    fn call(&self, _context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(ToolOutput::ExecCommand(output(self.name, None)))
        })
    }
}

impl FakeBackend {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl ToolBackend for FakeBackend {
    fn call(&self, _context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            match request {
                ToolRequest::ExecCommand(input) => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("exec:{}", input.cmd));
                    Ok(ToolOutput::ExecCommand(output(self.name, Some(1_000))))
                }
                ToolRequest::WriteStdin(input) => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("stdin:{}", input.session_id));
                    Ok(ToolOutput::WriteStdin(output(
                        self.name,
                        Some(input.session_id),
                    )))
                }
                ToolRequest::SkillsList(input) => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("list:{:?}", input.cursor));
                    Ok(ToolOutput::SkillsList(SkillListOutput {
                        skills: if input.cursor.is_none() {
                            vec![ListedSkill {
                                authority: SkillAuthority::Host,
                                scope: SkillScope::Project,
                                package: "native-package".to_owned(),
                                name: "native".to_owned(),
                                description: "test".to_owned(),
                                main_resource: "skill://host/project/native-package/SKILL.md"
                                    .to_owned(),
                                source: SkillSource {
                                    kind: SkillSourceKind::Host,
                                    repository: None,
                                    commit: None,
                                    selector: None,
                                },
                            }]
                        } else {
                            Vec::new()
                        },
                        warnings: Vec::new(),
                        next_cursor: input.cursor.is_none().then(|| "native-next".to_owned()),
                    }))
                }
                ToolRequest::SkillsRead(input) => {
                    self.calls.lock().unwrap().push(format!(
                        "read:{}:{}:{:?}",
                        input.package, input.resource, input.cursor
                    ));
                    Ok(ToolOutput::SkillsRead(SkillReadOutput {
                        resource: input.resource,
                        contents: self.name.to_owned(),
                        next_cursor: Some("native-read-next".to_owned()),
                    }))
                }
                ToolRequest::ApplyPatch(_) => Ok(ToolOutput::ExecCommand(output(self.name, None))),
                ToolRequest::TerminateSession(input) => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("terminate:{}", input.session_id));
                    Ok(ToolOutput::TerminateSession(
                        mcp_agent_tool_contracts::TerminateSessionOutput { terminated: true },
                    ))
                }
            }
        })
    }
}

fn output(name: &str, session_id: Option<i32>) -> ExecCommandOutput {
    ExecCommandOutput {
        chunk_id: None,
        wall_time_seconds: 0.0,
        exit_code: Some(0),
        session_id,
        original_token_count: None,
        output: name.to_owned(),
    }
}

fn route(kind: BackendKind, workspace: &str, generation: u64) -> RouteContext {
    RouteContext {
        kind,
        workspace_id: workspace.to_owned(),
        generation,
        operating_system: if kind == BackendKind::Vps {
            "linux"
        } else {
            "macos"
        }
        .to_owned(),
        privilege_posture: if kind == BackendKind::Vps {
            "container-root"
        } else {
            "native-sandbox"
        }
        .to_owned(),
    }
}

fn context(session: &str) -> CallContext {
    CallContext::new(CancellationToken::new(), None).with_identity(CallIdentity {
        principal_fingerprint: "owner".to_owned(),
        session_fingerprint: session.to_owned(),
    })
}

fn other_principal_context(session: &str) -> CallContext {
    CallContext::new(CancellationToken::new(), None).with_identity(CallIdentity {
        principal_fingerprint: "other-owner".to_owned(),
        session_fingerprint: session.to_owned(),
    })
}

fn exec(cmd: &str) -> ToolRequest {
    ToolRequest::ExecCommand(ExecCommandInput {
        cmd: cmd.to_owned(),
        workdir: None,
        tty: false,
        yield_time_ms: 10,
        max_output_tokens: None,
        shell: None,
        login: None,
    })
}

async fn confirm_call(gateway: &GatewayBackend, session: &str, request: ToolRequest) -> ToolOutput {
    let first = gateway
        .call(context(session), request.clone())
        .await
        .unwrap_err();
    assert_eq!(first.code, "backend_changed");
    gateway.call(context(session), request).await.unwrap()
}

#[tokio::test]
async fn principal_revocation_cancels_inflight_calls_and_fences_future_use() {
    let backend = Arc::new(CancellationAwareBackend::new());
    let gateway = Arc::new(GatewayBackend::new(
        backend.clone(),
        route(BackendKind::Vps, "vps-workspace", 1),
        Duration::from_secs(1),
    ));
    let first = gateway
        .call(context("revoked-chat"), exec("long-running"))
        .await
        .unwrap_err();
    assert_eq!(first.code, "backend_changed");

    let active_gateway = Arc::clone(&gateway);
    let active = tokio::spawn(async move {
        active_gateway
            .call(context("revoked-chat"), exec("long-running"))
            .await
    });
    backend.started.notified().await;
    gateway.revoke_principal("owner").await.unwrap();
    let error = active.await.unwrap().unwrap_err();
    assert_eq!(error.code, "cancelled");

    let rejected = gateway
        .call(context("revoked-chat"), exec("after-revoke"))
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "authority_revoked");
}

#[tokio::test]
async fn principal_reconciliation_emits_revocation_only_once() {
    let telemetry = Arc::new(Observability::test_instance());
    let gateway = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps-workspace", 1),
        Duration::from_secs(1),
    )
    .with_observability(Arc::clone(&telemetry));
    let error = gateway
        .call(context("chat"), exec("pwd"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "backend_changed");

    gateway
        .reconcile_principals(&HashSet::default())
        .await
        .unwrap();
    gateway
        .reconcile_principals(&HashSet::default())
        .await
        .unwrap();

    let revocations = telemetry
        .recent_events()
        .into_iter()
        .filter(|event| event.contains("\"event\":\"principal_revoked\""))
        .count();
    assert_eq!(revocations, 1);
}

#[tokio::test]
async fn principal_revocation_terminates_every_published_native_terminal() {
    let backend = Arc::new(FakeBackend::new("vps"));
    let gateway = GatewayBackend::new(
        backend.clone(),
        route(BackendKind::Vps, "vps-workspace", 1),
        Duration::from_secs(1),
    );
    let output = confirm_call(&gateway, "terminal-chat", exec("sleep 30")).await;
    assert!(matches!(output, ToolOutput::ExecCommand(_)));

    gateway.revoke_principal("owner").await.unwrap();
    assert!(
        backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "terminate:1000")
    );
}

#[tokio::test]
async fn newest_launch_wins_and_expiry_falls_back_with_visible_fences() {
    let vps = Arc::new(FakeBackend::new("vps"));
    let gateway = GatewayBackend::new(
        vps,
        route(BackendKind::Vps, "vps-workspace", 1),
        Duration::from_millis(40),
    );
    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("one")).await),
        "vps"
    );
    let local_a = Arc::new(FakeBackend::new("local-a"));
    let lease_a = gateway
        .register_local(
            "launch-a",
            1,
            local_a,
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();
    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("two")).await),
        "local-a"
    );
    let local_b = Arc::new(FakeBackend::new("local-b"));
    let lease_b = gateway
        .register_local(
            "launch-b",
            1,
            local_b,
            route(BackendKind::Local, "project-b", 0),
        )
        .unwrap();
    assert!(lease_a.is_fenced());
    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("three")).await),
        "local-b"
    );
    for epoch in 2..=4 {
        assert_eq!(
            gateway
                .register_local(
                    "launch-a",
                    epoch,
                    Arc::new(FakeBackend::new("stale")),
                    route(BackendKind::Local, "stale", 0),
                )
                .unwrap_err()
                .code,
            "launch_superseded"
        );
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("four")).await),
        "vps"
    );
    assert!(lease_b.is_fenced());
    assert_eq!(gateway.heartbeat(&lease_b).unwrap_err().code, "lease_lost");
    assert_eq!(
        gateway
            .register_local(
                "launch-b",
                2,
                Arc::new(FakeBackend::new("expired-b")),
                route(BackendKind::Local, "project-b", 0),
            )
            .unwrap_err()
            .code,
        "launch_superseded"
    );
}

#[tokio::test]
async fn current_launch_reconnect_replaces_the_dead_backend() {
    let gateway = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    );
    let first_lease = gateway
        .register_local(
            "launch-a",
            1,
            Arc::new(FakeBackend::new("dead-a")),
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();
    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("before-reconnect")).await),
        "dead-a"
    );

    let resumed_lease = gateway
        .register_local(
            "launch-a",
            2,
            Arc::new(FakeBackend::new("resumed-a")),
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();
    assert!(first_lease.is_fenced());
    assert!(resumed_lease.generation > first_lease.generation);
    assert!(!gateway.expire_local_lease(&first_lease));
    assert!(!resumed_lease.is_fenced());

    for stale_epoch in [1, 2] {
        assert_eq!(
            gateway
                .register_local(
                    "launch-a",
                    stale_epoch,
                    Arc::new(FakeBackend::new("stale-a")),
                    route(BackendKind::Local, "project-a", 0),
                )
                .unwrap_err()
                .code,
            "stale_connection_epoch"
        );
    }

    assert_eq!(
        extract_output(confirm_call(&gateway, "chat", exec("after-reconnect")).await),
        "resumed-a"
    );
}

#[tokio::test]
async fn superseded_launch_remains_rejected_after_gateway_restart() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    {
        let store = Arc::new(StateStore::open(&database, &watermark).unwrap());
        let gateway = GatewayBackend::new(
            Arc::new(FakeBackend::new("vps")),
            route(BackendKind::Vps, "vps", 1),
            Duration::from_secs(1),
        )
        .with_state_store(store);
        gateway
            .register_local(
                "launch-a",
                1,
                Arc::new(FakeBackend::new("local-a")),
                route(BackendKind::Local, "project-a", 0),
            )
            .unwrap();
        gateway
            .register_local(
                "launch-b",
                1,
                Arc::new(FakeBackend::new("local-b")),
                route(BackendKind::Local, "project-b", 0),
            )
            .unwrap();
    }

    let store = Arc::new(StateStore::open(&database, &watermark).unwrap());
    let restarted = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    )
    .with_state_store(store);
    assert_eq!(
        restarted
            .register_local(
                "launch-a",
                2,
                Arc::new(FakeBackend::new("stale-a")),
                route(BackendKind::Local, "project-a", 0),
            )
            .unwrap_err()
            .code,
        "launch_superseded"
    );
}

#[tokio::test]
async fn racing_launches_have_one_takeover_linearization_point() {
    let gateway = Arc::new(GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    ));
    let local_a = Arc::new(FakeBackend::new("local-a"));
    let local_b = Arc::new(FakeBackend::new("local-b"));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let register_a = tokio::task::spawn_blocking({
        let gateway = gateway.clone();
        let backend = local_a.clone();
        let barrier = barrier.clone();
        move || {
            barrier.wait();
            gateway.register_local(
                "launch-a",
                1,
                backend,
                route(BackendKind::Local, "project-a", 0),
            )
        }
    });
    let register_b = tokio::task::spawn_blocking({
        let gateway = gateway.clone();
        let backend = local_b.clone();
        move || {
            barrier.wait();
            gateway.register_local(
                "launch-b",
                1,
                backend,
                route(BackendKind::Local, "project-b", 0),
            )
        }
    });
    let (lease_a, lease_b) = tokio::join!(register_a, register_b);
    let lease_a = lease_a.unwrap().unwrap();
    let lease_b = lease_b.unwrap().unwrap();
    assert_ne!(lease_a.is_fenced(), lease_b.is_fenced());

    let output = extract_output(confirm_call(&gateway, "race-launches", exec("only-once")).await);
    let (winner, loser) = if lease_a.is_fenced() {
        ("local-b", &local_a)
    } else {
        ("local-a", &local_b)
    };
    assert_eq!(output, winner);
    assert!(loser.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn route_fences_block_concurrent_calls_on_every_default_route_change() {
    let vps = Arc::new(FakeBackend::new("vps"));
    let local = Arc::new(FakeBackend::new("local"));
    let gateway = Arc::new(GatewayBackend::new(
        vps.clone(),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_millis(30),
    ));
    confirm_call(&gateway, "race-chat", exec("initial-vps")).await;
    gateway
        .register_local(
            "launch-a",
            1,
            local.clone(),
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();

    let (first, second) = tokio::join!(
        gateway.call(context("race-chat"), exec("local-one")),
        gateway.call(context("race-chat"), exec("local-two")),
    );
    assert_eq!(first.unwrap_err().code, "backend_changed");
    assert_eq!(second.unwrap_err().code, "backend_changed");
    assert!(local.calls.lock().unwrap().is_empty());
    gateway
        .call(context("race-chat"), exec("local-retry"))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(40)).await;
    let vps_calls_before = vps.calls.lock().unwrap().len();
    let (first, second) = tokio::join!(
        gateway.call(context("race-chat"), exec("vps-one")),
        gateway.call(context("race-chat"), exec("vps-two")),
    );
    assert_eq!(first.unwrap_err().code, "backend_changed");
    assert_eq!(second.unwrap_err().code, "backend_changed");
    assert_eq!(vps.calls.lock().unwrap().len(), vps_calls_before);
    gateway
        .call(context("race-chat"), exec("vps-retry"))
        .await
        .unwrap();
}

#[tokio::test]
async fn both_backends_unavailable_fail_without_dispatch_or_host_fallback() {
    let vps = Arc::new(FakeBackend::new("vps"));
    let gateway = GatewayBackend::new(
        vps.clone(),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_millis(20),
    );
    assert!(gateway.fence_vps_generation(1));
    assert_eq!(
        gateway
            .call(context("offline"), exec("must-not-run"))
            .await
            .unwrap_err()
            .code,
        "no_backend"
    );
    assert!(vps.calls.lock().unwrap().is_empty());

    let local = Arc::new(FakeBackend::new("local"));
    let lease = gateway
        .register_local(
            "launch-a",
            1,
            local.clone(),
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();
    assert_eq!(
        extract_output(confirm_call(&gateway, "online", exec("local-only")).await),
        "local"
    );
    assert!(gateway.expire_local_lease(&lease));
    assert_eq!(
        gateway
            .call(context("offline-again"), exec("must-not-run"))
            .await
            .unwrap_err()
            .code,
        "no_backend"
    );
    assert_eq!(local.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn delayed_result_from_a_fenced_generation_is_unknown_and_never_replayed() {
    let delayed = Arc::new(BlockingBackend::new("late-a"));
    let replacement = Arc::new(FakeBackend::new("local-b"));
    let gateway = Arc::new(GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    ));
    gateway
        .register_local(
            "launch-a",
            1,
            delayed.clone(),
            route(BackendKind::Local, "project-a", 0),
        )
        .unwrap();
    assert_eq!(
        gateway
            .call(context("loss-chat"), exec("mutate"))
            .await
            .unwrap_err()
            .code,
        "backend_changed"
    );
    let in_flight = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.call(context("loss-chat"), exec("mutate")).await }
    });
    tokio::time::timeout(Duration::from_secs(1), delayed.started.notified())
        .await
        .unwrap();
    gateway
        .register_local(
            "launch-b",
            1,
            replacement.clone(),
            route(BackendKind::Local, "project-b", 0),
        )
        .unwrap();
    delayed.release.notify_waiters();

    let error = in_flight.await.unwrap().unwrap_err();
    assert_eq!(error.code, "backend_lost");
    assert_eq!(error.details.unwrap()["outcome"], "unknown");
    assert_eq!(delayed.calls.load(Ordering::SeqCst), 1);
    assert!(replacement.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn terminal_ids_are_public_and_lost_generations_never_alias() {
    let gateway = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    );
    gateway
        .register_local(
            "launch-a",
            1,
            Arc::new(FakeBackend::new("a")),
            route(BackendKind::Local, "a", 0),
        )
        .unwrap();
    let output = confirm_call(&gateway, "terminal-chat", exec("yield")).await;
    let public_id = match output {
        ToolOutput::ExecCommand(output) => output.session_id.unwrap(),
        _ => panic!("expected exec output"),
    };
    assert_eq!(public_id, 1_000);
    gateway
        .register_local(
            "launch-b",
            1,
            Arc::new(FakeBackend::new("b")),
            route(BackendKind::Local, "b", 0),
        )
        .unwrap();
    let error = gateway
        .call(
            context("terminal-chat"),
            ToolRequest::WriteStdin(WriteStdinInput {
                session_id: public_id,
                chars: String::new(),
                yield_time_ms: 10,
                max_output_tokens: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "lost_context");
}

#[tokio::test]
async fn vps_terminal_affinity_survives_local_activation_and_is_principal_bound() {
    let vps = Arc::new(FakeBackend::new("vps"));
    let gateway = GatewayBackend::new(
        vps,
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    );
    let public_id = match confirm_call(&gateway, "terminal-chat", exec("yield")).await {
        ToolOutput::ExecCommand(output) => output.session_id.unwrap(),
        _ => panic!("expected exec output"),
    };
    gateway
        .register_local(
            "launch-a",
            1,
            Arc::new(FakeBackend::new("local")),
            route(BackendKind::Local, "local", 0),
        )
        .unwrap();

    let continued = gateway
        .call(
            context("terminal-chat"),
            ToolRequest::WriteStdin(WriteStdinInput {
                session_id: public_id,
                chars: String::new(),
                yield_time_ms: 10,
                max_output_tokens: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(extract_output(continued), "vps");
    let denied = gateway
        .call(
            other_principal_context("other-terminal-chat"),
            ToolRequest::WriteStdin(WriteStdinInput {
                session_id: public_id,
                chars: String::new(),
                yield_time_ms: 10,
                max_output_tokens: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "unknown_session");
}

#[tokio::test]
async fn restart_and_stale_database_restore_do_not_reuse_public_terminal_ids() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    let first_id = {
        let store = Arc::new(StateStore::open(&database, &watermark).unwrap());
        let gateway = GatewayBackend::new(
            Arc::new(FakeBackend::new("vps")),
            route(BackendKind::Vps, "vps", 1),
            Duration::from_secs(1),
        )
        .with_state_store(store);
        match confirm_call(&gateway, "before-restart", exec("yield")).await {
            ToolOutput::ExecCommand(output) => output.session_id.unwrap(),
            _ => panic!("expected exec output"),
        }
    };
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE allocations SET value = 999 WHERE kind = 'terminal'",
            [],
        )
        .unwrap();

    let store = Arc::new(StateStore::open(&database, &watermark).unwrap());
    let gateway = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    )
    .with_state_store(store);
    let lost = gateway
        .call(
            context("after-restart"),
            ToolRequest::WriteStdin(WriteStdinInput {
                session_id: first_id,
                chars: String::new(),
                yield_time_ms: 10,
                max_output_tokens: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(lost.code, "unknown_session");
    let second_id = match confirm_call(&gateway, "after-restart", exec("yield")).await {
        ToolOutput::ExecCommand(output) => output.session_id.unwrap(),
        _ => panic!("expected exec output"),
    };
    assert!(second_id > first_id);
}

#[tokio::test]
async fn mutable_skill_handles_and_cursors_keep_principal_and_route_affinity() {
    let vps = Arc::new(FakeBackend::new("vps"));
    let gateway = GatewayBackend::new(
        vps.clone(),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    );
    let listed = confirm_call(
        &gateway,
        "skills-chat",
        ToolRequest::SkillsList(SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        }),
    )
    .await;
    let (package, resource, cursor) = match listed {
        ToolOutput::SkillsList(output) => {
            let skill = &output.skills[0];
            (
                skill.package.clone(),
                skill.main_resource.clone(),
                output.next_cursor.unwrap(),
            )
        }
        _ => panic!("expected skill list"),
    };
    assert!(package.starts_with("gateway-package-"));
    assert!(resource.starts_with("gateway-skill://"));
    assert!(cursor.starts_with("gateway-cursor-"));
    gateway
        .call(
            context("skills-chat"),
            ToolRequest::SkillsList(SkillListInput {
                scope: SkillScope::Project,
                cursor: Some(cursor),
            }),
        )
        .await
        .unwrap();
    let read = gateway
        .call(
            context("skills-chat"),
            ToolRequest::SkillsRead(SkillReadInput {
                scope: SkillScope::Project,
                package: package.clone(),
                resource: resource.clone(),
                cursor: None,
            }),
        )
        .await
        .unwrap();
    match read {
        ToolOutput::SkillsRead(output) => {
            assert_eq!(output.resource, resource);
            assert!(output.next_cursor.unwrap().starts_with("gateway-cursor-"));
        }
        _ => panic!("expected skill read"),
    }
    let denied = gateway
        .call(
            other_principal_context("other-chat"),
            ToolRequest::SkillsRead(SkillReadInput {
                scope: SkillScope::Project,
                package,
                resource,
                cursor: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "invalid_resource");
    assert!(
        vps.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.contains("native-next"))
    );
    assert!(
        vps.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.contains("native-package"))
    );
}

#[tokio::test]
async fn skill_handles_from_a_superseded_local_generation_fail_closed() {
    let gateway = GatewayBackend::new(
        Arc::new(FakeBackend::new("vps")),
        route(BackendKind::Vps, "vps", 1),
        Duration::from_secs(1),
    );
    gateway
        .register_local(
            "launch-a",
            1,
            Arc::new(FakeBackend::new("local-a")),
            route(BackendKind::Local, "local-a", 0),
        )
        .unwrap();
    let listed = confirm_call(
        &gateway,
        "takeover-skills",
        ToolRequest::SkillsList(SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        }),
    )
    .await;
    let (old_package, old_resource, old_cursor) = match listed {
        ToolOutput::SkillsList(output) => {
            let skill = &output.skills[0];
            (
                skill.package.clone(),
                skill.main_resource.clone(),
                output.next_cursor.unwrap(),
            )
        }
        _ => panic!("expected skill list"),
    };
    gateway
        .register_local(
            "launch-b",
            1,
            Arc::new(FakeBackend::new("local-b")),
            route(BackendKind::Local, "local-b", 0),
        )
        .unwrap();

    let cursor_error = gateway
        .call(
            context("takeover-skills"),
            ToolRequest::SkillsList(SkillListInput {
                scope: SkillScope::Project,
                cursor: Some(old_cursor),
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(cursor_error.code, "lost_context");
    let resource_error = gateway
        .call(
            context("takeover-skills"),
            ToolRequest::SkillsRead(SkillReadInput {
                scope: SkillScope::Project,
                package: old_package,
                resource: old_resource.clone(),
                cursor: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(resource_error.code, "lost_context");

    let fresh = confirm_call(
        &gateway,
        "takeover-skills",
        ToolRequest::SkillsList(SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        }),
    )
    .await;
    let ToolOutput::SkillsList(fresh) = fresh else {
        panic!("expected skill list");
    };
    assert_ne!(fresh.skills[0].main_resource, old_resource);
}

fn extract_output(output: ToolOutput) -> String {
    match output {
        ToolOutput::ExecCommand(output) | ToolOutput::WriteStdin(output) => output.output,
        ToolOutput::TerminateSession(_) => "terminated".to_owned(),
        other => panic!("unexpected output: {other:?}"),
    }
}
