use anyhow::{Context as _, Result, bail};
use codex_tools_runtime::process::{OwnerId, PodmanLaunchConfig, ProcessManager};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use mcp_agent_local_backend::LocalBackend;
use mcp_agent_relay::{
    AuthenticatedPeer, ERROR_CONTRACT_VERSION, Platform, RELAY_PROTOCOL, RELAY_SUBPROTOCOL,
    RESULT_CONTRACT_VERSION, ReconnectConfig, Register, RelayWorker, WorkerConfig,
    run_reconnecting_outbound_worker, tool_schema_digest,
};
use mcp_agent_vps_backend::{PodmanControl, PodmanControlConfig, VpsBackend};
use rand::RngCore as _;
use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest as _, Sha256};
use skill_store::SkillCatalog;
use std::ffi::OsString;
use std::fs::File;
use std::io::BufReader;
use std::net::ToSocketAddrs as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
struct Settings {
    relay_url: Url,
    relay_ca: PathBuf,
    device_cert: PathBuf,
    device_key: PathBuf,
    device_id: String,
    workspace: PathBuf,
    persistent_home: PathBuf,
    system_skills: PathBuf,
    system_skill_manifest: PathBuf,
    podman: PathBuf,
    preflight: PathBuf,
    configure_egress: PathBuf,
    container: String,
}

pub async fn run() -> Result<()> {
    let settings = Settings::from_env()?;
    let capabilities = Arc::new(resolve_capabilities(&settings)?);
    let authority = WorkspaceAuthority::from_capabilities(Arc::clone(&capabilities))?;
    let processes = Arc::new(ProcessManager::new_podman(PodmanLaunchConfig {
        executable: settings.podman.clone(),
        container: settings.container.clone(),
        host_workspace: settings.workspace.clone(),
        container_workspace: PathBuf::from("/workspace"),
        container_home: PathBuf::from("/home/developer"),
    })?);
    let catalog = Arc::new(SkillCatalog::new(&authority)?);
    let local = LocalBackend::new(
        authority,
        Arc::clone(&processes),
        catalog,
        OwnerId::from("vps-runner"),
    );
    let control = PodmanControl::new(PodmanControlConfig {
        executable: settings.podman.clone(),
        container: settings.container.clone(),
        preflight: settings.preflight.clone(),
        configure_egress: settings.configure_egress.clone(),
    })?;
    control.verify().await?;
    let backend = Arc::new(VpsBackend::new(local, control));
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal.cancel();
    });
    let worker = RelayWorker::new(
        AuthenticatedPeer {
            owner_id: "owner".to_owned(),
            device_id: settings.device_id.clone(),
            certificate_fingerprint: certificate_fingerprint(&settings.device_cert)?,
            platform: Platform::LinuxVps,
        },
        Arc::clone(&backend),
        WorkerConfig::default(),
    );
    let registration = Register {
        min_protocol: RELAY_PROTOCOL,
        max_protocol: RELAY_PROTOCOL,
        tool_schema_digest: tool_schema_digest(),
        result_contract_version: RESULT_CONTRACT_VERSION,
        error_contract_version: ERROR_CONTRACT_VERSION,
        system_skill_manifest_digest: file_digest(&settings.system_skill_manifest)?,
        platform: Platform::LinuxVps,
        workspace_id: workspace_id(&settings.workspace)?,
        containment_posture: "rootless-podman-verified".to_owned(),
        launch_instance_id: random_launch_id(),
        connection_epoch: 1,
    };
    let connector_settings = settings.clone();
    let relay_result = run_reconnecting_outbound_worker(
        worker,
        registration,
        ReconnectConfig::default(),
        cancellation.clone(),
        move || {
            let settings = connector_settings.clone();
            async move { connect(&settings).await }
        },
    )
    .await;
    cancellation.cancel();
    let cleanup_result = backend.shutdown_and_clean().await;
    relay_result.context("VPS relay stopped")?;
    cleanup_result.context("VPS container cleanup failed")?;
    Ok(())
}

async fn wait_for_shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

