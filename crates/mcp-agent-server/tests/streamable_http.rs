mod common;

use common::{Fixture, arguments, complete, yielded_command};
use mcp_agent_server::http::router;
use mcp_agent_server::http::{HttpConfig, HttpConfigError};
use serde_json::json;
use skill_store::{
    FetchedRepository, GitFetcher, InstallLimits, NormalizedGitSource, RepositoryEntry,
    SkillInstallError,
};
use std::sync::{Arc, Condvar, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[test]
fn defaults_are_the_fixed_u7_admission_contract() {
    let config = HttpConfig::default();
    assert_eq!(config.max_request_body_bytes, 4 * 1024 * 1024);
    assert_eq!(config.max_header_bytes, 64 * 1024);
    assert_eq!(config.max_header_count, 100);
    assert_eq!(config.max_in_flight_requests, 32);
    assert_eq!(config.max_sse_responses, 16);
}

#[test]
fn wildcard_host_or_origin_validation_is_rejected() {
    let config = HttpConfig {
        allowed_hosts: vec!["*".to_string()],
        ..HttpConfig::default()
    };
    assert_eq!(config.validate(), Err(HttpConfigError::WildcardHost));

    let config = HttpConfig {
        allowed_origins: vec!["*".to_string()],
        ..HttpConfig::default()
    };
    assert_eq!(config.validate(), Err(HttpConfigError::WildcardOrigin));

    let config = HttpConfig {
        upload_idle_timeout: std::time::Duration::ZERO,
        ..HttpConfig::default()
    };
    assert_eq!(config.validate(), Err(HttpConfigError::ZeroLimit));

    let config = HttpConfig {
        response_idle_timeout: std::time::Duration::ZERO,
        ..HttpConfig::default()
    };
    assert_eq!(config.validate(), Err(HttpConfigError::ZeroLimit));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn raw_http_is_stateless_json_and_enforces_admission() {
    let fixture = Fixture::new();
    let token = CancellationToken::new();
    let app = router(
        fixture.context.clone(),
        HttpConfig::default(),
        token.child_token(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let token = token.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let discovery = modern_tools_list(1);
    let response = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&discovery)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let is_json = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json");
    let response_body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{response_body}");
    assert!(is_json);
    let body: serde_json::Value = serde_json::from_str(&response_body).unwrap();
    let tools = body["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    assert_eq!(tools[0]["name"], "exec_command");
    assert_eq!(tools[5]["name"], "skills.read");

    for (header, value) in [
        ("X-Forwarded-Host", "forged.example"),
        ("Mcp-Session-Id", "unrelated-stateless-session"),
    ] {
        let response = client
            .post(&url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/list")
            .header(header, value)
            .json(&discovery)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{header}");
    }

    let legacy_initialize = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "legacy-u7-test", "version": "1"}
        }
    });
    let legacy_response = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Method", "initialize")
        .json(&legacy_initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(legacy_response.status(), reqwest::StatusCode::OK);
    assert!(legacy_response.headers().get("Mcp-Session-Id").is_none());

    let denied_host = client
        .post(&url)
        .header("Host", "forged.example")
        .json(&discovery)
        .send()
        .await
        .unwrap();
    assert!(!denied_host.status().is_success());
    let denied_origin = client
        .post(&url)
        .header("Origin", "https://evil.example")
        .json(&discovery)
        .send()
        .await
        .unwrap();
    assert_eq!(denied_origin.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(!client.get(&url).send().await.unwrap().status().is_success());
    assert!(
        !client
            .delete(&url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );

    token.cancel();
    server.await.unwrap();
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_and_header_rejections_recover_without_starting_tool_work() {
    let fixture = Fixture::new();
    let token = CancellationToken::new();
    let config = HttpConfig {
        max_request_body_bytes: 64,
        max_header_count: 20,
        ..HttpConfig::default()
    };
    let app = router(fixture.context.clone(), config, token.child_token()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let token = token.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");

    let oversized = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body("x".repeat(65))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    let mut request = client.post(&url).body("{}");
    for index in 0..30 {
        request = request.header(format!("x-u7-{index}"), "v");
    }
    let too_many_headers = request.send().await.unwrap();
    assert_eq!(
        too_many_headers.status(),
        reqwest::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );

    let recovery = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_ne!(recovery.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_ne!(
        recovery.status(),
        reqwest::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );

    token.cancel();
    server.await.unwrap();
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_concurrency_saturation_returns_429_and_recovers() {
    let fixture = Fixture::new();
    let token = CancellationToken::new();
    let config = HttpConfig {
        max_in_flight_requests: 1,
        ..HttpConfig::default()
    };
    let app = router(fixture.context.clone(), config, token.child_token()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let token = token.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let tool_call = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "exec_command",
            "arguments": {"cmd": yielded_command(), "yield_time_ms": 1000, "login": false},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "u7-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let first = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        async move {
            client
                .post(url)
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", "exec_command")
                .json(&tool_call)
                .send()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let discovery = modern_tools_list(11);
    let saturated = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&discovery)
        .send()
        .await
        .unwrap();
    assert_eq!(saturated.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(first.await.unwrap().status(), reqwest::StatusCode::OK);

    let recovered = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&discovery)
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), reqwest::StatusCode::OK);

    token.cancel();
    server.await.unwrap();
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_and_response_timeouts_are_enforced_and_recover() {
    let fixture = Fixture::new();
    let token = CancellationToken::new();
    let config = HttpConfig {
        upload_idle_timeout: std::time::Duration::from_millis(50),
        response_idle_timeout: std::time::Duration::from_millis(50),
        ..HttpConfig::default()
    };
    let app = router(fixture.context.clone(), config, token.child_token()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let token = token.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .unwrap();
        }
    });

    let mut upload = tokio::net::TcpStream::connect(address).await.unwrap();
    upload
        .write_all(
            b"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: 10\r\n\r\n{",
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut upload_response = vec![0_u8; 1024];
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        upload.read(&mut upload_response),
    )
    .await
    .unwrap()
    .unwrap();
    upload_response.truncate(read);
    assert!(
        String::from_utf8_lossy(&upload_response).starts_with("HTTP/1.1 408"),
        "{}",
        String::from_utf8_lossy(&upload_response)
    );

    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let slow_call = json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": {
            "name": "exec_command",
            "arguments": {"cmd": yielded_command(), "yield_time_ms": 1000, "login": false},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "u7-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let timed_out = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "exec_command")
        .json(&slow_call)
        .send()
        .await
        .unwrap();
    assert_eq!(timed_out.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);

    let recovery = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&modern_tools_list(21))
        .send()
        .await
        .unwrap();
    assert_eq!(recovery.status(), reqwest::StatusCode::OK);

    token.cancel();
    server.await.unwrap();
    fixture.processes.shutdown().await;
}

#[derive(Debug)]
struct BlockingFetcher {
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl GitFetcher for BlockingFetcher {
    fn fetch(
        &self,
        source: &NormalizedGitSource,
        _limits: &InstallLimits,
    ) -> Result<FetchedRepository, SkillInstallError> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        wake.notify_all();
        while !state.1 {
            state = wake.wait(state).unwrap();
        }
        Ok(FetchedRepository {
            repository: source.repository.clone(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            entries: vec![RepositoryEntry::regular(
                "SKILL.md",
                b"---\nname: late\ndescription: late install\n---\n".to_vec(),
            )],
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_install_worker_cannot_publish_later() {
    let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let fixture = Fixture::with_fetcher(Arc::new(BlockingFetcher {
        state: Arc::clone(&state),
    }));
    let token = CancellationToken::new();
    let config = HttpConfig {
        response_idle_timeout: std::time::Duration::from_millis(50),
        ..HttpConfig::default()
    };
    let app = router(fixture.context.clone(), config, token.child_token()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let token = token.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": {
            "name": "skills.install",
            "arguments": {
                "source": "https://example.com/skills.git",
                "scope": "project"
            },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "u7-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "skills.install")
        .json(&call)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);

    {
        let (lock, wake) = &*state;
        let mut state = lock.lock().unwrap();
        assert!(state.0, "install fetch never started");
        state.1 = true;
        wake.notify_all();
    }
    fixture.context.wait_for_install_operations().await;

    let listed = complete(
        fixture
            .handler()
            .call("skills.list", Some(arguments(json!({"scope": "project"}))))
            .await,
    );
    assert!(
        listed.structured_content.unwrap()["skills"]
            .as_array()
            .unwrap()
            .is_empty(),
        "timed-out install published after the response"
    );
    token.cancel();
    server.await.unwrap();
    fixture.processes.shutdown().await;
}

fn modern_tools_list(id: u64) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "u7-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
}
