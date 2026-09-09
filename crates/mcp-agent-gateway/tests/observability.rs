use mcp_agent_gateway::{
    BackendKind, CallDisposition, GatewayBackend, Observability, RouteContext,
};
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, ExecCommandInput, ExecCommandOutput, ToolBackend,
    ToolOutput, ToolRequest,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct EchoBackend;

impl ToolBackend for EchoBackend {
    fn call(&self, _context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async {
            Ok(ToolOutput::ExecCommand(ExecCommandOutput {
                chunk_id: None,
                wall_time_seconds: 0.0,
                exit_code: Some(0),
                session_id: None,
                original_token_count: Some(0),
                output: "raw-secret-output".to_owned(),
            }))
        })
    }
}

fn secret_request() -> ToolRequest {
    ToolRequest::ExecCommand(ExecCommandInput {
        cmd: "printf 'token=raw-secret-output'".to_owned(),
        workdir: Some("/Users/private/repository".to_owned()),
        tty: false,
        yield_time_ms: 10_000,
        max_output_tokens: Some(1_000),
        shell: None,
        login: None,
    })
}

#[test]
fn structured_events_and_metrics_never_include_payloads_or_raw_identifiers() {
    let telemetry = Observability::test_instance();
    let route = RouteContext {
        kind: BackendKind::Vps,
        workspace_id: "/workspace/private-repository".to_owned(),
        generation: 42,
        operating_system: "linux".to_owned(),
        privilege_posture: "rootless-podman-verified".to_owned(),
    };

    telemetry.gateway_started();
    telemetry.set_ready(true);
    telemetry.backend_connected(&route, Some("raw-device-certificate-fingerprint"));
    let call = telemetry.begin_call(&secret_request(), "raw-owner-access-token");
    call.finish(
        Some(&route),
        CallDisposition::OutcomeUnknown,
        Some(&BackendError::new(
            "outcome_unknown",
            "raw-secret-output /Users/private/repository Authorization: Bearer leaked",
        )),
        Duration::from_millis(17),
    );
    telemetry.record_refresh(false, Some("invalid_grant"));
    telemetry.record_relay_rejection(false, "secret_error_class");

    let evidence = format!(
        "{}\n{}",
        telemetry.recent_events().join("\n"),
        telemetry.render_metrics(None).unwrap()
    );
    for forbidden in [
        "raw-owner-access-token",
        "raw-device-certificate-fingerprint",
        "raw-secret-output",
        "/Users/private/repository",
        "/workspace/private-repository",
        "Authorization: Bearer",
        "secret_error_class",
    ] {
        assert!(!evidence.contains(forbidden), "leaked {forbidden}");
    }
    assert!(evidence.contains("\"event\":\"call_finished\""));
    assert!(evidence.contains("\"tool\":\"exec_command\""));
    assert!(evidence.contains("\"disposition\":\"outcome_unknown\""));
    assert!(evidence.contains("tools_mcp_gateway_ready 1"));
    assert!(evidence.contains("tools_mcp_backend_eligible{backend=\"vps\"} 1"));
    assert!(evidence.contains("tools_mcp_calls_in_flight 0"));
    assert!(evidence.contains("tools_mcp_oauth_refresh_total{outcome=\"failure\"} 1"));
}

#[tokio::test]
async fn gateway_dispatch_emits_context_fence_and_completion_without_backend_output() {
    let telemetry = Arc::new(Observability::test_instance());
    let gateway = GatewayBackend::new(
        Arc::new(EchoBackend),
        RouteContext {
            kind: BackendKind::Vps,
            workspace_id: "/workspace/private-repository".to_owned(),
            generation: 8,
            operating_system: "linux".to_owned(),
            privilege_posture: "rootless-podman-verified".to_owned(),
        },
        Duration::from_secs(4),
    )
    .with_observability(Arc::clone(&telemetry));
    let context = || CallContext::new(CancellationToken::new(), None);

    assert_eq!(
        gateway
            .call(context(), secret_request())
            .await
            .unwrap_err()
            .code,
        "backend_changed"
    );
    gateway.call(context(), secret_request()).await.unwrap();

    let events = telemetry.recent_events().join("\n");
    assert!(events.contains("\"disposition\":\"backend_changed\""));
    assert!(events.contains("\"disposition\":\"completed\""));
    assert!(!events.contains("raw-secret-output"));
    assert!(!events.contains("/workspace/private-repository"));
}

#[test]
fn host_metric_merge_accepts_only_bounded_unlabelled_project_metrics() {
    let telemetry = Observability::test_instance();
    let safe = "tools_mcp_sqlite_integrity_ok 1\n\
                tools_mcp_backup_age_seconds 31\n\
                tools_mcp_tls_certificate_lifetime_seconds 2592000\n";
    let rendered = telemetry.render_metrics(Some(safe)).unwrap();
    assert!(rendered.contains("tools_mcp_sqlite_integrity_ok 1"));

    for unsafe_metrics in [
        "other_project_metric 1\n",
        "tools_mcp_probe{secret=\"token\"} 1\n",
        "tools_mcp_probe /workspace/private\n",
        "tools_mcp_probe 1\nAuthorization: Bearer secret\n",
    ] {
        assert!(telemetry.render_metrics(Some(unsafe_metrics)).is_err());
    }
}
