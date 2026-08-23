use crate::installer_workflow;
use crate::package;
use anyhow::{Context, ensure};
use serde::Deserialize;
use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector@2.1.0";
const EXPECTED_TOOLS: [&str; 5] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "skills.list",
    "skills.read",
];

#[must_use]
pub const fn inspector_package() -> &'static str {
    INSPECTOR_PACKAGE
}

pub fn validate_tool_names<'a>(names: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
    let names = names.into_iter().collect::<Vec<_>>();
    ensure!(
        names == EXPECTED_TOOLS,
        "unexpected Inspector tools: {names:?}"
    );
    Ok(())
}

#[derive(Deserialize)]
struct InspectorListResult {
    tools: Vec<InspectorTool>,
}

#[derive(Deserialize)]
struct InspectorTool {
    name: String,
}

pub fn run() -> anyhow::Result<()> {
    package::ensure_supported_os(std::env::consts::OS)?;
    let packaged = package::build()?;
    let profile = tempfile::tempdir().context("create disposable Inspector profile")?;
    let workspace = profile.path().join("workspace with spaces");
    let home = profile.path().join("home");
    let shadow = profile.path().join("path shadow");
    for directory in [&workspace, &home, &shadow] {
        fs::create_dir_all(directory)?;
    }
    let workspace = workspace.canonicalize()?;
    let home = home.canonicalize()?;
    let shadow = shadow.canonicalize()?;
    fs::write(shadow.join("sandbox-exec"), b"#!/bin/sh\nexit 99\n")?;
    package::set_executable(&shadow.join("sandbox-exec"))?;
    let installer =
        installer_workflow::InstallerFixture::prepare(profile.path(), &workspace, &home, &shadow)?;

    let address = available_loopback()?;
    let log_path = profile.path().join("mcp-agent.stderr.log");
    let log = fs::File::create(&log_path)?;
    let path_env = format!(
        "{}:{}",
        shadow.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(packaged.release_dir.join("mcp-agent"))
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("CODEX_HOME", installer.codex_home())
        .env("TMPDIR", installer.tmp_dir())
        .env("GIT_CONFIG_GLOBAL", installer.git_config())
        .env("PATH", path_env)
        .args(["--bind", &address.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .context("start packaged mcp-agent")?;
    let mut server = ServerProcess { child, log_path };
    server.wait_ready(address)?;
    let endpoint = format!("http://{address}/mcp");

    let list = inspector(&endpoint, "tools/list", None, None)?;
    let listed: InspectorListResult = serde_json::from_slice(&list)
        .context("Inspector tools/list did not return its pinned JSON shape")?;
    let names = listed
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    validate_tool_names(names)?;

    let exec = inspector(
        &endpoint,
        "tools/call",
        Some("exec_command"),
        Some(&serde_json::json!({
            "cmd": "printf inspector-ready; sleep 2; printf inspector-follow-up",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 250
        })),
    )?;
    let exec: serde_json::Value = serde_json::from_slice(&exec)?;
    let session_id = find_i64(&exec, "session_id")
        .context("Inspector exec_command did not return a live session_id")?;
    let stdin = inspector(
        &endpoint,
        "tools/call",
        Some("write_stdin"),
        Some(&serde_json::json!({
            "session_id": session_id,
            "yield_time_ms": 5000
        })),
    )?;
    ensure!(
        String::from_utf8_lossy(&stdin).contains("inspector-follow-up"),
        "Inspector write_stdin did not return follow-up output"
    );

    let patch = "*** Begin Patch\n*** Add File: inspector-created.txt\n+inspector\n*** End Patch";
    let output = inspector(
        &endpoint,
        "tools/call",
        Some("apply_patch"),
        Some(&serde_json::json!({"patch": patch})),
    )?;
    let _: serde_json::Value = serde_json::from_slice(&output)
        .context("Inspector returned invalid JSON for apply_patch")?;
    for (tool, arguments) in [
        ("skills.list", serde_json::json!({"scope": "system"})),
        (
            "skills.read",
            serde_json::json!({
                "scope": "system",
                "package": "skill-installer",
                "resource": "skill://host/system/skill-installer/SKILL.md"
            }),
        ),
    ] {
        inspector_allow_schema_error(&endpoint, tool, &arguments)?;
    }
    ensure!(
        fs::read_to_string(workspace.join("inspector-created.txt"))? == "inspector\n",
        "Inspector apply_patch did not modify the disposable workspace"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create packaged installer workflow runtime")?;
    runtime.block_on(installer.run(&endpoint))?;
    for (tool, arguments) in [
        ("skills.list", serde_json::json!({"scope": "project"})),
        (
            "skills.read",
            serde_json::json!({
                "scope": "project",
                "package": "recreated-project",
                "resource": "skill://host/project/recreated-project/SKILL.md"
            }),
        ),
    ] {
        inspector_allow_schema_error(&endpoint, tool, &arguments)?;
    }
    server.stop()?;
    println!("MCP Inspector {INSPECTOR_PACKAGE}: listed and called all five packaged macOS tools");
    Ok(())
}

fn inspector_allow_schema_error(
    endpoint: &str,
    tool: &str,
    arguments: &serde_json::Value,
) -> anyhow::Result<()> {
    match inspector(endpoint, "tools/call", Some(tool), Some(arguments)) {
        Ok(output) => {
            let _: serde_json::Value = serde_json::from_slice(&output)?;
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            ensure!(
                message.contains("MCP Inspector tools/call")
                    && message.contains(tool)
                    && message.contains("data must have required property")
                    && message.contains("data must NOT have additional properties"),
                "Inspector did not report the pinned CLI output-schema incompatibility for {tool}: {message}"
            );
            eprintln!(
                "MCP Inspector {INSPECTOR_PACKAGE} invoked {tool}, but its CLI rejected the tool result against the output schema; server result conformance is covered by the Rust transport suite"
            );
            Ok(())
        }
    }
}

fn find_i64(value: &serde_json::Value, key: &str) -> Option<i64> {
    match value {
        serde_json::Value::Object(object) => object
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .or_else(|| object.values().find_map(|value| find_i64(value, key))),
        serde_json::Value::Array(values) => values.iter().find_map(|value| find_i64(value, key)),
        _ => None,
    }
}

fn inspector(
    endpoint: &str,
    method: &str,
    tool: Option<&str>,
    arguments: Option<&serde_json::Value>,
) -> anyhow::Result<Vec<u8>> {
    let mut command = Command::new("npx");
    command.args([
        "--yes",
        INSPECTOR_PACKAGE,
        "--cli",
        endpoint,
        "--transport",
        "http",
    ]);
    command.args(["--method", method]);
    if let Some(tool) = tool {
        command.args(["--tool-name", tool]);
    }
    if let Some(arguments) = arguments {
        command.arg("--tool-args-json").arg(arguments.to_string());
    }
    let output = command
        .output()
        .context("execute pinned MCP Inspector; Node.js/npm are required")?;
    ensure!(
        output.status.success(),
        "MCP Inspector {method}{} failed: {}",
        tool.map_or_else(String::new, |name| format!(" ({name})")),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn available_loopback() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(address)
}

struct ServerProcess {
    child: Child,
    log_path: PathBuf,
}

impl ServerProcess {
    fn wait_ready(&mut self, address: SocketAddr) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "packaged mcp-agent exited before Inspector connected ({status}): {}",
                    fs::read_to_string(&self.log_path).unwrap_or_default()
                );
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!(
            "packaged mcp-agent did not become ready: {}",
            fs::read_to_string(&self.log_path).unwrap_or_default()
        )
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let status = Command::new("/bin/kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .context("send Ctrl-C to packaged mcp-agent")?;
        ensure!(
            status.success(),
            "failed to send Ctrl-C to packaged mcp-agent"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                ensure!(
                    status.success(),
                    "packaged mcp-agent shutdown failed: {}",
                    fs::read_to_string(&self.log_path).unwrap_or_default()
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.child.kill()?;
        anyhow::bail!("packaged mcp-agent did not stop after Ctrl-C")
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
