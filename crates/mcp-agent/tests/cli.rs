use mcp_agent::cli::{Cli, CliError, Command, DeviceKeySource};

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
fn relay_configuration_is_complete_or_rejected() {
    assert!(matches!(
        Cli::parse_from(["mcp-agent", "--relay-url", "wss://relay.example:8444"]),
        Err(CliError::IncompleteRelay)
    ));
    let cli = Cli::parse_from([
        "mcp-agent",
        "--relay-url",
        "wss://relay.example:8444",
        "--relay-ca",
        "ca.pem",
        "--device-cert",
        "device.pem",
        "--device-key",
        "device.key",
        "--device-id",
        "macbook",
    ])
    .unwrap();
    let relay = cli.relay.unwrap();
    assert_eq!(relay.device_id, "macbook");
    assert_eq!(relay.device_key, DeviceKeySource::Pem("device.key".into()));

    let keychain = Cli::parse_from([
        "mcp-agent",
        "--relay-url",
        "wss://relay.example:8444",
        "--relay-ca",
        "ca.pem",
        "--device-cert",
        "device.pem",
        "--device-keychain-label",
        "tools-mcp-device:macbook",
        "--device-id",
        "macbook",
    ])
    .unwrap();
    assert_eq!(
        keychain.relay.unwrap().device_key,
        DeviceKeySource::MacosKeychain("tools-mcp-device:macbook".into())
    );

    let cng = Cli::parse_from([
        "mcp-agent",
        "--relay-url",
        "wss://relay.example:8444",
        "--relay-ca",
        "ca.pem",
        "--device-cert",
        "device.pem",
        "--device-cng-key-name",
        "tools-mcp-device:windows-pc",
        "--device-id",
        "windows-pc",
    ])
    .unwrap();
    assert_eq!(
        cng.relay.unwrap().device_key,
        DeviceKeySource::WindowsCng("tools-mcp-device:windows-pc".into())
    );

    assert!(matches!(
        Cli::parse_from([
            "mcp-agent",
            "--relay-url",
            "wss://relay.example:8444",
            "--relay-ca",
            "ca.pem",
            "--device-cert",
            "device.pem",
            "--device-key",
            "device.key",
            "--device-keychain-label",
            "tools-mcp-device:macbook",
            "--device-id",
            "macbook",
        ]),
        Err(CliError::IncompleteRelay)
    ));
}

#[test]
fn exposure_warning_names_every_persistent_local_risk() {
    let warning = mcp_agent::startup::EXPOSURE_WARNING;
    for risk in [
        "command execution",
        "host reads",
        "writes across declared workspace/temp/cache/tool roots",
        "unrestricted workload networking and listener binds",
        "project/global skill installation",
        "durable Cargo/Gradle state changes",
        "unreviewed third-party instructions or executable content",
        "development-only",
    ] {
        assert!(warning.contains(risk), "missing risk: {risk}");
    }
}

#[test]
fn parses_human_only_device_enrollment_commands() {
    assert_eq!(
        Command::parse_from([
            "mcp-agent",
            "enroll-device",
            "--device-id",
            "macbook",
            "--csr-output",
            "device.csr",
            "--release-dir",
            "release",
        ])
        .unwrap(),
        Command::EnrollDevice {
            device_id: "macbook".to_owned(),
            csr_output: "device.csr".into(),
            release_dir: Some("release".into()),
        }
    );
    assert_eq!(
        Command::parse_from([
            "mcp-agent",
            "install-device-certificate",
            "--device-id",
            "macbook",
            "--device-cert",
            "device.pem",
        ])
        .unwrap(),
        Command::InstallDeviceCertificate {
            device_id: "macbook".to_owned(),
            certificate: "device.pem".into(),
            release_dir: None,
        }
    );
}
