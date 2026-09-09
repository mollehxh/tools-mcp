//! VPS wrapper that couples the five-tool backend to fail-safe container cleanup.

use mcp_agent_local_backend::LocalBackend;
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, ToolBackend, ToolRequest,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PodmanControlConfig {
    pub executable: PathBuf,
    pub container: String,
    pub preflight: PathBuf,
    pub configure_egress: PathBuf,
}

#[derive(Clone)]
pub struct PodmanControl {
    config: Arc<PodmanControlConfig>,
}

impl PodmanControl {
    /// Creates a fixed Podman control capability.
    ///
    /// # Errors
    ///
    /// Returns an error when paths or the container identifier are invalid.
    pub fn new(mut config: PodmanControlConfig) -> Result<Self, BackendError> {
        config.executable = canonical_file(&config.executable, "Podman executable")?;
        config.preflight = canonical_file(&config.preflight, "Podman preflight")?;
        config.configure_egress =
            canonical_file(&config.configure_egress, "Podman egress configurator")?;
        if config.container.is_empty()
            || config.container.len() > 128
            || !config
                .container
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(control_error("container identifier is invalid"));
        }
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Proves the host-side rootless containment contract before relay registration.
    ///
    /// # Errors
    ///
    /// Returns an error when any preflight assertion fails.
    pub async fn verify(&self) -> Result<(), BackendError> {
        let status = tokio::process::Command::new(&self.config.preflight)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|_| control_error("container preflight could not start"))?;
        if status.success() {
            Ok(())
        } else {
            Err(control_error("container preflight rejected the runtime"))
        }
    }

    /// Restarts the persistent container to guarantee descendant cleanup, then re-verifies it.
    ///
    /// # Errors
    ///
    /// Returns an error when restart or the post-restart containment proof fails.
    pub async fn restart_and_verify(&self) -> Result<(), BackendError> {
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new(&self.config.executable)
                .args(["restart", "--time", "5"])
                .arg(&self.config.container)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await
        .map_err(|_| control_error("container cleanup timed out"))?
        .map_err(|_| control_error("container cleanup could not start"))?;
        if !status.success() {
            return Err(control_error("container cleanup restart failed"));
        }
        let egress = tokio::process::Command::new(&self.config.configure_egress)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|_| control_error("container egress configuration could not start"))?;
        if !egress.success() {
            return Err(control_error("container egress configuration failed"));
        }
        self.verify().await
    }
}

#[derive(Clone)]
pub struct VpsBackend {
    local: LocalBackend,
    control: PodmanControl,
}

impl VpsBackend {
    #[must_use]
    pub const fn new(local: LocalBackend, control: PodmanControl) -> Self {
        Self { local, control }
    }

    /// Stops host-side sessions and restarts the container to remove untracked descendants.
    ///
    /// # Errors
    ///
    /// Returns an error if post-loss container cleanup cannot be proven.
    pub async fn shutdown_and_clean(&self) -> Result<(), BackendError> {
        self.local.shutdown().await;
        self.control.restart_and_verify().await
    }
}

impl ToolBackend for VpsBackend {
    fn call(&self, context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            let cancellation = context.cancellation.clone();
            let was_cancelled_before_dispatch = cancellation.is_cancelled();
            let requires_forced_cleanup = matches!(
                &request,
                ToolRequest::WriteStdin(input) if input.chars == "\u{3}"
            ) || matches!(&request, ToolRequest::TerminateSession(_));
            let result = self.local.call(context, request).await;
            if requires_forced_cleanup
                || (!was_cancelled_before_dispatch && cancellation.is_cancelled())
            {
                self.control.restart_and_verify().await?;
            }
            result
        })
    }
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, BackendError> {
    let path = path
        .canonicalize()
        .map_err(|_| control_error(&format!("{label} is unavailable")))?;
    if !path
        .metadata()
        .map_err(|_| control_error(&format!("{label} metadata is unavailable")))?
        .is_file()
    {
        return Err(control_error(&format!("{label} is not a file")));
    }
    Ok(path)
}

fn control_error(message: &str) -> BackendError {
    BackendError::new("containment_unavailable", message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{PodmanControl, PodmanControlConfig};
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn restart_is_bounded_and_requires_post_restart_preflight() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("restart.marker");
        let egress_marker = root.path().join("egress.marker");
        let podman = root.path().join("podman");
        let preflight = root.path().join("preflight");
        let configure_egress = root.path().join("configure-egress");
        std::fs::write(
            &podman,
            format!(
                "#!/bin/sh\nset -eu\nprintf restarted > '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(&preflight, "#!/bin/sh\nset -eu\nexit 0\n").unwrap();
        std::fs::write(
            &configure_egress,
            format!(
                "#!/bin/sh\nset -eu\nprintf egress > '{}'\n",
                egress_marker.display()
            ),
        )
        .unwrap();
        for path in [&podman, &preflight, &configure_egress] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let control = PodmanControl::new(PodmanControlConfig {
            executable: podman,
            container: "tools-mcp-workspace".to_owned(),
            preflight,
            configure_egress,
        })
        .unwrap();
        control.verify().await.unwrap();
        control.restart_and_verify().await.unwrap();
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "restarted");
        assert_eq!(std::fs::read_to_string(egress_marker).unwrap(), "egress");
    }

    #[tokio::test]
    async fn failed_preflight_never_becomes_eligible() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("podman");
        let preflight = root.path().join("preflight");
        let configure_egress = root.path().join("configure-egress");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(&preflight, "#!/bin/sh\nexit 9\n").unwrap();
        std::fs::write(&configure_egress, "#!/bin/sh\nexit 0\n").unwrap();
        for path in [&executable, &preflight, &configure_egress] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let control = PodmanControl::new(PodmanControlConfig {
            executable,
            container: "tools-mcp-workspace".to_owned(),
            preflight,
            configure_egress,
        })
        .unwrap();
        assert!(control.verify().await.is_err());
    }
}
