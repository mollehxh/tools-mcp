use anyhow::{Context as _, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mcp_agent_gateway::oauth_http::{OAuthHttpConfig, OAuthHttpService, require_bearer};
use mcp_agent_gateway::{
    AllocationKind, AuthConfig, AuthStore, BackendKind, GatewayBackend, LocalLease, Observability,
    RouteContext, StateStore,
};
use mcp_agent_relay::{GatewaySocketConfig, MtlsAcceptor, Platform, accept_gateway_authenticated};
use mcp_agent_server::ApplicationContext;
use mcp_agent_server::http::{HttpConfig, router};
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, ToolBackend, ToolRequest,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_RELAY_CONNECTIONS: usize = 64;
const RELAY_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_LEASE_TTL: Duration = Duration::from_secs(4);

struct UnavailableBackend;

#[derive(Clone)]
struct RelayRuntime {
    acceptor: Arc<MtlsAcceptor<AuthStore>>,
    auth_store: Arc<AuthStore>,
    gateway: Arc<GatewayBackend>,
    state: Arc<StateStore>,
    ready: Arc<AtomicBool>,
    observability: Arc<Observability>,
    socket_config: GatewaySocketConfig,
}

impl ToolBackend for UnavailableBackend {
    fn call(&self, _context: CallContext, _request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async {
            Err(BackendError::new(
                "backend_unavailable",
                "the VPS runner is not ready",
            ))
        })
    }
}

struct Settings {
    issuer: String,
    client_id: String,
    redirect_uri: String,
    owner_secret_phc: String,
    token_hash_key: Vec<u8>,
    database: PathBuf,
    watermark: PathBuf,
    tls_cert: PathBuf,
    tls_key: PathBuf,
    device_ca: PathBuf,
    mcp_bind: SocketAddr,
    relay_bind: SocketAddr,
    trusted_header_salt: Vec<u8>,
    relay_manifest_digests: Vec<String>,
    host_metrics: PathBuf,
}

#[derive(Clone)]
struct MetricsRuntime {
    observability: Arc<Observability>,
    host_metrics: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::from_env()?;
    let observability = Arc::new(Observability::default());
    observability.gateway_started();
    let state_store = Arc::new(StateStore::open(&settings.database, &settings.watermark)?);
    let oauth_config = OAuthHttpConfig::new(
        &settings.issuer,
        settings.client_id.clone(),
        &settings.redirect_uri,
    )?;
    let auth_store = Arc::new(AuthStore::open(
        &settings.database,
        AuthConfig {
            client_id: settings.client_id.clone(),
            resource: oauth_config.resource().to_owned(),
            owner_secret_phc: settings.owner_secret_phc,
            token_hash_key: settings.token_hash_key,
            access_lifetime: Duration::from_mins(5),
            refresh_lifetime: Duration::from_hours(24 * 30),
        },
    )?);
    let initial_generation = state_store.allocate(AllocationKind::Generation)?;
    let gateway = Arc::new(
        GatewayBackend::new(
            Arc::new(UnavailableBackend),
            RouteContext {
                kind: BackendKind::Vps,
                workspace_id: "vps-starting".to_owned(),
                generation: initial_generation,
                operating_system: "linux".to_owned(),
                privilege_posture: "unavailable".to_owned(),
            },
            LOCAL_LEASE_TTL,
        )
        .with_state_store(Arc::clone(&state_store))
        .with_observability(Arc::clone(&observability)),
    );
    let oauth = OAuthHttpService::new(oauth_config, Arc::clone(&auth_store))
        .with_observability(Arc::clone(&observability));
    let ready = Arc::new(AtomicBool::new(false));
    let cancellation = CancellationToken::new();
    let app = application_router(
        Arc::clone(&gateway),
        &oauth,
        Arc::clone(&ready),
        Arc::clone(&observability),
        settings.host_metrics.clone(),
        settings.trusted_header_salt,
        cancellation.child_token(),
    )?;
    let tls =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&settings.tls_cert, &settings.tls_key)
            .await
            .context("load gateway TLS identity")?;
    let relay_acceptor = Arc::new(MtlsAcceptor::new(
        first_certificate(&settings.device_ca)?,
        certificates(&settings.tls_cert)?,
        private_key(&settings.tls_key)?,
        Arc::clone(&auth_store),
    )?);
    let relay_socket_config = GatewaySocketConfig::new(settings.relay_manifest_digests)
        .context("configure exact relay system-skill manifest allowlist")?;
    let relay = tokio::spawn(relay_loop(
        settings.relay_bind,
        RelayRuntime {
            acceptor: relay_acceptor,
            auth_store: Arc::clone(&auth_store),
            gateway: Arc::clone(&gateway),
            state: state_store,
            ready,
            observability,
            socket_config: relay_socket_config,
        },
        cancellation.child_token(),
    ));
    let authority_monitor = tokio::spawn(monitor_grant_revocation(
        Arc::clone(&auth_store),
        Arc::clone(&gateway),
        cancellation.child_token(),
    ));
    let shutdown = cancellation.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        shutdown.cancel();
    });
    let server = axum_server::bind_rustls(settings.mcp_bind, tls).serve(app.into_make_service());
    tokio::select! {
        result = server => result.context("gateway HTTPS server failed")?,
        result = relay => result.context("relay listener task failed")??,
        result = authority_monitor => result.context("grant revocation monitor failed")??,
        () = cancellation.cancelled() => {}
    }
    cancellation.cancel();
    Ok(())
}

