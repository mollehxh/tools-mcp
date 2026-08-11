use anyhow::{Context, ensure};
use std::process::Command;

/// Runs the shared conformance gates that are implemented by completed units.
pub fn run() -> anyhow::Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "test",
            "-p",
            "mcp-agent-authority",
            "--test",
            "workspace_write_security",
            "--test",
            "platform_sandbox",
        ])
        .status()
        .context("run workspace-write conformance")?;
    ensure!(status.success(), "workspace-write conformance failed");
    Ok(())
}
