use mcp_agent::cli::{Cli, CliError};

#[test]
fn defaults_to_loopback_mcp_endpoint() {
    let cli = Cli::parse_from(["mcp-agent"]).unwrap();
    assert_eq!(cli.bind.to_string(), "127.0.0.1:8000");
    assert!(cli.public_hosts.is_empty());
}

#[test]
fn accepts_repeatable_public_hosts_and_origins() {
    let cli = Cli::parse_from([
        "mcp-agent",
        "--bind",
        "127.0.0.1:9000",
        "--public-host",
        "example.ngrok.app",
        "--public-host",
        "localhost:9000",
        "--origin",
        "https://chatgpt.com",
    ])
    .unwrap();

    assert_eq!(cli.public_hosts, ["example.ngrok.app", "localhost:9000"]);
    assert_eq!(cli.allowed_origins, ["https://chatgpt.com"]);
}

#[test]
fn rejects_non_mcp_endpoint_and_non_loopback_bind() {
    assert!(matches!(
        Cli::parse_from(["mcp-agent", "--endpoint", "/other"]),
        Err(CliError::Endpoint)
    ));
    assert!(matches!(
        Cli::parse_from(["mcp-agent", "--bind", "0.0.0.0:8000"]),
        Err(CliError::NonLoopbackBind)
    ));
}

#[test]
fn exposure_warning_names_every_persistent_local_risk() {
    let warning = mcp_agent::startup::EXPOSURE_WARNING;
    for risk in [
        "command execution",
        "host reads",
        "workspace writes",
        "local-service effects",
        "project/global skill installation",
        "development-only",
    ] {
        assert!(warning.contains(risk), "missing risk: {risk}");
    }
}