async fn monitor_grant_revocation(
    auth_store: Arc<AuthStore>,
    gateway: Arc<GatewayBackend>,
    cancellation: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                let active = auth_store
                    .active_principal_fingerprints()
                    .context("read active OAuth grants")?
                    .into_iter()
                    .collect::<HashSet<_>>();
                gateway
                    .reconcile_principals(&active)
                    .await
                    .context("terminate revoked OAuth authority")?;
            }
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn application_router(
    gateway: Arc<GatewayBackend>,
    oauth: &OAuthHttpService,
    ready: Arc<AtomicBool>,
    observability: Arc<Observability>,
    host_metrics: PathBuf,
    trusted_header_salt: Vec<u8>,
    cancellation: CancellationToken,
) -> Result<Router> {
    let public_host = oauth
        .issuer()
        .host_str()
        .context("OAuth issuer has no host")?
        .to_owned();
    let http = HttpConfig {
        allowed_hosts: vec![
            public_host,
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ],
        trusted_openai_header_salt: Some(trusted_header_salt),
        ..HttpConfig::default()
    };
    let protected = router(
        Arc::new(ApplicationContext::new(gateway)),
        http,
        cancellation,
    )?
    .layer(from_fn_with_state(oauth.clone(), require_bearer))
    .layer(from_fn_with_state(Arc::clone(&ready), require_ready));
    let health = Router::new()
        .route("/healthz", get(healthz))
        .with_state(ready);
    let metrics = Router::new()
        .route("/metrics", get(metrics))
        .with_state(MetricsRuntime {
            observability: Arc::clone(&observability),
            host_metrics,
        });
    Ok(oauth
        .routes()
        .merge(health)
        .merge(metrics)
        .merge(protected)
        .layer(from_fn_with_state(observability, observe_http)))
}

async fn metrics(State(runtime): State<MetricsRuntime>) -> Response {
    let Ok(host_metrics) = std::fs::read_to_string(&runtime.host_metrics) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "host metrics unavailable").into_response();
    };
    match runtime.observability.render_metrics(Some(&host_metrics)) {
        Ok(metrics) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
            metrics,
        )
            .into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "host metrics invalid").into_response(),
    }
}

async fn observe_http(
    State(observability): State<Arc<Observability>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let response = next.run(request).await;
    observability.record_http(response.status().as_u16(), started.elapsed());
    response
}

async fn healthz(State(ready): State<Arc<AtomicBool>>) -> StatusCode {
    if ready.load(Ordering::Acquire) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn require_ready(
    State(ready): State<Arc<AtomicBool>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if ready.load(Ordering::Acquire) {
        next.run(request).await
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "VPS runner is not ready").into_response()
    }
}

async fn relay_loop(
    bind: SocketAddr,
    runtime: RelayRuntime,
    cancellation: CancellationToken,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind relay listener {bind}"))?;
    let connection_slots = Arc::new(tokio::sync::Semaphore::new(MAX_RELAY_CONNECTIONS));
    loop {
        let (stream, _) = tokio::select! {
            () = cancellation.cancelled() => break,
            accepted = listener.accept() => accepted.context("accept relay TCP connection")?,
        };
        let Ok(connection_slot) = Arc::clone(&connection_slots).try_acquire_owned() else {
            runtime
                .observability
                .record_relay_rejection(true, "connection_saturated");
            continue;
        };
        tokio::spawn(handle_relay_connection(
            stream,
            connection_slot,
            runtime.clone(),
            cancellation.child_token(),
        ));
    }
    Ok(())
}

