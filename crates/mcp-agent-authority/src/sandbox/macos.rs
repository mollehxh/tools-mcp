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
// host reads/process/network, and grant writes only beneath startup-canonical
// capability roots. The marker is expanded into one `require-any` expression.
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
  ;; WRITABLE_ROOT_RULES
)
"#;

pub(super) fn render_policy(sandbox: &Sandbox) -> Result<String, SandboxError> {
    let roots = sandbox.writable_roots()?;
    let rules = roots
        .iter()
        .enumerate()
        .map(|(index, _)| format!("    (subpath (param \"WRITABLE_ROOT_{index}\"))"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(POLICY.replace(";; WRITABLE_ROOT_RULES", &format!("(require-any\n{rules})")))
}

#[allow(clippy::unnecessary_wraps)]
pub(super) fn command(
    sandbox: &Sandbox,
    launcher: &Path,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<Command, SandboxError> {
    let policy = render_policy(sandbox)?;
    let roots = sandbox.writable_roots()?;
    let mut command = Command::new(launcher);
    command.arg("-p").arg(policy);
    for (index, root) in roots.iter().enumerate() {
        command
            .arg("-D")
            .arg(format!("WRITABLE_ROOT_{index}={}", root.display()));
    }
    command.arg(program).args(args).current_dir(cwd);
    Ok(command)
}
