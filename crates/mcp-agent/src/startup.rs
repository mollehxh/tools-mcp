use crate::cli::Cli;
use anyhow::{Context, Result};
use axum::Router;
use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::release::verify_release;
use mcp_agent_authority::sandbox::{PreflightReceipt, Sandbox};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use mcp_agent_server::ApplicationContext;
use mcp_agent_server::http::{HttpConfig, MCP_ENDPOINT, router};
use skill_store::{SkillCatalog, SkillInstaller};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const EXPOSURE_WARNING: &str = "WARNING: possession of a tunnel URL grants command execution, host reads, writes across declared workspace/temp/cache/tool roots, unrestricted workload networking and listener binds, and durable project/global skill installation. Unauthenticated tunnels are development-only.";

/// Constructs all long-lived capabilities, serves MCP, and shuts down in order.
///
/// # Errors
///
/// Returns an error when authority, sandbox preflight, capability construction,
/// binding, or HTTP serving fails.
pub async fn run(cli: Cli) -> Result<()> {
    let workspace = std::env::current_dir().context("current directory is unavailable")?;
    let release = release_dir(&cli)?;
    if cli.release_dir.is_none() {
        let executable = std::env::current_exe().context("executable path is unavailable")?;
        verify_release(&release, &executable, env!("CARGO_PKG_VERSION"))
            .context("installed release compatibility verification failed")?;
    }
    let capabilities = Arc::new(
        CapabilitySnapshot::resolve(&workspace, release.join("system-skills"))
            .context("managed workload capabilities could not be established")?,
    );
    let authority = WorkspaceAuthority::from_capabilities(capabilities)
        .context("fixed workspace authority could not be established")?;
    #[cfg(target_os = "macos")]
    let sandbox = Sandbox::load_with_reexec(
        authority.clone(),
        &release,
        &std::env::current_exe().context("executable path is unavailable")?,
    )
    .context("sandbox release assets failed verification")?;
    #[cfg(not(target_os = "macos"))]
    let sandbox = Sandbox::load(authority.clone(), &release)
        .context("sandbox release assets failed verification")?;
    let (sandbox, receipt) = sandbox
        .preflight()
        .context("workspace-write sandbox preflight failed")?;

    let processes = Arc::new(ProcessManager::new(Arc::new(sandbox)));
    let catalog = Arc::new(SkillCatalog::new(&authority).context("skill catalog setup failed")?);
    let installer = Arc::new(
        SkillInstaller::new(&authority, Arc::clone(&catalog))
            .context("skill installer setup failed")?,
    );
    let context = Arc::new(ApplicationContext::new(
        authority.clone(),
        Arc::clone(&processes),
        catalog,
        installer,
        OwnerId::from("local-anonymous"),
    ));

    let cancellation = CancellationToken::new();
    let mut http = HttpConfig::default();
    http.allowed_hosts.extend(cli.public_hosts.iter().cloned());
    http.allowed_hosts.push(cli.bind.to_string());
    http.allowed_origins = cli.allowed_origins.clone();
    let app = router(Arc::clone(&context), http, cancellation.child_token())?;
    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("could not bind loopback endpoint at {}", cli.bind))?;
    let address = listener.local_addr().context("bound address unavailable")?;

    print_banner(
        authority.workspace_root(),
        address,
        &receipt,
        &cli.public_hosts,
    );
    let signal_token = cancellation.clone();
    tokio::spawn(async move {
        let _ = crate::shutdown::cancel_on_signal(signal_token).await;
    });
    serve_prepared(listener, app, cancellation, context, processes).await
}

/// Serves an already-prepared application and coordinates graceful shutdown.
///
/// The injectable boundary keeps sandbox and authority construction in
/// [`run`] while making the transport lifecycle independently testable.
///
/// # Errors
///
/// Returns an error when the HTTP server fails.
pub async fn serve_prepared(
    listener: tokio::net::TcpListener,
    app: Router,
    cancellation: CancellationToken,
    context: Arc<ApplicationContext>,
    processes: Arc<ProcessManager>,
) -> Result<()> {
    let shutdown_token = cancellation.clone();
    let shutdown_context = Arc::clone(&context);
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            shutdown_context.cancel_install_operations();
        })
        .await;
    cancellation.cancel();
    context.cancel_install_operations();
    context.wait_for_install_operations().await;
    processes.shutdown().await;
    serve_result.context("HTTP server failed")
}

