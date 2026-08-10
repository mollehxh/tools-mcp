use anyhow::Context;
use mcp_agent_server::stub::StubServer;
use rmcp::model::{ClientInfo, ProtocolVersion};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use tokio_util::sync::CancellationToken;

pub fn run() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let (stateless, legacy) = tokio::join!(
            probe(ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            }),
            probe(ClientLifecycleMode::Initialize),
        );
        let stateless = stateless?;
        let legacy = legacy?;
        println!("loopback stateless discovery: {stateless} tools");
        println!("loopback legacy initialize: {legacy} tools");
        println!("Inspector-compatible tools/list: PASS");
        println!("ChatGPT/ngrok checkpoint: NOT RUN (manual external-client checkpoint)");
        anyhow::ensure!(
            stateless == 6 && legacy == 6,
            "stub surface must contain six tools"
        );
        Ok(())
    })
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
        names
            == [
                "exec_command",
                "write_stdin",
                "apply_patch",
                "skills.install",
                "skills.list",
                "skills.read",
            ],
        "unexpected tool discovery order: {names:?}"
    );

    cancellation.cancel();
    server.await?;
    Ok(tools.len())
}
