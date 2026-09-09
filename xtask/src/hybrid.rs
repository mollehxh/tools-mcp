use anyhow::{Context as _, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;

const OAUTH_SUITES: &[(&str, &[&str])] = &[
    (
        "OAuth browser, refresh, revocation, and persistence",
        &["test", "-p", "mcp-agent-gateway", "--test", "auth"],
    ),
    (
        "OAuth HTTP and privacy-safe observability",
        &[
            "test",
            "-p",
            "mcp-agent-gateway",
            "--lib",
            "--test",
            "observability",
        ],
    ),
];

const HYBRID_SUITES: &[(&str, &[&str])] = &[
    (
        "generation routing, affinity, and context fences",
        &[
            "test",
            "-p",
            "mcp-agent-gateway",
            "--test",
            "routing",
            "--test",
            "state",
            "--test",
            "five_tool_conformance",
        ],
    ),
    (
        "relay disposition, mTLS, and reconnect",
        &[
            "test",
            "-p",
            "mcp-agent-relay",
            "--test",
            "protocol",
            "--test",
            "tls",
            "--test",
            "reconnect",
        ],
    ),
    (
        "gateway execution boundary",
        &[
            "test",
            "-p",
            "mcp-agent-server",
            "--test",
            "backend_boundary",
        ],
    ),
];

pub fn oauth_conformance() -> anyhow::Result<()> {
    run_cargo_suites(OAUTH_SUITES)
}

pub fn hybrid_smoke() -> anyhow::Result<()> {
    run_cargo_suites(HYBRID_SUITES)
}

pub fn vps_smoke() -> anyhow::Result<()> {
    ensure!(std::env::consts::OS == "linux", "VPS smoke requires Linux");
    ensure!(
        std::env::var("TOOLS_MCP_LIVE_VPS_SMOKE").as_deref() == Ok("1"),
        "VPS smoke is release evidence and requires TOOLS_MCP_LIVE_VPS_SMOKE=1 on the deployed host"
    );
    ensure!(
        command_output(Command::new("id").arg("-u"), "read effective user")? == "0",
        "VPS smoke must run as root so it can combine host and rootless-user evidence"
    );
    let repository = repository_root()?;
    let release = std::env::var_os("TOOLS_MCP_VPS_RELEASE_ROOT")
        .map_or_else(|| PathBuf::from("/opt/tools-mcp/current"), PathBuf::from);
    ensure!(release.is_dir(), "deployed VPS release root is unavailable");
    run(
        "storage preflight",
        &release.join("libexec/storage-preflight"),
        &[],
    )?;
    run_as_workload(
        "rootless containment preflight",
        &release.join("libexec/rootless-preflight"),
    )?;
    run_as_workload(
        "container verification",
        &release.join("libexec/verify-container"),
    )?;
    run(
        "matched and stale recovery fixture",
        &repository.join("deploy/vps/tests/recovery-fixture"),
        &[],
    )?;
    run(
        "privacy fixture",
        &repository.join("deploy/vps/tests/privacy-fixture"),
        &[],
    )?;
    if let Ok(canary_file) = std::env::var("TOOLS_MCP_PRIVACY_CANARY_FILE") {
        run(
            "live privacy scan",
            &release.join("libexec/privacy-scan"),
            &[canary_file],
        )?;
    }
    Ok(())
}

fn run_cargo_suites(suites: &[(&str, &[&str])]) -> anyhow::Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    for (name, arguments) in suites {
        let status = Command::new(&cargo)
            .args(*arguments)
            .status()
            .with_context(|| format!("run {name}"))?;
        ensure!(status.success(), "{name} failed");
    }
    Ok(())
}

fn run(name: &str, program: &Path, arguments: &[String]) -> anyhow::Result<()> {
    let status = Command::new(program)
        .args(arguments)
        .status()
        .with_context(|| format!("run {name}"))?;
    ensure!(status.success(), "{name} failed");
    Ok(())
}

fn run_as_workload(name: &str, program: &Path) -> anyhow::Result<()> {
    let workload_uid = command_output(
        Command::new("id").args(["-u", "tools-mcp-workload"]),
        "resolve tools-mcp-workload UID",
    )?;
    ensure!(
        workload_uid.bytes().all(|byte| byte.is_ascii_digit()),
        "tools-mcp-workload UID is invalid"
    );
    let status = Command::new("runuser")
        .current_dir("/var/lib/tools-mcp/workload")
        .args(["-u", "tools-mcp-workload", "--", "env"])
        .arg("HOME=/var/lib/tools-mcp/workload")
        .arg(format!("XDG_RUNTIME_DIR=/run/user/{workload_uid}"))
        .arg(format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{workload_uid}/bus"
        ))
        .args(["sh", "-c"])
        .arg("set -a; . /etc/tools-mcp/workload.env; set +a; exec \"$1\"")
        .arg("tools-mcp-vps-smoke")
        .arg(program)
        .status()
        .with_context(|| format!("run {name} as tools-mcp-workload"))?;
    ensure!(status.success(), "{name} failed");
    Ok(())
}

fn command_output(command: &mut Command, name: &str) -> anyhow::Result<String> {
    let output = command.output().with_context(|| format!("run {name}"))?;
    ensure!(output.status.success(), "{name} failed");
    Ok(String::from_utf8(output.stdout)
        .context("command output was not UTF-8")?
        .trim()
        .to_owned())
}

fn repository_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask repository root is unavailable")?
        .to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{HYBRID_SUITES, OAUTH_SUITES};

    #[test]
    fn oauth_gate_covers_auth_http_and_observability() {
        assert_eq!(OAUTH_SUITES.len(), 2);
        let arguments = OAUTH_SUITES
            .iter()
            .flat_map(|(_, arguments)| *arguments)
            .copied()
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"auth"));
        assert!(arguments.contains(&"observability"));
    }

    #[test]
    fn hybrid_gate_covers_gateway_relay_and_adapter_boundary() {
        let names = HYBRID_SUITES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 3);
        assert!(names.iter().any(|name| name.contains("routing")));
        assert!(names.iter().any(|name| name.contains("relay")));
        assert!(names.iter().any(|name| name.contains("execution boundary")));
    }
}
