use super::ProcessError;
use super::manager::{command_with_fixed_environment, shell_dialect};
use crate::contracts::ExecCommandInput;
use mcp_agent_authority::sandbox::VerifiedSandbox;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use rand::Rng as _;

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt as _;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd as _;

/// Crate-sealed launch boundary. Only verified platform launchers can reach production managers.
pub(crate) trait CommandLauncher: Send + Sync {
    fn build_command(
        &self,
        input: &ExecCommandInput,
    ) -> Result<std::process::Command, ProcessError>;

    fn workspace_relative_workdir(&self, input: &ExecCommandInput)
    -> Result<PathBuf, ProcessError>;
}

pub(crate) struct VerifiedSandboxLauncher {
    sandbox: Arc<VerifiedSandbox>,
}

impl VerifiedSandboxLauncher {
    pub(crate) fn new(sandbox: Arc<VerifiedSandbox>) -> Self {
        Self { sandbox }
    }
}

impl CommandLauncher for VerifiedSandboxLauncher {
    fn build_command(
        &self,
        input: &ExecCommandInput,
    ) -> Result<std::process::Command, ProcessError> {
        let cwd = input
            .workdir
            .as_deref()
            .filter(|path| !path.is_empty())
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let capabilities = self.sandbox.capabilities();
        #[cfg(windows)]
        let (shell, args) = {
            let shell = input
                .shell
                .clone()
                .unwrap_or_else(|| "powershell.exe".to_owned());
            let args = vec![
                "-NoLogo".to_owned(),
                "-Command".to_owned(),
                input.cmd.clone(),
            ];
            (shell, args)
        };
        #[cfg(not(windows))]
        let (shell, args) = {
            let shell = input
                .shell
                .clone()
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/sh".to_owned());
            let dialect = shell_dialect(&shell)?;
            let command = capabilities.as_ref().map_or_else(
                || Ok(input.cmd.clone()),
                |snapshot| {
                    command_with_fixed_environment(dialect, snapshot.environment(), &input.cmd)
                },
            )?;
            let mode = if input.login.unwrap_or(true) {
                "-lc"
            } else {
                "-c"
            };
            (shell, vec![mode.to_owned(), command])
        };
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let mut command = self
            .sandbox
            .command(&shell, &args, &cwd)
            .and_then(mcp_agent_authority::sandbox::SandboxCommand::into_std_command)
            .map_err(ProcessError::spawn)?;
        if let Some(capabilities) = capabilities {
            command.envs(capabilities.environment());
        }
        Ok(command)
    }

