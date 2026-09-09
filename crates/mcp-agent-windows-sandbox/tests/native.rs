#![cfg(windows)]

use rcgen::CertificateSigningRequestParams;
use std::fs;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_mcp-agent-windows-sandbox")
}

fn sandbox_command(workspace: &Path) -> Command {
    let mut command = Command::new(helper());
    command
        .args(["--protocol", "mcp-agent-workspace-write/v1", "--workspace"])
        .arg(workspace)
        .arg("--cwd")
        .arg(workspace)
        .arg("--");
    command
}

fn cng(command: &str, key_name: &str) -> Output {
    Command::new(helper())
        .args([command, key_name])
        .output()
        .unwrap()
}

struct TestKey(String);

impl TestKey {
    fn unique() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(format!(
            "tools-mcp-device:test-{}-{nonce}",
            std::process::id()
        ))
    }
}

impl Drop for TestKey {
    fn drop(&mut self) {
        let _ = cng("delete-test-key", &self.0);
    }
}

fn wait_for_file(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        assert!(child.try_wait().unwrap().is_none(), "sandbox exited early");
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {}", path.display());
}

#[test]
fn restricted_token_writes_only_below_declared_root() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside_file = workspace.path().join("inside.txt");
    let outside_file = outside.path().join("outside.txt");
    let script = format!(
        "Set-Content -LiteralPath '{}' -Value inside; try {{ Set-Content -LiteralPath '{}' -Value outside -ErrorAction Stop; exit 9 }} catch {{ exit 0 }}",
        inside_file.display(),
        outside_file.display()
    );
    let status = Command::new(helper())
        .args(["--protocol", "mcp-agent-workspace-write/v1", "--workspace"])
        .arg(workspace.path())
        .arg("--cwd")
        .arg(workspace.path())
        .args([
            "--",
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(inside_file.is_file());
    assert!(!outside_file.exists());
}

#[test]
fn child_observes_restricted_medium_integrity_token() {
    let workspace = tempfile::tempdir().unwrap();
    let output = sandbox_command(workspace.path())
        .arg(helper())
        .arg("inspect-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("restricted=true"), "{stdout}");
    let integrity = stdout
        .split("integrity_rid=")
        .nth(1)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(integrity <= 0x2000, "unexpected integrity RID {integrity}");
}

#[test]
fn cng_key_is_non_exportable_and_csr_signature_is_valid() {
    let key = TestKey::unique();
    let generated = cng("generate", &key.0);
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let csr = String::from_utf8(generated.stdout).unwrap();
    let parsed = CertificateSigningRequestParams::from_pem(&csr).unwrap();
    assert_eq!(
        parsed.public_key.algorithm(),
        &rcgen::PKCS_ECDSA_P256_SHA256
    );

    let verified = cng("verify-non-exportable", &key.0);
    assert!(verified.status.success());
    let public_key = cng("public-key", &key.0);
    assert!(public_key.status.success());
    assert!(public_key.stdout.len() > 65);

    let duplicate = cng("generate", &key.0);
    assert!(
        !duplicate.status.success(),
        "duplicate key silently replaced"
    );
}

#[test]
fn restricted_workload_cannot_use_device_cng_key() {
    let workspace = tempfile::tempdir().unwrap();
    let key = TestKey::unique();
    assert!(cng("generate", &key.0).status.success());

    let output = sandbox_command(workspace.path())
        .arg(helper())
        .args(["sign", &key.0])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "restricted token used the relay key"
    );
}

#[test]
fn only_standard_handles_are_inherited() {
    let workspace = tempfile::tempdir().unwrap();
    let unrelated = fs::File::create(workspace.path().join("unrelated-handle")).unwrap();
    let raw = unrelated.as_raw_handle();
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
        0
    );
    let output = sandbox_command(workspace.path())
        .arg(helper())
        .args(["inspect-handle", &(raw as usize).to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "inherited=false"
    );
}

#[test]
fn hardlink_and_rename_cannot_move_writes_outside_the_declared_root() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel.txt");
    fs::write(&sentinel, "original").unwrap();
    let hardlink = workspace.path().join("hardlink.txt");
    let moved = workspace.path().join("moved.txt");
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; New-Item -ItemType HardLink -Path '{}' -Target '{}' | Out-Null; Set-Content -LiteralPath '{}' -Value changed; Move-Item -LiteralPath '{}' -Destination '{}' -Force",
        hardlink.display(),
        sentinel.display(),
        hardlink.display(),
        sentinel.display(),
        moved.display()
    );
    let _ = sandbox_command(workspace.path())
        .args([
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .status()
        .unwrap();
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "original");
    assert!(!moved.exists());
}

#[test]
fn restricted_workload_cannot_write_the_user_registry() {
    let workspace = tempfile::tempdir().unwrap();
    let registry_name = format!("ToolsMcpSandboxTest{}", std::process::id());
    let script = format!(
        "$ErrorActionPreference='Stop'; try {{ New-Item -Path 'HKCU:\\Software\\{registry_name}' -Force | Out-Null; exit 9 }} catch {{ exit 0 }}"
    );
    let status = sandbox_command(workspace.path())
        .args([
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .status()
        .unwrap();
    assert!(status.success(), "restricted workload wrote HKCU");
    let cleanup = format!(
        "Remove-Item -LiteralPath 'HKCU:\\Software\\{registry_name}' -Recurse -Force -ErrorAction SilentlyContinue"
    );
    let _ = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cleanup])
        .status();
}

#[test]
fn closing_launcher_kills_descendant_process_tree() {
    let workspace = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("descendant.pid");
    let script = format!(
        "$p = Start-Process powershell.exe -PassThru -ArgumentList '-NoProfile','-Command','Start-Sleep 120'; Set-Content -LiteralPath '{}' -Value $p.Id; Wait-Process -Id $p.Id",
        pid_file.display()
    );
    let mut launcher = sandbox_command(workspace.path())
        .args([
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .spawn()
        .unwrap();
    wait_for_file(&pid_file, &mut launcher);
    let descendant_pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();

    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let alive = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "if (Get-Process -Id {descendant_pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
                ),
            ])
            .status()
            .unwrap()
            .success();
        if !alive {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("descendant process {descendant_pid} survived launcher termination");
}

#[test]
fn reparse_workspace_is_rejected_before_child_launch() {
    let target = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let junction = parent.path().join("workspace junction");
    let command = format!(
        "mklink /J \"{}\" \"{}\"",
        junction.display(),
        target.path().display()
    );
    assert!(
        Command::new("cmd.exe")
            .args(["/d", "/c", &command])
            .status()
            .unwrap()
            .success()
    );
    let status = Command::new(helper())
        .args(["--protocol", "mcp-agent-workspace-write/v1", "--workspace"])
        .arg(&junction)
        .arg("--cwd")
        .arg(&junction)
        .args(["--", "cmd.exe", "/d", "/c", "exit 0"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(125));
    fs::remove_dir(&junction).unwrap();
}
