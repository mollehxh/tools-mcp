use anyhow::Context;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use codex_tools_runtime::contracts::frozen_tool_contracts;
use mcp_agent_server::stub::StubServer;
use rand::RngCore;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientInfo, ErrorCode, ErrorData, ListToolsResult,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleServer, ServerHandler};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::oauth_spike::{
    OAuthSpike, OAuthSpikeConfig, authorization_server_metadata, authorize,
    client_metadata_document, consent, protected_resource_metadata, require_bearer, token,
};

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector@2.1.0";
const EXPECTED_TOOLS: [&str; 5] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "skills.list",
    "skills.read",
];

const STANDARD_META_KEYS: [&str; 4] = [
    "io.modelcontextprotocol/protocolVersion",
    "io.modelcontextprotocol/clientInfo",
    "io.modelcontextprotocol/clientCapabilities",
    "io.modelcontextprotocol/logLevel",
];

#[derive(Clone)]
struct ContextObservingStub {
    fingerprint_salt: Arc<[u8; 32]>,
}

impl ContextObservingStub {
    fn new() -> Self {
        let mut salt = [0_u8; 32];
        rand::rng().fill_bytes(&mut salt);
        Self {
            fingerprint_salt: Arc::new(salt),
        }
    }

    fn observe(&self, context: &rmcp::service::RequestContext<RoleServer>) {
        let protocol = context
            .protocol_version()
            .map_or_else(|| "unknown".to_owned(), |version| version.to_string());
        let client = context.client_info().map_or_else(
            || "unknown".to_owned(),
            |info| format!("{}@{}", info.name, info.version),
        );
        eprintln!("U1 request context: protocol={protocol} client={client}");
        for (key, value) in context
            .meta
            .iter()
            .filter(|(key, _)| !STANDARD_META_KEYS.contains(&key.as_str()))
        {
            let safe_key = key
                .chars()
                .take(128)
                .map(|character| {
                    if character.is_ascii_alphanumeric() || ".-_/:".contains(character) {
                        character
                    } else {
                        '?'
                    }
                })
                .collect::<String>();
            let mut digest = Sha256::new();
            digest.update(self.fingerprint_salt.as_slice());
            digest.update(serde_json::to_vec(value).unwrap_or_default());
            let digest = digest.finalize();
            let fingerprint =
                digest[..6]
                    .iter()
                    .fold(String::with_capacity(12), |mut fingerprint, byte| {
                        let _ = write!(fingerprint, "{byte:02x}");
                        fingerprint
                    });
            eprintln!("U1 candidate context: key={safe_key} fingerprint={fingerprint}");
        }
    }
}

impl ServerHandler for ContextObservingStub {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "U1 contract-only stub; execution is disabled while ChatGPT context is observed",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.observe(&context);
        Ok(ListToolsResult::with_all_items(StubServer::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.observe(&context);
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown tool",
                None,
            ));
        }
        Err(ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            "U1 checkpoint disables tool execution",
            None,
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        StubServer::tools()
            .into_iter()
            .find(|tool| tool.name == name)
    }
}

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
    endpoint_kind: String,
    endpoint: String,
    expected_tools: Vec<String>,
    observed_at: String,
    registration_mechanism: String,
    observed_protocol_versions: Vec<String>,
    redirect_uris: Vec<String>,
    refresh_exchanges: u32,
    context_correlation: String,
    port_443_unchanged: bool,
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
        checkpoint.version == 4,
        "ChatGPT stable-edge checkpoint uses an unsupported record version"
    );
    anyhow::ensure!(
        checkpoint.status == "passed",
        "ChatGPT stable-edge OAuth checkpoint has not passed"
    );
    anyhow::ensure!(
        !checkpoint.observed_at.trim().is_empty(),
        "ChatGPT stable-edge OAuth checkpoint is missing observed_at"
    );
    anyhow::ensure!(
        checkpoint
            .expected_tools
            .iter()
            .map(String::as_str)
            .eq(EXPECTED_TOOLS),
        "ChatGPT/ngrok checkpoint tool surface does not match the frozen contract"
    );
    anyhow::ensure!(
        checkpoint.endpoint_kind == "free_stable_ngrok_https_to_loopback_8443",
        "ChatGPT checkpoint did not use the verified stable-edge topology"
    );
    anyhow::ensure!(
        checkpoint.endpoint == "https://fidela-unsubversive-imaginarily.ngrok-free.dev/mcp",
        "ChatGPT checkpoint did not use the canonical stable MCP resource"
    );
    anyhow::ensure!(
        matches!(checkpoint.registration_mechanism.as_str(), "cimd" | "dcr"),
        "ChatGPT checkpoint is missing its observed client registration mechanism"
    );
    anyhow::ensure!(
        checkpoint
            .observed_protocol_versions
            .iter()
            .any(|version| version == "2026-07-28"),
        "ChatGPT checkpoint did not observe MCP 2026-07-28"
    );
    anyhow::ensure!(
        !checkpoint.redirect_uris.is_empty(),
        "ChatGPT checkpoint is missing the observed redirect URI"
    );
    anyhow::ensure!(
        checkpoint.refresh_exchanges >= 2,
        "ChatGPT checkpoint must prove repeated access-token refresh"
    );
    anyhow::ensure!(
        checkpoint.context_correlation.starts_with("stable:"),
        "ChatGPT checkpoint found no stable per-conversation correlation key; KTD5 must be revised before U2"
    );
    anyhow::ensure!(
        checkpoint.port_443_unchanged,
        "ChatGPT checkpoint did not prove port 443 remained unchanged"
    );

    Ok(checkpoint.expected_tools.len())
}

