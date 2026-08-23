use rmcp::model::{ClientInfo, ProtocolVersion};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn protocol_v2_requires_a_fresh_chatgpt_checkpoint() {
    let error = xtask::transport_spike::verify_chatgpt_checkpoint().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ChatGPT/ngrok checkpoint has not passed")
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
