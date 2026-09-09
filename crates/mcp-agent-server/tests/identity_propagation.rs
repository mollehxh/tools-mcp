use axum::Extension;
use mcp_agent_server::ApplicationContext;
use mcp_agent_server::http::{AuthenticatedPrincipal, HttpConfig, router};
use mcp_agent_tool_contracts::{
    BackendFuture, CallContext, ExecCommandOutput, ToolBackend, ToolOutput, ToolRequest,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct IdentityBackend {
    identity: Mutex<Option<mcp_agent_tool_contracts::CallIdentity>>,
}

impl ToolBackend for IdentityBackend {
    fn call(&self, context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            *self.identity.lock().unwrap() = context.identity;
            Ok(ToolOutput::ExecCommand(ExecCommandOutput {
                chunk_id: None,
                wall_time_seconds: 0.0,
                exit_code: Some(0),
                session_id: None,
                original_token_count: None,
                output: "ok".to_owned(),
            }))
        })
    }
}

#[tokio::test]
async fn authenticated_identity_reaches_backend_across_streamable_http_task_boundary() {
    let backend = Arc::new(IdentityBackend::default());
    let cancellation = CancellationToken::new();
    let app = router(
        Arc::new(ApplicationContext::new(Arc::clone(&backend))),
        HttpConfig {
            trusted_openai_header_salt: Some(b"deployment-salt".to_vec()),
            ..HttpConfig::default()
        },
        cancellation.child_token(),
    )
    .unwrap()
    .layer(Extension(AuthenticatedPrincipal {
        principal_fingerprint: "oauth-grant-fingerprint".to_owned(),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "exec_command")
        .header("X-OpenAI-Session", "conversation-a")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "exec_command", "arguments": {"cmd": "pwd"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        backend
            .identity
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .principal_fingerprint,
        "oauth-grant-fingerprint"
    );

    cancellation.cancel();
    server.abort();
}