    fn workspace_relative_workdir(
        &self,
        input: &ExecCommandInput,
    ) -> Result<PathBuf, ProcessError> {
        relative_workdir(
            self.sandbox.workspace_root(),
            input.workdir.as_deref(),
            "native workdir must remain inside the launch workspace",
        )
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct PodmanLaunchConfig {
    pub executable: PathBuf,
    pub container: String,
    pub host_workspace: PathBuf,
    pub container_workspace: PathBuf,
    pub container_home: PathBuf,
}

#[cfg(target_os = "linux")]
pub(crate) struct PodmanLauncher {
    config: PodmanLaunchConfig,
    executable: File,
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl PodmanLauncher {
    pub(crate) fn new(mut config: PodmanLaunchConfig) -> Result<Self, ProcessError> {
        if config.container.is_empty()
            || config.container.len() > 128
            || !config
                .container
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !config.container_workspace.is_absolute()
            || !config.container_home.is_absolute()
        {
            return Err(ProcessError::spawn(std::io::Error::other(
                "invalid immutable Podman launch mapping",
            )));
        }
        config.executable = config
            .executable
            .canonicalize()
            .map_err(ProcessError::spawn)?;
        config.host_workspace = config
            .host_workspace
            .canonicalize()
            .map_err(ProcessError::spawn)?;
        let executable = File::open(&config.executable).map_err(ProcessError::spawn)?;
        let metadata = executable.metadata().map_err(ProcessError::spawn)?;
        if !metadata.is_file() {
            return Err(ProcessError::spawn(std::io::Error::other(
                "Podman launcher is not a regular file",
            )));
        }
        Ok(Self {
            config,
            executable,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn reverify(&self) -> Result<(), ProcessError> {
        let current = std::fs::metadata(&self.config.executable).map_err(ProcessError::spawn)?;
        let held = self.executable.metadata().map_err(ProcessError::spawn)?;
        if current.dev() != self.device
            || current.ino() != self.inode
            || held.dev() != self.device
            || held.ino() != self.inode
        {
            return Err(ProcessError::spawn(std::io::Error::other(
                "Podman launcher identity changed",
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn container_workdir(
    workspace: &std::path::Path,
    requested: Option<&str>,
) -> Result<PathBuf, ProcessError> {
    let requested = std::path::Path::new(requested.unwrap_or_default());
    let relative = if requested.is_absolute() {
        requested.strip_prefix(workspace).map_err(|_| {
            ProcessError::spawn(std::io::Error::other(
                "container workdir must remain inside /workspace",
            ))
        })?
    } else {
        requested
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(ProcessError::spawn(std::io::Error::other(
            "container workdir must remain inside /workspace",
        )));
    }
    Ok(workspace.join(relative))
}

fn relative_workdir(
    workspace: &std::path::Path,
    requested: Option<&str>,
    error_message: &'static str,
) -> Result<PathBuf, ProcessError> {
    let requested = std::path::Path::new(requested.unwrap_or_default());
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(workspace)
            .map_err(|_| ProcessError::spawn(std::io::Error::other(error_message)))?
    } else {
        requested
    };
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(ProcessError::spawn(std::io::Error::other(error_message)));
    }
    Ok(relative.to_path_buf())
}

#[cfg(target_os = "linux")]
impl CommandLauncher for PodmanLauncher {
    fn build_command(
        &self,
        input: &ExecCommandInput,
    ) -> Result<std::process::Command, ProcessError> {
        self.reverify()?;
        let container_cwd =
            container_workdir(&self.config.container_workspace, input.workdir.as_deref())?;
        let shell = input.shell.as_deref().unwrap_or("/bin/bash");
        if !matches!(shell, "/bin/bash" | "/bin/sh" | "bash" | "sh") {
            return Err(ProcessError::UnsupportedShell {
                shell: shell.to_owned(),
            });
        }
        let job_id = format!("tools-mcp-job-{:032x}", rand::rng().random::<u128>());
        let mode = if input.login.unwrap_or(true) {
            "-lc"
        } else {
            "-c"
        };
        let mut command =
            std::process::Command::new(format!("/proc/self/fd/{}", self.executable.as_raw_fd()));
        command
            .arg("exec")
            .arg("--interactive")
            .args(input.tty.then_some("--tty"))
            .arg("--user")
            .arg("0:0")
            .arg("--workdir")
            .arg(container_cwd)
            .arg("--env")
            .arg(format!("HOME={}", self.config.container_home.display()))
            .arg("--env")
            .arg(format!(
                "CODEX_HOME={}/.codex",
                self.config.container_home.display()
            ))
            .arg(&self.config.container)
            .arg("/bin/sh")
            .arg("-c")
            .arg(include_str!("container_job_wrapper.sh"))
            .arg("tools-mcp-job-wrapper")
            .arg(job_id)
            .arg(shell)
            .arg(mode)
            .arg(&input.cmd)
            .current_dir(&self.config.host_workspace);
        Ok(command)
    }

    fn workspace_relative_workdir(
        &self,
        input: &ExecCommandInput,
    ) -> Result<PathBuf, ProcessError> {
        relative_workdir(
            &self.config.container_workspace,
            input.workdir.as_deref(),
            "container workdir must remain inside /workspace",
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
mod container_job_tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn container_workdir_accepts_public_workspace_paths_and_rejects_escape() {
        let workspace = Path::new("/workspace");
        assert_eq!(
            super::container_workdir(workspace, Some("/workspace/project")).unwrap(),
            Path::new("/workspace/project")
        );
        assert_eq!(
            super::container_workdir(workspace, Some("project")).unwrap(),
            Path::new("/workspace/project")
        );
        assert!(super::container_workdir(workspace, Some("/tmp")).is_err());
        assert!(super::container_workdir(workspace, Some("/workspace/../tmp")).is_err());
    }

    #[test]
    fn wrapper_reaps_background_processes_after_the_shell_exits() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("child.pid");
        let workload = format!("sleep 30 & echo $! > '{}'", pid_file.display());
        let status = Command::new("/bin/sh")
            .args([
                "-c",
                include_str!("container_job_wrapper.sh"),
                "tools-mcp-job-wrapper",
                "tools-mcp-job-0123456789abcdef",
                "/bin/sh",
                "-c",
                &workload,
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let pid = fs::read_to_string(pid_file).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let output = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", pid.trim()])
                .output()
                .unwrap();
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("background process {} survived wrapper cleanup", pid.trim());
    }
}
