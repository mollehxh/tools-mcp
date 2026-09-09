use super::{Sandbox, SandboxError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const POLICY_DESCRIPTION: &str = "bwrap: ro-bind /, bind declared writable roots and /dev, inherit network, unshare user and pid";

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
    let roots = sandbox.writable_roots()?;
    let mut command = Command::new(launcher);
    command
        .args(["--die-with-parent", "--unshare-user", "--unshare-pid"])
        .args(["--ro-bind", "/", "/"])
        .args(["--dev-bind", "/dev", "/dev"]);
    for root in roots.iter() {
        command.arg("--bind").arg(root).arg(root);
    }
    command
        .arg("--chdir")
        .arg(cwd)
        .arg("--")
        .arg(program)
        .args(args);
    Ok(command)
}
