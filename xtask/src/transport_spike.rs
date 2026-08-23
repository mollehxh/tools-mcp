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
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector@2.1.0";
const EXPECTED_TOOLS: [&str; 5] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
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

#[derive(Deserialize)]
struct ChatGptCheckpoint {
    version: u32,
    status: String,
    expected_tools: Vec<String>,
    observed_at: String,
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
        let chatgpt = verify_chatgpt_checkpoint()?;
        println!("ChatGPT/ngrok checkpoint: PASS ({chatgpt} tools)");
        anyhow::ensure!(
            stateless == 5 && legacy == 5 && inspector == 5,
            "stub surface must contain five tools"
        );
        Ok(())
    })
}

pub fn verify_chatgpt_checkpoint() -> anyhow::Result<usize> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/e2e/chatgpt-scan-tools-checkpoint.toml");
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let checkpoint: ChatGptCheckpoint =
        toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))?;

    anyhow::ensure!(
        checkpoint.version == 2,
        "ChatGPT/ngrok checkpoint uses an unsupported protocol version"
    );
    anyhow::ensure!(
        checkpoint.status == "passed",
        "ChatGPT/ngrok checkpoint has not passed"
    );
    anyhow::ensure!(
        !checkpoint.observed_at.trim().is_empty(),
        "ChatGPT/ngrok checkpoint is missing observed_at"
    );
    anyhow::ensure!(
        checkpoint
            .expected_tools
            .iter()
            .map(String::as_str)
            .eq(EXPECTED_TOOLS),
        "ChatGPT/ngrok checkpoint tool surface does not match the frozen contract"
    );

    Ok(checkpoint.expected_tools.len())
}

pub fn serve(bind: &str, public_host: Option<&str>) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("failed to bind U1 stub at {bind}"))?;
        let address = listener.local_addr()?;
        let cancellation = CancellationToken::new();
        let allowed_hosts = public_host.map_or_else(
            || {
                vec![
                    "localhost".to_owned(),
                    "127.0.0.1".to_owned(),
                    "::1".to_owned(),
                ]
            },
            |host| {
                vec![
                    "localhost".to_owned(),
                    "127.0.0.1".to_owned(),
                    "::1".to_owned(),
                    host.to_owned(),
                ]
            },
        );
        let server = tokio::spawn(serve_stub(listener, cancellation.clone(), allowed_hosts));

        println!("U1 contract-only stub listening at http://{address}/mcp");
        if let Some(public_host) = public_host {
            println!("Allowed public Host: {public_host}");
        }
        println!("Tool execution is intentionally unavailable; press Ctrl-C to stop.");

        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
        cancellation.cancel();
        server.await.context("U1 stub server task failed")??;
        Ok(())
    })
}

pub async fn serve_stub(
    listener: tokio::net::TcpListener,
    cancellation: CancellationToken,
    allowed_hosts: Vec<String>,
) -> anyhow::Result<()> {
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_legacy_session_mode(false)
        .with_allowed_hosts(allowed_hosts)
        .with_cancellation_token(cancellation.child_token());
    let service: StreamableHttpService<StubServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(StubServer), std::sync::Arc::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancellation.cancelled_owned().await })
        .await
        .context("U1 stub server failed")
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
