use super::{Sandbox, SandboxError, digest};
use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PreflightReceipt {
    pub outside_read_allowed: bool,
    pub local_service_allowed: bool,
    pub listener_bind_allowed: bool,
    pub workspace_write_allowed: bool,
    pub writable_roots_checked: usize,
    pub descendant_write_allowed: bool,
    pub outside_write_denied: bool,
    pub release_canary_verified: bool,
}

pub(super) fn run(sandbox: &Sandbox) -> Result<PreflightReceipt, SandboxError> {
    let roots = sandbox.authority.capabilities().map_or_else(
        || vec![sandbox.authority.workspace_root().to_path_buf()],
        |capabilities| capabilities.writable_roots().to_vec(),
    );
    if roots.is_empty() {
        return Err(SandboxError::Preflight(
            "sandbox has no writable capability roots".to_owned(),
        ));
    }

    let mut root_writes_allowed = true;
    let mut descendant_write_allowed = true;
    for root in &roots {
        let probe = OwnedWorkspaceProbe::reserve(root)?;
        let direct = platform_write(sandbox, probe.probe_path());
        let descendant = platform_descendant_write(sandbox, probe.descendant_probe_path());
        let cleanup = probe.cleanup();
        root_writes_allowed &= direct?;
        descendant_write_allowed &= descendant?;
        cleanup?;
    }

    let canary = sandbox.release.join(&sandbox.manifest.canary_path);
    if roots.iter().any(|root| canary.starts_with(root)) {
        return Err(SandboxError::Preflight(
            "manifest-verified release canary overlaps a writable root".to_owned(),
        ));
    }
    sandbox.reverify()?;
    let original = fs::read(&canary)?;
    if digest(&original) != sandbox.manifest.canary_sha256 {
        return Err(SandboxError::ArtifactReplaced);
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&canary)
        .map_err(|error| {
            SandboxError::Preflight(format!(
                "server account cannot open release canary for write: {error}"
            ))
        })?;
    let read = platform_read(sandbox, &canary)?;
    let canary_write_succeeded = platform_open_for_write(sandbox, &canary)?;
    let canary_unchanged = fs::read(&canary)? == original;
    sandbox.reverify()?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let connect_succeeded = platform_connect(sandbox, port)?;
    let local_service_allowed =
        connect_succeeded && accept_loopback_connection(&listener, Duration::from_secs(2))?;
    let listener_bind_allowed = platform_listener_bind(sandbox, "0.0.0.0")?;
    let receipt = PreflightReceipt {
        outside_read_allowed: read.status.success() && read.stdout == original,
        local_service_allowed,
        listener_bind_allowed,
        workspace_write_allowed: root_writes_allowed,
        writable_roots_checked: roots.len(),
        descendant_write_allowed,
        outside_write_denied: !canary_write_succeeded && canary_unchanged,
        release_canary_verified: canary_unchanged,
    };
    if receipt.outside_read_allowed
        && receipt.local_service_allowed
        && receipt.listener_bind_allowed
        && receipt.workspace_write_allowed
        && receipt.writable_roots_checked == roots.len()
        && receipt.descendant_write_allowed
        && receipt.outside_write_denied
        && receipt.release_canary_verified
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
    descendant_directory: PathBuf,
    grandchild_directory: PathBuf,
    descendant_probe: PathBuf,
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
                descendant_directory: directory.join("child"),
                grandchild_directory: directory.join("child/grandchild"),
                descendant_probe: directory.join("child/grandchild/descendant-write"),
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

    fn descendant_probe_path(&self) -> &Path {
        &self.descendant_probe
    }

    fn cleanup(self) -> Result<(), SandboxError> {
        if fs::read(&self.marker).ok().as_deref() != Some(self.token.as_slice()) {
            return Ok(());
        }
        if fs::read(&self.probe).ok().as_deref() == Some(b"probe") {
            fs::remove_file(&self.probe)?;
        }
        if fs::read(&self.descendant_probe).ok().as_deref() == Some(b"probe") {
            fs::remove_file(&self.descendant_probe)?;
            fs::remove_dir(&self.grandchild_directory)?;
            fs::remove_dir(&self.descendant_directory)?;
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

#[cfg(unix)]
fn platform_descendant_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    let parent = path
        .parent()
        .ok_or_else(|| SandboxError::Preflight("descendant probe has no parent".to_owned()))?;
    let inner = format!(
        "mkdir -p {} && printf probe > {}",
        shell_quote(parent),
        shell_quote(path)
    );
    let script = format!("/bin/sh -c {}", shell_quote_text(&inner));
    Ok(sandbox
        .command_unverified("/bin/sh", &["-c", &script], Path::new("."))?
        .status()?
        .success())
}

#[cfg(windows)]
fn platform_descendant_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    platform_write(sandbox, path)
}

#[cfg(unix)]
fn platform_open_for_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    let script = format!(": >> {}", shell_quote(path));
    quiet_status(sandbox, "/bin/sh", &["-c", &script])
}

#[cfg(windows)]
fn platform_open_for_write(sandbox: &Sandbox, path: &Path) -> Result<bool, SandboxError> {
    let script = format!(
        "$f=[System.IO.File]::Open('{}','Open','Write');$f.Close()",
        path.display()
    );
    Ok(sandbox
        .command_unverified(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            Path::new("."),
        )?
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
            return quiet_status(sandbox, executable, &["-z", "127.0.0.1", &port]);
        }
    }
    Err(SandboxError::Preflight(
        "no fixed local-service probe executable is installed".to_owned(),
    ))
}

#[cfg(unix)]
fn platform_listener_bind(sandbox: &Sandbox, address: &str) -> Result<bool, SandboxError> {
    let reservation = TcpListener::bind(("127.0.0.1", 0))?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    let port_text = port.to_string();
    let mut child = None;
    for executable in ["/usr/bin/nc", "/bin/nc"] {
        if Path::new(executable).is_file() {
            let mut command = sandbox
                .command_unverified(executable, &["-l", address, &port_text], Path::new("."))?
                .into_std_command()?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            child = Some(command.spawn()?);
            break;
        }
    }
    let mut child = child.ok_or_else(|| {
        SandboxError::Preflight("no fixed listener probe executable is installed".to_owned())
    })?;
    let connected = connect_to_child_listener(&mut child, port, Duration::from_secs(2))?;
    terminate_probe_child(&mut child);
    Ok(connected)
}

#[cfg(windows)]
fn platform_listener_bind(sandbox: &Sandbox, _address: &str) -> Result<bool, SandboxError> {
    let script = "$l=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Any,0);$l.Start();$l.Stop()";
    Ok(sandbox
        .command_unverified(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", script],
            Path::new("."),
        )?
        .status()?
        .success())
}

fn connect_to_child_listener(
    child: &mut Child,
    port: u16,
    timeout: Duration,
) -> Result<bool, SandboxError> {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(true);
        }
        if child.try_wait()?.is_some() || Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate_probe_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn quiet_status(sandbox: &Sandbox, program: &str, args: &[&str]) -> Result<bool, SandboxError> {
    let mut command = sandbox
        .command_unverified(program, args, Path::new("."))?
        .into_std_command()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command.status()?.success())
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
    shell_quote_text(&path.to_string_lossy())
}

#[cfg(unix)]
fn shell_quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
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
