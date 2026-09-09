use super::{CAPABILITY_PROTOCOL, Sandbox, SandboxError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const POLICY_DESCRIPTION: &str = "restricted-token helper; DISABLE_MAX_PRIVILEGE|LUA_TOKEN|WRITE_RESTRICTED; root-derived restricting SID; medium-integrity ceiling; inherited stdio allowlist; suspended child assigned to non-breakaway kill-on-close Job Object before resume";

pub(super) fn packaging_source() -> Option<PathBuf> {
    option_env!("MCP_AGENT_WINDOWS_SANDBOX_HELPER_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/release/mcp-agent-windows-sandbox.exe")
                .canonicalize()
                .ok()
        })
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
        .args(
            sandbox
                .writable_roots()?
                .iter()
                .filter(|root| root.as_path() != sandbox.authority.workspace_root())
                .flat_map(|root| [std::ffi::OsStr::new("--write-root"), root.as_os_str()]),
        )
        .arg("--")
        .arg(program)
        .args(args);
    Ok(command)
}
