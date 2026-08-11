use super::{Sandbox, SandboxError};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PreflightReceipt {
    pub outside_read_allowed: bool,
    pub local_service_allowed: bool,
    pub workspace_write_allowed: bool,
    pub outside_write_denied: bool,
}

pub(super) fn run(
    sandbox: &Sandbox,
    outside_sentinel: &Path,
) -> Result<PreflightReceipt, SandboxError> {
    let original = fs::read(outside_sentinel)?;
    let read = platform_read(sandbox, outside_sentinel)?;
    let workspace_probe = OwnedWorkspaceProbe::reserve(sandbox.authority.workspace_root())?;
    let workspace_write_result = platform_write(sandbox, workspace_probe.probe_path());
    let cleanup_result = workspace_probe.cleanup();
    let workspace_write_allowed = workspace_write_result?;
    cleanup_result?;
    let outside_write_succeeded = platform_write(sandbox, outside_sentinel)?;
    let outside_unchanged = fs::read(outside_sentinel)? == original;
    if !outside_unchanged {
        fs::write(outside_sentinel, &original)?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let connect_succeeded = platform_connect(sandbox, port)?;
    let local_service_allowed =
        connect_succeeded && accept_loopback_connection(&listener, Duration::from_secs(2))?;
    let receipt = PreflightReceipt {
        outside_read_allowed: read.status.success() && read.stdout == original,
        local_service_allowed,
        workspace_write_allowed,
        outside_write_denied: !outside_write_succeeded && outside_unchanged,
    };
    if receipt.outside_read_allowed
        && receipt.local_service_allowed
        && receipt.workspace_write_allowed
        && receipt.outside_write_denied
    {
        Ok(receipt)
    } else {
        Err(SandboxError::Preflight(format!("receipt={receipt:?}")))
    }
}

fn accept_loopback_connection(
    listener: &TcpListener,
    timeout: Duration,
) -> Result<bool, SandboxError> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                return Ok(peer.ip().is_loopback() && stream.local_addr()?.ip().is_loopback());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(SandboxError::Io(error)),
        }
    }
}

/// A one-shot workspace write target that is atomically reserved by this
/// preflight. The child writes only below this directory, never to a fixed
/// project path. Cleanup requires the random owner marker and never recurses.
struct OwnedWorkspaceProbe {
    directory: PathBuf,
    marker: PathBuf,
    probe: PathBuf,
    token: Vec<u8>,
}

impl OwnedWorkspaceProbe {
    fn reserve(workspace: &Path) -> Result<Self, SandboxError> {
        for _ in 0..128 {
            let token = unique_token();
            let directory = workspace.join(format!(".mcp-agent-preflight-{token}"));
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(SandboxError::Io(error)),
            }

            let marker = directory.join("owner");
            let marker_result = (|| -> Result<(), std::io::Error> {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)?;
                file.write_all(token.as_bytes())?;
                file.sync_all()
            })();
            if let Err(error) = marker_result {
                let _ = fs::remove_dir(&directory);
                return Err(SandboxError::Io(error));
            }
            return Ok(Self {
                probe: directory.join("workspace-write"),
                directory,
                marker,
                token: token.into_bytes(),
            });
        }
        Err(SandboxError::Preflight(
            "could not reserve a unique workspace preflight directory".to_owned(),
        ))
    }

    fn probe_path(&self) -> &Path {
        &self.probe
    }

    fn cleanup(self) -> Result<(), SandboxError> {
        if fs::read(&self.marker).ok().as_deref() != Some(self.token.as_slice()) {
            return Ok(());
        }
        if fs::read(&self.probe).ok().as_deref() == Some(b"probe") {
            fs::remove_file(&self.probe)?;
        }
        fs::remove_file(&self.marker)?;
        match fs::remove_dir(&self.directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SandboxError::Io(error)),
        }
    }
}

fn unique_token() -> String {
    let sequence = PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{:x}-{:x}-{:x}", std::process::id(), sequence, timestamp)
}

#[cfg(unix)]
fn platform_read(sandbox: &Sandbox, path: &Path) -> Result<std::process::Output, SandboxError> {
    sandbox
        .command_unverified("/bin/cat", &[&path.to_string_lossy()], Path::new("."))?
        .output()
}

#[cfg(windows)]
fn platform_read(sandbox: &Sandbox, path: &Path) -> Result<std::process::Output, SandboxError> {
    Ok(sandbox
        .command_unverified(
            "cmd.exe",
            &["/d", "/c", "type", &path.to_string_lossy()],
            Path::new("."),
        )?
        .output()?)
}

#[cfg(unix)]
fn platform_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    let script = format!("printf probe > {}", shell_quote(path));
    Ok(sandbox
        .command_unverified("/bin/sh", &["-c", &script], Path::new("."))?
        .status()?
        .success())
}

#[cfg(windows)]
fn platform_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    let script = format!(">\"{}\" echo probe", path.display());
    Ok(sandbox
        .command_unverified("cmd.exe", &["/d", "/c", &script], Path::new("."))?
        .status()?
        .success())
}

#[cfg(unix)]
fn platform_connect(sandbox: &Sandbox, port: u16) -> Result<bool, SandboxError> {
    let port = port.to_string();
    for executable in ["/usr/bin/nc", "/bin/nc"] {
        if Path::new(executable).is_file() {
            return Ok(sandbox
                .command_unverified(executable, &["-z", "127.0.0.1", &port], Path::new("."))?
                .status()?
                .success());
        }
    }
    Err(SandboxError::Preflight(
        "no fixed local-service probe executable is installed".to_owned(),
    ))
}

#[cfg(windows)]
fn platform_connect(sandbox: &Sandbox, port: u16) -> Result<bool, SandboxError> {
    let script = format!("$c=New-Object Net.Sockets.TcpClient('127.0.0.1',{port});$c.Close()");
    Ok(sandbox
        .command_unverified(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            Path::new("."),
        )?
        .status()?
        .success())
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{OwnedWorkspaceProbe, accept_loopback_connection};
    use std::fs;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    #[test]
    fn reservation_does_not_touch_a_fixed_project_probe_path() {
        let workspace = tempfile::tempdir().unwrap();
        let fixed_path = workspace.path().join(".mcp-agent-preflight");
        fs::write(&fixed_path, b"project data").unwrap();

        let probe = OwnedWorkspaceProbe::reserve(workspace.path()).unwrap();
        assert_ne!(probe.probe_path(), fixed_path);
        probe.cleanup().unwrap();

        assert_eq!(fs::read(fixed_path).unwrap(), b"project data");
    }

    #[test]
    fn cleanup_leaves_a_reservation_with_a_replaced_owner_marker() {
        let workspace = tempfile::tempdir().unwrap();
        let probe = OwnedWorkspaceProbe::reserve(workspace.path()).unwrap();
        let directory = probe.directory.clone();
        fs::write(&probe.marker, b"different owner").unwrap();

        probe.cleanup().unwrap();

        assert!(directory.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn loopback_accept_is_bounded_when_child_never_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let started = Instant::now();

        assert!(!accept_loopback_connection(&listener, Duration::from_millis(20)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