async fn handle_relay_connection(
    stream: tokio::net::TcpStream,
    connection_slot: tokio::sync::OwnedSemaphorePermit,
    runtime: RelayRuntime,
    connection_cancellation: CancellationToken,
) {
    let _connection_slot = connection_slot;
    let Ok(Ok((stream, peer))) =
        tokio::time::timeout(RELAY_TLS_HANDSHAKE_TIMEOUT, runtime.acceptor.accept(stream)).await
    else {
        runtime
            .observability
            .record_relay_rejection(false, "tls_handshake_failed");
        return;
    };
    let Ok(generation) = runtime.state.allocate(AllocationKind::Generation) else {
        runtime
            .observability
            .record_relay_rejection(false, "state_unavailable");
        return;
    };
    let certificate_fingerprint = peer.certificate_fingerprint.clone();
    let revoke_connection = connection_cancellation.clone();
    let Ok((registration, remote)) = accept_gateway_authenticated(
        stream,
        peer,
        generation,
        runtime.socket_config.clone(),
        connection_cancellation,
    )
    .await
    else {
        runtime
            .observability
            .record_relay_rejection(false, "registration_rejected");
        return;
    };
    runtime.observability.relay_connection_opened();
    observe_relay_close(remote.clone(), Arc::clone(&runtime.observability));
    let context = RouteContext {
        kind: if registration.platform == Platform::LinuxVps {
            BackendKind::Vps
        } else {
            BackendKind::Local
        },
        workspace_id: registration.workspace_id.clone(),
        generation,
        operating_system: platform_name(registration.platform).to_owned(),
        privilege_posture: registration.containment_posture.clone(),
    };
    let backend: Arc<dyn ToolBackend> = remote.clone();
    let published = if context.kind == BackendKind::Vps {
        publish_vps(&runtime, backend, context, remote.clone(), generation)
    } else {
        publish_local(&runtime, backend, context, remote.clone(), &registration)
    };
    if published {
        tokio::spawn(monitor_device_revocation(
            Arc::clone(&runtime.auth_store),
            certificate_fingerprint,
            remote,
            revoke_connection,
        ));
    } else {
        runtime
            .observability
            .record_relay_rejection(false, "registration_rejected");
        let _ = remote.revoke("relay registration was rejected").await;
    }
}

fn observe_relay_close(
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    observability: Arc<Observability>,
) {
    tokio::spawn(async move {
        remote.connection_lost().cancelled().await;
        observability.relay_connection_closed();
    });
}

fn publish_vps(
    runtime: &RelayRuntime,
    backend: Arc<dyn ToolBackend>,
    context: RouteContext,
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    generation: u64,
) -> bool {
    if runtime
        .gateway
        .replace_vps_preallocated(backend, context)
        .is_err()
    {
        return false;
    }
    runtime.ready.store(true, Ordering::Release);
    runtime.observability.set_ready(true);
    tokio::spawn(monitor_vps(
        Arc::clone(&runtime.gateway),
        remote,
        generation,
        Arc::clone(&runtime.ready),
        Arc::clone(&runtime.observability),
    ));
    true
}

fn publish_local(
    runtime: &RelayRuntime,
    backend: Arc<dyn ToolBackend>,
    context: RouteContext,
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    registration: &mcp_agent_relay::Register,
) -> bool {
    let Ok(lease) = runtime.gateway.register_local_preallocated(
        &registration.launch_instance_id,
        registration.connection_epoch,
        backend,
        context,
    ) else {
        return false;
    };
    tokio::spawn(monitor_local(Arc::clone(&runtime.gateway), remote, lease));
    true
}

async fn monitor_device_revocation(
    auth_store: Arc<AuthStore>,
    certificate_fingerprint: String,
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    cancellation: CancellationToken,
) {
    let connection_lost = remote.connection_lost();
    loop {
        tokio::select! {
            () = connection_lost.cancelled() => break,
            () = tokio::time::sleep(Duration::from_secs(1)) => {
                if !auth_store
                    .resolve_device(&certificate_fingerprint)
                    .is_ok_and(|device| device.is_some())
                {
                    let _ = remote.revoke("device certificate was revoked").await;
                    cancellation.cancel();
                    break;
                }
            }
        }
    }
}