fn release_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(release) = &cli.release_dir {
        return release
            .canonicalize()
            .context("configured release directory is unavailable");
    }
    let executable = std::env::current_exe().context("executable path is unavailable")?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no release directory")
}

fn print_banner(
    workspace: &Path,
    address: std::net::SocketAddr,
    receipt: &PreflightReceipt,
    public_hosts: &[String],
) {
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    let workspace_id = hasher.finish();
    eprintln!("mcp-agent workspace={workspace_id:016x}");
    eprintln!("MCP endpoint: http://{address}{MCP_ENDPOINT}");
    eprintln!("Sandbox preflight: {receipt:?}");
    eprintln!("{EXPOSURE_WARNING}");
    if public_hosts.is_empty() {
        eprintln!(
            "External example: mcp-agent --public-host <id>.ngrok.app; ngrok http http://{address}"
        );
    } else {
        eprintln!("Allowed public hosts: {}", public_hosts.join(", "));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::serve_prepared;
    use axum::{Router, routing::get};
    use codex_tools_runtime::contracts::ExecCommandInput;
    use codex_tools_runtime::process::{OwnerId, ProcessError, ProcessManager};
    use mcp_agent_authority::WorkspaceAuthority;
    use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
    use mcp_agent_server::ApplicationContext;
    use skill_store::{SkillCatalog, SkillInstaller};
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepared_server_closes_admission_before_draining_and_cleans_processes() {
        let (fixture, context, processes) = fixture();
        let pending = processes
            .exec_command(&OwnerId::from("lifecycle-test"), command("sleep 30"))
            .await
            .unwrap();
        assert!(pending.handoff().await.unwrap().session_id.is_some());

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = Router::new().route(
            "/hold",
            get({
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                move || {
                    let entered = Arc::clone(&entered);
                    let release = Arc::clone(&release);
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        "done"
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = CancellationToken::new();
        let server = tokio::spawn(serve_prepared(
            listener,
            app,
            cancellation.clone(),
            Arc::clone(&context),
            Arc::clone(&processes),
        ));
        let request = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"GET /hold HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        });
        entered.notified().await;

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if context.install_operations_closed()
                    && tokio::net::TcpStream::connect(address).await.is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shutdown must close HTTP and install admission");
        assert!(!server.is_finished(), "in-flight request was not drained");

        release.notify_waiters();
        let response = request.await.unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));
        server.await.unwrap().unwrap();

        let error = processes
            .exec_command(&OwnerId::from("lifecycle-test"), command("printf late"))
            .await
            .unwrap_err();
        assert!(matches!(error, ProcessError::ShuttingDown));
        drop(fixture);
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<ApplicationContext>,
        Arc<ProcessManager>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global");
        let release = root.path().join("release");
        for directory in [&workspace, &global, &release] {
            fs::create_dir(directory).unwrap();
        }
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        expected_manifest()
            .unwrap()
            .write_release_relative(&release)
            .unwrap();
        let sandbox = Sandbox::load(authority.clone(), &release)
            .unwrap()
            .preflight()
            .unwrap()
            .0;
        let processes = Arc::new(ProcessManager::new(Arc::new(sandbox)));
        let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
        let installer = Arc::new(SkillInstaller::new(&authority, Arc::clone(&catalog)).unwrap());
        let context = Arc::new(ApplicationContext::new(
            authority,
            Arc::clone(&processes),
            catalog,
            installer,
            OwnerId::from("lifecycle-test"),
        ));
        (root, context, processes)
    }

    fn command(cmd: &str) -> ExecCommandInput {
        ExecCommandInput {
            cmd: cmd.to_owned(),
            workdir: None,
            tty: false,
            yield_time_ms: 250,
            max_output_tokens: None,
            shell: Some("/bin/sh".to_owned()),
            login: Some(false),
        }
    }
}
