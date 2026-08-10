use anyhow::Context;
use codex_tools_runtime::contracts::frozen_tool_contracts;
use mcp_agent_server::stub::StubServer;
use rmcp::model::{ClientInfo, ProtocolVersion};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector@2.1.0";
const EXPECTED_TOOLS: [&str; 6] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "skills.install",
    "skills.list",
    "skills.read",
];

#[derive(Deserialize)]
struct InspectorListToolsResult {
    tools: Vec<InspectorTool>,
}

#[derive(Deserialize)]
struct InspectorTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
    #[serde(rename = "outputSchema")]
    output_schema: serde_json::Value,
}

pub fn run() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let (loopback, inspector) =
            tokio::join!(probe_loopback_transports(), probe_with_inspector());
        let (stateless, legacy) = loopback?;
        let inspector = inspector?;
        println!("loopback stateless discovery: {stateless} tools");
        println!("loopback legacy initialize: {legacy} tools");
        println!("MCP Inspector {INSPECTOR_PACKAGE} tools/list: {inspector} tools");
        println!("ChatGPT/ngrok checkpoint: NOT RUN (manual external-client checkpoint)");
        anyhow::ensure!(
            stateless == 6 && legacy == 6 && inspector == 6,
            "stub surface must contain six tools"
        );
        Ok(())
    })
}

pub async fn probe_loopback_transports() -> anyhow::Result<(usize, usize)> {
    let (stateless, legacy) = tokio::join!(
        probe(ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        }),
        probe(ClientLifecycleMode::Initialize),
    );
    Ok((stateless?, legacy?))
}

async fn probe_with_inspector() -> anyhow::Result<usize> {
    let cancellation = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_legacy_session_mode(false)
        .with_cancellation_token(cancellation.child_token());
    let service: StreamableHttpService<StubServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(StubServer), std::sync::Arc::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancellation.cancelled_owned().await })
                .await;
        }
    });

    let mut command = Command::new("npx");
    command.kill_on_drop(true).args([
        "--yes",
        INSPECTOR_PACKAGE,
        "--cli",
        &format!("http://{address}/mcp"),
        "--transport",
        "http",
        "--method",
        "tools/list",
    ]);
    let output = tokio::time::timeout(Duration::from_mins(2), command.output())
        .await
        .context("MCP Inspector timed out after 120 seconds")?
        .context("failed to execute `npx`; install Node.js/npm to run the Inspector gate")?;

    cancellation.cancel();
    server.await?;

    anyhow::ensure!(
        output.status.success(),
        "MCP Inspector failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: InspectorListToolsResult =
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "MCP Inspector emitted non-JSON output: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;
    let names = result
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        names == EXPECTED_TOOLS,
        "unexpected Inspector tools: {names:?}"
    );
    for (observed, expected) in result.tools.iter().zip(frozen_tool_contracts()) {
        anyhow::ensure!(
            observed.description == expected.description,
            "Inspector description drift for {}",
            observed.name
        );
        anyhow::ensure!(
            observed.input_schema == expected.input_schema,
            "Inspector input schema drift for {}",
            observed.name
        );
        anyhow::ensure!(
            &observed.output_schema
                == expected
                    .output_schema
                    .as_ref()
                    .expect("frozen output schema"),
            "Inspector output schema drift for {}",
            observed.name
        );
    }
    Ok(names.len())
}

async fn probe(lifecycle: ClientLifecycleMode) -> anyhow::Result<usize> {
    let legacy = lifecycle == ClientLifecycleMode::Initialize;
    let cancellation = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_cancellation_token(cancellation.child_token());
    if !legacy {
        config = config.with_legacy_session_mode(false);
    }
    let service: StreamableHttpService<StubServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(StubServer), std::sync::Arc::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancellation.cancelled_owned().await })
                .await;
        }
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
    );
    let context = if legacy {
        "legacy initialize"
    } else {
        "stateless discovery"
    };
    let client = ClientInfo::default()
        .serve_with_lifecycle(transport, lifecycle)
        .await
        .context(context)?;
    let tools = client.list_tools(None).await?.tools;
    client.cancel().await?;

    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        names == EXPECTED_TOOLS,
        "unexpected tool discovery order: {names:?}"
    );

    cancellation.cancel();
    server.await?;
    Ok(tools.len())
}
