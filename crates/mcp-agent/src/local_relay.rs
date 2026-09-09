use crate::cli::{DeviceKeySource, RelayCli};
use anyhow::{Context as _, Result, bail};
use codex_tools_runtime::process::ProcessManager;
use mcp_agent_local_backend::LocalBackend;
use mcp_agent_relay::{
    AuthenticatedPeer, ERROR_CONTRACT_VERSION, Platform, RELAY_PROTOCOL, RELAY_SUBPROTOCOL,
    RESULT_CONTRACT_VERSION, ReconnectConfig, Register, RelayWorker, WorkerConfig,
    run_reconnecting_outbound_worker, tool_schema_digest,
};
use rand::RngCore as _;
use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::BufReader;
use std::net::ToSocketAddrs as _;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::CancellationToken;
use url::Url;

pub async fn run(
    backend: Arc<LocalBackend>,
    processes: Arc<ProcessManager>,
    workspace: &Path,
    release: &Path,
    settings: RelayCli,
    cancellation: CancellationToken,
) -> Result<()> {
    let relay_url: Url = settings.url.parse().context("invalid relay URL")?;
    let worker = RelayWorker::new(
        AuthenticatedPeer {
            owner_id: "owner".to_owned(),
            device_id: settings.device_id.clone(),
            certificate_fingerprint: certificate_fingerprint(&settings.device_cert)?,
            platform: local_platform(),
        },
        backend,
        WorkerConfig::default(),
    );
    let registration = Register {
        min_protocol: RELAY_PROTOCOL,
        max_protocol: RELAY_PROTOCOL,
        tool_schema_digest: tool_schema_digest(),
        result_contract_version: RESULT_CONTRACT_VERSION,
        error_contract_version: ERROR_CONTRACT_VERSION,
        system_skill_manifest_digest: file_digest(&release.join("release-manifest.json"))?,
        platform: local_platform(),
        workspace_id: workspace_id(workspace)?,
        containment_posture: local_containment().to_owned(),
        launch_instance_id: random_launch_id(),
        connection_epoch: 1,
    };
    eprintln!(
        "tools-mcp local worker connecting workspace={} relay={}",
        registration.workspace_id, relay_url
    );
    let connector_url = relay_url.clone();
    let connector_settings = settings.clone();
    let connector_release = release.to_path_buf();
    let relay_result = run_reconnecting_outbound_worker(
        worker,
        registration,
        ReconnectConfig::default(),
        cancellation.clone(),
        move || {
            let relay_url = connector_url.clone();
            let settings = connector_settings.clone();
            let release = connector_release.clone();
            async move { connect(&relay_url, &settings, &release).await }
        },
    )
    .await;
    cancellation.cancel();
    processes.shutdown().await;
    relay_result.context("local relay stopped")
}

async fn connect(
    relay_url: &Url,
    settings: &RelayCli,
    release: &Path,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
> {
    if relay_url.scheme() != "wss" {
        bail!("--relay-url must use wss")
    }
    let host = relay_url
        .host_str()
        .context("relay URL has no host")?
        .to_owned();
    let port = relay_url.port().unwrap_or(443);
    let address = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .context("relay host did not resolve")?;
    let tcp = tokio::net::TcpStream::connect(address)
        .await
        .context("connect relay TCP")?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates(&settings.ca)? {
        roots.add(certificate)?;
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let certificate_chain = certificates(&settings.device_cert)?;
    let tls_config = match &settings.device_key {
        DeviceKeySource::Pem(path) => {
            builder.with_client_auth_cert(certificate_chain, private_key(path)?)?
        }
        DeviceKeySource::MacosKeychain(label) => {
            #[cfg(target_os = "macos")]
            {
                use rustls::sign::SingleCertAndKey;

                let certified_key = crate::macos_keychain::certified_key(certificate_chain, label)?;
                builder.with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key)))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = label;
                bail!("--device-keychain-label is supported only on macOS")
            }
        }
        DeviceKeySource::WindowsCng(key_name) => {
            #[cfg(target_os = "windows")]
            {
                use rustls::sign::SingleCertAndKey;

                let helper = release.join("tools-mcp-keygen.exe");
                let certified_key =
                    crate::windows_cng::certified_key(certificate_chain, key_name, &helper)?;
                builder.with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key)))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (key_name, release);
                bail!("--device-cng-key-name is supported only on Windows")
            }
        }
    };
    let server_name = ServerName::try_from(host).context("relay TLS server name is invalid")?;
    let tls = TlsConnector::from(Arc::new(tls_config))
        .connect(server_name, tcp)
        .await
        .context("relay mutual-TLS handshake failed")?;
    let mut request = relay_url.as_str().into_client_request()?;
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

#[cfg(target_os = "macos")]
const fn local_platform() -> Platform {
    Platform::Macos
}

#[cfg(target_os = "windows")]
const fn local_platform() -> Platform {
    Platform::Windows
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn local_platform() -> Platform {
    panic!("the local outbound worker is supported only on macOS and Windows")
}

#[cfg(target_os = "macos")]
const fn local_containment() -> &'static str {
    "macos-seatbelt-verified"
}

#[cfg(target_os = "windows")]
const fn local_containment() -> &'static str {
    "windows-restricted-token-job-verified"
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const fn local_containment() -> &'static str {
    "unsupported-local-platform"
}
