use super::{Sandbox, SandboxError};
use crate::workspace::PROTECTED_TOP_LEVEL;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const POLICY_DESCRIPTION: &str = "bwrap: ro-bind /, bind workspace, re-apply protected roots read-only, inherit network, unshare user and pid";

pub(super) fn packaging_source() -> Option<PathBuf> {
    option_env!("MCP_AGENT_BWRAP_PATH")
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
    let workspace = sandbox.authority.workspace_root();
    let mut command = Command::new(launcher);
    command
        .args(["--die-with-parent", "--unshare-user", "--unshare-pid"])
        .args(["--ro-bind", "/", "/"])
        .arg("--bind")
        .arg(workspace)
        .arg(workspace);
    append_protected_mounts(&mut command, workspace);
    command
        .arg("--chdir")
        .arg(cwd)
        .arg("--")
        .arg(program)
        .args(args);
    Ok(command)
}

fn append_protected_mounts(command: &mut Command, workspace: &Path) {
    for protected in PROTECTED_TOP_LEVEL {
        let path = workspace.join(protected);
        if path.exists() {
            command.arg("--ro-bind").arg(&path).arg(&path);
        }
    }
}
