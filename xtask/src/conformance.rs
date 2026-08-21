use anyhow::{Context, ensure};
use std::process::Command;

const SUITES: &[(&str, &[&str])] = &[
    (
        "workspace-write",
        &[
            "test",
            "-p",
            "mcp-agent-authority",
            "--test",
            "workspace_write_security",
            "--test",
            "platform_sandbox",
        ],
    ),
    (
        "six-tool transport",
        &[
            "test",
            "-p",
            "mcp-agent-server",
            "--test",
            "tool_contracts",
            "--test",
            "tool_errors",
            "--test",
            "composability",
            "--test",
            "streamable_http",
            "--test",
            "shared_sessions",
        ],
    ),
    (
        "CLI exposure",
        &["test", "-p", "mcp-agent", "--test", "cli"],
    ),
    (
        "macOS package contract",
        &[
            "test",
            "-p",
            "xtask",
            "--test",
            "package",
            "--test",
            "mcp_inspector",
        ],
    ),
];

/// Runs the shared macOS conformance gates for the six-tool server and package.
pub fn run() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::consts::OS == "macos",
        "native conformance currently supports macOS only; Linux and Windows are deferred"
    );
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    for (name, arguments) in SUITES {
        let status = Command::new(&cargo)
            .args(*arguments)
            .status()
            .with_context(|| format!("run {name} conformance"))?;
        ensure!(status.success(), "{name} conformance failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SUITES;

    #[test]
    fn conformance_covers_authority_transport_cli_and_package() {
        let names = SUITES.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "workspace-write",
                "six-tool transport",
                "CLI exposure",
                "macOS package contract"
            ]
        );
    }
}