pub fn serve_oauth(
    bind: &str,
    public_issuer: &str,
    certificate_path: &Path,
    private_key_path: &Path,
) -> anyhow::Result<()> {
    let owner_secret = std::env::var("U1_OWNER_SECRET")
        .context("set U1_OWNER_SECRET to a disposable high-entropy value")?;
    let client_id = std::env::var("U1_ALLOWED_CLIENT_ID")
        .context("set U1_ALLOWED_CLIENT_ID to the exact observed ChatGPT client ID")?;
    let redirect_uri = std::env::var("U1_ALLOWED_REDIRECT_URI")
        .context("set U1_ALLOWED_REDIRECT_URI to the exact observed ChatGPT redirect URI")?;
    let oauth = OAuthSpike::new(OAuthSpikeConfig::new(
        public_issuer,
        owner_secret,
        client_id,
        &redirect_uri,
    )?);
    let address: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid U1 OAuth bind address {bind}"))?;
    tokio::runtime::Runtime::new()?.block_on(async move {
        let tls = RustlsConfig::from_pem_file(certificate_path, private_key_path)
            .await
            .context("failed to load U1 TLS certificate and key")?;
        let cancellation = CancellationToken::new();
        let router = oauth_router(oauth.clone(), cancellation.clone());
        let handle = Handle::new();
        let server = tokio::spawn(
            axum_server::bind_rustls(address, tls)
                .handle(handle.clone())
                .serve(router.into_make_service()),
        );
        println!(
            "U1 OAuth/MCP spike listening at {}/mcp",
            oauth.config().issuer()
        );
        println!("Access tokens expire after 30 seconds; refresh tokens rotate on every use.");
        println!("Tool execution is intentionally unavailable; press Ctrl-C to stop.");
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
        cancellation.cancel();
        handle.graceful_shutdown(Some(Duration::from_secs(10)));
        server.await.context("U1 OAuth/TLS server task failed")??;
        Ok(())
    })
}

pub fn oauth_router(oauth: OAuthSpike, cancellation: CancellationToken) -> axum::Router {
    let rmcp_config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_legacy_session_mode(false)
        .with_allowed_hosts(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
            oauth
                .config()
                .issuer()
                .strip_prefix("https://")
                .unwrap_or_default()
                .to_owned(),
        ])
        .with_allowed_origins(vec![oauth.config().issuer().to_owned()])
        .with_cancellation_token(cancellation);
    let observer = ContextObservingStub::new();
    let service: StreamableHttpService<ContextObservingStub, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(observer.clone()),
            std::sync::Arc::default(),
            rmcp_config,
        );
    let protected = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(from_fn_with_state(oauth.clone(), require_bearer));
    axum::Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/openid-configuration",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route("/oauth-client/chatgpt.json", get(client_metadata_document))
        .route("/authorize", get(authorize).post(consent))
        .route("/token", post(token))
        .merge(protected)
        .with_state(oauth)
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
