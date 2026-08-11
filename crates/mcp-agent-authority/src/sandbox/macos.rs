use super::{CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT, Sandbox, SandboxError};
use std::path::Path;
use std::process::Command;

pub(super) fn artifact_marker() -> Vec<u8> {
    format!(
        "OpenAI Codex Seatbelt policy adapter\ncommit={PINNED_CODEX_COMMIT}\nprotocol={CAPABILITY_PROTOCOL}\n"
    )
    .into_bytes()
}

// Derived from the pinned Codex Seatbelt policy shape: deny by default, permit
// host reads/process/network, and grant writes only under one fixed root while
// excluding metadata and server-owned state both as literals and subpaths.
pub(super) const POLICY: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow file-read*)
(allow process*)
(allow sysctl-read)
(allow mach-lookup)
(allow ipc-posix*)
(allow network-outbound)
(allow network-inbound)
(allow system-socket)
(allow file-write*
  (require-all
    (subpath (param "WORKSPACE"))
    (require-not (literal (param "PROTECTED_GIT")))
    (require-not (subpath (param "PROTECTED_GIT")))
    (require-not (literal (param "PROTECTED_CODEX")))
    (require-not (subpath (param "PROTECTED_CODEX")))
    (require-not (literal (param "PROTECTED_AGENT")))
    (require-not (subpath (param "PROTECTED_AGENT")))
    (require-not (literal (param "PROTECTED_STAGING")))
    (require-not (subpath (param "PROTECTED_STAGING")))))
"#;

#[allow(clippy::unnecessary_wraps)]
pub(super) fn command(
    sandbox: &Sandbox,
    launcher: &Path,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<Command, SandboxError> {
    let workspace = sandbox.authority.workspace_root();
    let protected_agent = workspace.join(".mcp-agent");
    let mut command = Command::new(launcher);
    command
        .arg("-p")
        .arg(POLICY)
        .arg("-D")
        .arg(format!("WORKSPACE={}", workspace.display()))
        .arg("-D")
        .arg(format!(
            "PROTECTED_GIT={}",
            workspace.join(".git").display()
        ))
        .arg("-D")
        .arg(format!(
            "PROTECTED_CODEX={}",
            workspace.join(".codex").display()
        ))
        .arg("-D")
        .arg(format!("PROTECTED_AGENT={}", protected_agent.display()))
        .arg("-D")
        .arg(format!(
            "PROTECTED_STAGING={}",
            protected_agent.join("staging").display()
        ))
        .arg(program)
        .args(args)
        .current_dir(cwd);
    Ok(command)
}
