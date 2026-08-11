use super::{CAPABILITY_PROTOCOL, Sandbox, SandboxError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const POLICY_DESCRIPTION: &str = "restricted-token/elevated helper; workspace write capability; inherited handles closed; process tree job-owned";

pub(super) fn packaging_source() -> Option<PathBuf> {
    option_env!("MCP_AGENT_WINDOWS_SANDBOX_HELPER_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

pub(super) fn command(
    sandbox: &Sandbox,
    launcher: &Path,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<Command, SandboxError> {
    let mut command = Command::new(launcher);
    command
        .arg("--protocol")
        .arg(CAPABILITY_PROTOCOL)
        .arg("--workspace")
        .arg(sandbox.authority.workspace_root())
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .arg(program)
        .args(args);
    Ok(command)
}