fn resolve_capabilities(settings: &Settings) -> Result<CapabilitySnapshot> {
    let home = settings.persistent_home.as_os_str().to_owned();
    let codex_home = settings.persistent_home.join(".codex").into_os_string();
    let cargo_home = settings.persistent_home.join(".cargo").into_os_string();
    let gradle_home = settings.persistent_home.join(".gradle").into_os_string();
    CapabilitySnapshot::resolve_configured(
        &settings.workspace,
        &settings.system_skills,
        move |name| match name {
            "HOME" => Some(home.clone()),
            "CODEX_HOME" => Some(codex_home.clone()),
            "CARGO_HOME" => Some(cargo_home.clone()),
            "GRADLE_USER_HOME" => Some(gradle_home.clone()),
            _ => std::env::var_os(name),
        },
        PathBuf::from("/tmp"),
        PathBuf::from("/tmp"),
    )
    .context("resolve persistent VPS capability roots")
}

async fn connect(
    settings: &Settings,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
> {
    if settings.relay_url.scheme() != "wss" {
        bail!("TOOLS_MCP_RELAY_URL must use wss")
    }
    let host = settings
        .relay_url
        .host_str()
        .context("relay URL has no host")?
        .to_owned();
    let port = settings.relay_url.port().unwrap_or(443);
    let address = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .context("relay host did not resolve")?;
    let tcp = tokio::net::TcpStream::connect(address)
        .await
        .context("connect relay TCP")?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates(&settings.relay_ca)? {
        roots.add(certificate)?;
    }
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            certificates(&settings.device_cert)?,
            private_key(&settings.device_key)?,
        )?;
    let server_name = ServerName::try_from(host).context("relay TLS server name is invalid")?;
    let tls = TlsConnector::from(Arc::new(tls_config))
        .connect(server_name, tcp)
        .await
        .context("relay mutual-TLS handshake failed")?;
    let mut request = settings.relay_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        RELAY_SUBPROTOCOL.parse().expect("static protocol is valid"),
    );
    let (socket, response) = tokio_tungstenite::client_async(request, tls).await?;
    if response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok())
        != Some(RELAY_SUBPROTOCOL)
    {
        bail!("relay did not accept the required subprotocol")
    }
    Ok(socket)
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .collect::<std::io::Result<Vec<_>>>()
        .context("read PEM certificates")
}

fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?.context("PEM private key is missing")
}

fn certificate_fingerprint(path: &Path) -> Result<String> {
    let certificate = certificates(path)?
        .into_iter()
        .next()
        .context("device certificate is empty")?;
    Ok(hex_digest(certificate.as_ref()))
}

fn file_digest(path: &Path) -> Result<String> {
    Ok(hex_digest(&std::fs::read(path)?))
}

fn workspace_id(path: &Path) -> Result<String> {
    Ok(hex_digest(
        path.canonicalize()?.as_os_str().as_encoded_bytes(),
    ))
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}

fn random_launch_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex_digest(&bytes)[..32].to_owned()
}

impl Settings {
    fn from_env() -> Result<Self> {
        Ok(Self {
            relay_url: required("TOOLS_MCP_RELAY_URL")?.parse()?,
            relay_ca: required_path("TOOLS_MCP_RELAY_CA")?,
            device_cert: required_path("TOOLS_MCP_DEVICE_CERT")?,
            device_key: required_path("TOOLS_MCP_DEVICE_KEY")?,
            device_id: required("TOOLS_MCP_DEVICE_ID")?,
            workspace: required_path("TOOLS_MCP_WORKSPACE")?,
            persistent_home: required_path("TOOLS_MCP_PERSISTENT_HOME")?,
            system_skills: required_path("TOOLS_MCP_SYSTEM_SKILLS")?,
            system_skill_manifest: required_path("TOOLS_MCP_SYSTEM_SKILL_MANIFEST")?,
            podman: required_path("TOOLS_MCP_PODMAN")?,
            preflight: required_path("TOOLS_MCP_CONTAINER_PREFLIGHT")?,
            configure_egress: required_path("TOOLS_MCP_CONTAINER_EGRESS")?,
            container: required("TOOLS_MCP_CONTAINER")?,
        })
    }
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("required environment variable {name} is missing"))
}

fn required_path(name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(OsString::from(required(name)?)))
}