async fn monitor_vps(
    gateway: Arc<GatewayBackend>,
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    generation: u64,
    ready: Arc<AtomicBool>,
    observability: Arc<Observability>,
) {
    remote.connection_lost().cancelled().await;
    if gateway.current_vps_generation() == generation {
        gateway.fence_vps_generation(generation);
        ready.store(false, Ordering::Release);
        observability.set_ready(false);
    }
}

async fn monitor_local(
    gateway: Arc<GatewayBackend>,
    remote: Arc<mcp_agent_relay::RemoteBackend>,
    lease: LocalLease,
) {
    let mut observed = remote.heartbeat_count();
    loop {
        tokio::select! {
            () = lease.cancelled() => {
                let _ = remote.revoke("local connection was superseded").await;
                break;
            }
            heartbeat = remote.wait_for_heartbeat_after(observed) => {
                let Some(next) = heartbeat else {
                    break;
                };
                if gateway.heartbeat(&lease).is_err() {
                    let _ = remote.revoke("local lease was lost").await;
                    break;
                }
                observed = next;
            }
            () = tokio::time::sleep(LOCAL_LEASE_TTL) => {
                // The timeout is measured from the last accepted application heartbeat.
                // Fence even if the TCP/WebSocket remains open, then close the worker
                // before the five-second replacement bound.
                if gateway.expire_local_lease(&lease) {
                    let _ = remote.revoke("local lease heartbeat expired").await;
                    break;
                }
            }
        }
    }
}

const fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Macos => "macos",
        Platform::Windows => "windows",
        Platform::LinuxVps => "linux",
    }
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .collect::<std::io::Result<Vec<_>>>()
        .context("read PEM certificate chain")
}

fn first_certificate(path: &Path) -> Result<CertificateDer<'static>> {
    certificates(path)?
        .into_iter()
        .next()
        .context("PEM certificate file is empty")
}

fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?
        .context("PEM private-key file does not contain a supported key")
}

impl Settings {
    fn from_env() -> Result<Self> {
        Ok(Self {
            issuer: required("TOOLS_MCP_ISSUER")?,
            client_id: required("TOOLS_MCP_CLIENT_ID")?,
            redirect_uri: required("TOOLS_MCP_REDIRECT_URI")?,
            owner_secret_phc: std::fs::read_to_string(required_path("TOOLS_MCP_OWNER_HASH_FILE")?)?
                .trim()
                .to_owned(),
            token_hash_key: std::fs::read(required_path("TOOLS_MCP_TOKEN_HASH_KEY_FILE")?)?,
            database: required_path("TOOLS_MCP_DATABASE")?,
            watermark: required_path("TOOLS_MCP_WATERMARK")?,
            tls_cert: required_path("TOOLS_MCP_TLS_CERT")?,
            tls_key: required_path("TOOLS_MCP_TLS_KEY")?,
            device_ca: required_path("TOOLS_MCP_DEVICE_CA")?,
            mcp_bind: optional("TOOLS_MCP_BIND", "127.0.0.1:8443").parse()?,
            relay_bind: optional("TOOLS_MCP_RELAY_BIND", "0.0.0.0:8444").parse()?,
            trusted_header_salt: required("TOOLS_MCP_OPENAI_HEADER_SALT")?.into_bytes(),
            relay_manifest_digests: required("TOOLS_MCP_SYSTEM_SKILL_MANIFEST_ALLOWLIST")?
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            host_metrics: PathBuf::from(optional(
                "TOOLS_MCP_HOST_METRICS_FILE",
                "/var/lib/tools-mcp/metrics/host.prom",
            )),
        })
    }
}

fn required(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("required environment variable {name} is missing"),
    }
}

fn required_path(name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(required(name)?))
}

fn optional(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn metrics_endpoint_fails_closed_for_missing_or_untrusted_host_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("host.prom");
        let runtime = MetricsRuntime {
            observability: Arc::new(Observability::test_instance()),
            host_metrics: snapshot.clone(),
        };
        assert_eq!(
            metrics(State(runtime.clone())).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        std::fs::write(&snapshot, "tools_mcp_probe{secret=\"token\"} 1\n").unwrap();
        let response = metrics(State(runtime.clone())).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1_024).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("token"));

        std::fs::write(&snapshot, "tools_mcp_collector_ok 1\n").unwrap();
        let response = metrics(State(runtime)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("tools_mcp_collector_ok 1"));
    }
}
