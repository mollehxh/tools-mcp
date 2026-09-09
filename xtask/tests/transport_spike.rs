use rmcp::model::{ClientInfo, ProtocolVersion};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[test]
fn live_chatgpt_checkpoint_unblocks_hybrid_execution_work() {
    assert_eq!(
        xtask::transport_spike::verify_chatgpt_checkpoint().unwrap(),
        5
    );
}

#[tokio::test]
async fn both_loopback_lifecycle_modes_discover_the_frozen_surface() {
    let (stateless, legacy) = tokio::time::timeout(
        Duration::from_secs(10),
        xtask::transport_spike::probe_loopback_transports(),
    )
    .await
    .expect("loopback transport spike timed out")
    .expect("loopback transport spike failed");

    assert_eq!((stateless, legacy), (5, 5));
}

#[tokio::test]
async fn persistent_stub_serves_the_frozen_surface_until_cancelled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind persistent stub listener");
    let address = listener.local_addr().expect("read listener address");
    let cancellation = CancellationToken::new();
    let server = tokio::spawn(xtask::transport_spike::serve_stub(
        listener,
        cancellation.clone(),
        vec!["127.0.0.1".to_owned()],
    ));

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
    );
    let client = ClientInfo::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("connect to persistent stub");

    let names = client
        .list_tools(None)
        .await
        .expect("list persistent stub tools")
        .tools
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "exec_command",
            "write_stdin",
            "apply_patch",
            "skills.list",
            "skills.read",
        ]
    );

    client.cancel().await.expect("stop MCP client");
    cancellation.cancel();
    server
        .await
        .expect("join persistent stub task")
        .expect("stop persistent stub");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn oauth_spike_enforces_pkce_resource_refresh_and_bearer_access() {
    const ISSUER: &str = "https://u1.example.test:8443";
    const CLIENT_ID: &str = "https://chatgpt.example.test/client.json";
    const REDIRECT_URI: &str = "https://chatgpt.example.test/oauth/callback";
    const OWNER_SECRET: &str = "0123456789abcdef0123456789abcdef";
    const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";

    let oauth = xtask::oauth_spike::OAuthSpike::new(
        xtask::oauth_spike::OAuthSpikeConfig::new(
            ISSUER,
            OWNER_SECRET.to_owned(),
            CLIENT_ID.to_owned(),
            REDIRECT_URI,
        )
        .expect("build OAuth spike config"),
    );
    let cancellation = CancellationToken::new();
    let router = xtask::transport_spike::oauth_router(oauth, cancellation.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind OAuth spike");
    let address = listener.local_addr().expect("read OAuth spike address");
    let server = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancellation.cancelled_owned().await })
                .await
                .expect("serve OAuth spike");
        }
    });
    let base = format!("http://{address}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build HTTP client");

    let protected_metadata: Value = client
        .get(format!("{base}/.well-known/oauth-protected-resource/mcp"))
        .send()
        .await
        .expect("get protected-resource metadata")
        .json()
        .await
        .expect("decode protected-resource metadata");
    assert_eq!(protected_metadata["resource"], format!("{ISSUER}/mcp"));
    assert_eq!(protected_metadata["authorization_servers"][0], ISSUER);

    let oidc_discovery = client
        .get(format!("{base}/.well-known/openid-configuration"))
        .send()
        .await
        .expect("probe optional OIDC discovery");
    assert_eq!(oidc_discovery.status(), reqwest::StatusCode::NOT_FOUND);

    let client_metadata: Value = client
        .get(format!("{base}/oauth-client/chatgpt.json"))
        .send()
        .await
        .expect("get client metadata document")
        .json()
        .await
        .expect("decode client metadata document");
    assert_eq!(client_metadata["client_id"], CLIENT_ID);
    assert_eq!(client_metadata["redirect_uris"][0], REDIRECT_URI);
    assert_eq!(client_metadata["token_endpoint_auth_method"], "none");
    assert_eq!(client_metadata["scope"], "offline_access");

    let unauthorized = client
        .post(format!("{base}/mcp"))
        .send()
        .await
        .expect("call protected MCP without token");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(
        unauthorized
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("oauth-protected-resource/mcp"))
    );

    let challenge = pkce_challenge(VERIFIER);
    let resource = format!("{ISSUER}/mcp");
    for (redirect_uri, resource, method, scope) in [
        (
            REDIRECT_URI,
            "https://wrong.example.test/mcp",
            "S256",
            "offline_access",
        ),
        (
            "https://wrong.example.test/callback",
            resource.as_str(),
            "S256",
            "offline_access",
        ),
        (REDIRECT_URI, resource.as_str(), "plain", "offline_access"),
        (REDIRECT_URI, resource.as_str(), "S256", "openid"),
    ] {
        let denied = client
            .get(format!("{base}/authorize"))
            .query(&[
                ("response_type", "code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", redirect_uri),
                ("resource", resource),
                ("scope", scope),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", method),
            ])
            .send()
            .await
            .expect("send invalid authorization request");
        assert!(denied.status().is_client_error());
    }
    let authorize_page = client
        .get(format!("{base}/authorize"))
        .query(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("resource", &format!("{ISSUER}/mcp")),
            ("scope", "offline_access"),
            ("state", "opaque-state"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .expect("open authorization page");
    assert_eq!(authorize_page.status(), reqwest::StatusCode::OK);
    let html = authorize_page
        .text()
        .await
        .expect("read authorization page");
    let request_id = html
        .split("name=\"request_id\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("extract request id");
    let redirect = client
        .post(format!("{base}/authorize"))
        .form(&[("request_id", request_id), ("owner_secret", OWNER_SECRET)])
        .send()
        .await
        .expect("approve authorization");
    assert_eq!(redirect.status(), reqwest::StatusCode::SEE_OTHER);
    let location = redirect
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("authorization redirect");
    let location = Url::parse(location).expect("parse authorization redirect");
    let code = location
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
        .expect("authorization code");
    assert_eq!(
        location
            .query_pairs()
            .find_map(|(key, value)| (key == "iss").then(|| value.into_owned()))
            .as_deref(),
        Some(ISSUER)
    );

    let first = exchange_code(
        &client,
        &base,
        CLIENT_ID,
        REDIRECT_URI,
        ISSUER,
        &code,
        VERIFIER,
    )
    .await;
    let replayed_code = exchange_code_response(
        &client,
        &base,
        CLIENT_ID,
        REDIRECT_URI,
        ISSUER,
        &code,
        VERIFIER,
    )
    .await;
    assert_eq!(replayed_code.status(), reqwest::StatusCode::BAD_REQUEST);
    let access = first["access_token"].as_str().expect("access token");
    let refresh_one = first["refresh_token"].as_str().expect("refresh token");

    let discovery_body = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let evil_origin = client
        .post(format!("{base}/mcp"))
        .bearer_auth(access)
        .header("Origin", "https://evil.example.test")
        .header("Content-Type", "application/json")
        .body(discovery_body)
        .send()
        .await
        .expect("call MCP with disallowed Origin");
    assert_eq!(evil_origin.status(), reqwest::StatusCode::FORBIDDEN);
    let evil_host = client
        .post(format!("{base}/mcp"))
        .bearer_auth(access)
        .header("Host", "evil.example.test")
        .header("Content-Type", "application/json")
        .body(discovery_body)
        .send()
        .await
        .expect("call MCP with disallowed Host");
    assert_eq!(evil_host.status(), reqwest::StatusCode::FORBIDDEN);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(access),
    );
    let mcp = ClientInfo::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("connect with OAuth access token");
    assert_eq!(
        mcp.list_tools(None).await.expect("list tools").tools.len(),
        5
    );
    mcp.cancel().await.expect("stop OAuth MCP client");

    let second = refresh(&client, &base, CLIENT_ID, ISSUER, refresh_one).await;
    let refresh_two = second["refresh_token"]
        .as_str()
        .expect("rotated refresh token");
    let third = refresh(&client, &base, CLIENT_ID, ISSUER, refresh_two).await;
    let refresh_three = third["refresh_token"]
        .as_str()
        .expect("second rotated refresh token");
    let replay = refresh_response(&client, &base, CLIENT_ID, ISSUER, refresh_one).await;
    assert_eq!(replay.status(), reqwest::StatusCode::BAD_REQUEST);
    let revoked = refresh_response(&client, &base, CLIENT_ID, ISSUER, refresh_three).await;
    assert_eq!(revoked.status(), reqwest::StatusCode::BAD_REQUEST);

    cancellation.cancel();
    server.await.expect("join OAuth spike server");
}

async fn exchange_code(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    redirect_uri: &str,
    issuer: &str,
    code: &str,
    verifier: &str,
) -> Value {
    let response = exchange_code_response(
        client,
        base,
        client_id,
        redirect_uri,
        issuer,
        code,
        verifier,
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("decode token response")
}

async fn exchange_code_response(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    redirect_uri: &str,
    issuer: &str,
    code: &str,
    verifier: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("resource", &format!("{issuer}/mcp")),
            ("code", code),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .expect("exchange authorization code")
}

async fn refresh(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    issuer: &str,
    refresh_token: &str,
) -> Value {
    let response = refresh_response(client, base, client_id, issuer, refresh_token).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("decode refresh response")
}

async fn refresh_response(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    issuer: &str,
    refresh_token: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("resource", &format!("{issuer}/mcp")),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .expect("refresh access token")
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
