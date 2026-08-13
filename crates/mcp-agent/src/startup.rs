use crate::cli::Cli;
use anyhow::{Context, Result, bail};
use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_authority::sandbox::{PreflightReceipt, Sandbox};
use mcp_agent_server::ApplicationContext;
use mcp_agent_server::http::{HttpConfig, MCP_ENDPOINT, router};
use skill_store::{SkillCatalog, SkillInstaller};
use std::collections::hash_map::DefaultHasher;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

pub const EXPOSURE_WARNING: &str = "WARNING: possession of a tunnel URL grants command execution, host reads, workspace writes, local-service effects, and durable project/global skill installation. Unauthenticated tunnels are development-only.";

/// Constructs all long-lived capabilities, serves MCP, and shuts down in order.
///
/// # Errors
///
/// Returns an error when authority, sandbox preflight, capability construction,
/// binding, or HTTP serving fails.
pub async fn run(cli: Cli) -> Result<()> {
    let workspace = std::env::current_dir().context("current directory is unavailable")?;
    let authority = WorkspaceAuthority::new(&workspace)
        .context("fixed workspace authority could not be established")?;
    let release = release_dir(&cli)?;
    let sentinel = OutsideSentinel::create(authority.workspace_root())?;
    let sandbox = Sandbox::load(authority.clone(), &release)
        .context("sandbox release assets failed verification")?;
    let (sandbox, receipt) = sandbox
        .preflight(sentinel.path())
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
    let app = router(context, http, cancellation.child_token())?;
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
    let shutdown_token = cancellation.clone();
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_token.cancelled_owned())
        .await;
    cancellation.cancel();
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

struct OutsideSentinel {
    path: PathBuf,
}

impl OutsideSentinel {
    fn create(workspace: &Path) -> Result<Self> {
        let temp = std::env::temp_dir()
            .canonicalize()
            .context("system temporary directory is unavailable")?;
        if temp.starts_with(workspace) {
            bail!("sandbox preflight needs a temporary directory outside the workspace");
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for sequence in 0..128_u8 {
            let path = temp.join(format!(
                "mcp-agent-preflight-{}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(b"outside sentinel")?;
                    file.sync_all()?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("outside sentinel could not be created"),
            }
        }
        bail!("outside sentinel name space is exhausted")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OutsideSentinel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
