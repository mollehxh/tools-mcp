#![cfg(target_os = "macos")]

use codex_tools_runtime::contracts::ExecCommandInput;
use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;

struct ReexecFixture {
    _root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    outside: std::path::PathBuf,
    manager: ProcessManager,
}

impl ReexecFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        let release = root.path().join("release");
        let system_skills = release.join("system-skills");
        let home = root.path().join("home");
        let tmp = root.path().join("tmp");
        for path in [&workspace, &outside, &system_skills, &home, &tmp] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir_all(workspace.join(".git")).unwrap();
        let environment = BTreeMap::<String, OsString>::from([
            ("HOME".to_owned(), home.clone().into_os_string()),
            ("CODEX_HOME".to_owned(), home.join("codex").into_os_string()),
            ("CARGO_HOME".to_owned(), home.join("cargo").into_os_string()),
            (
                "GRADLE_USER_HOME".to_owned(),
                home.join("gradle").into_os_string(),
            ),
            ("TMPDIR".to_owned(), tmp.clone().into_os_string()),
        ]);
        let capabilities = Arc::new(
            CapabilitySnapshot::resolve_configured(
                &workspace,
                &system_skills,
                |name| environment.get(name).cloned(),
                tmp.clone(),
                tmp,
            )
            .unwrap(),
        );
        let authority = WorkspaceAuthority::from_capabilities(capabilities).unwrap();
        expected_manifest()
            .unwrap()
            .write_release_relative(&release)
            .unwrap();
        let sandbox = Sandbox::load_with_reexec(
            authority,
            &release,
            std::path::Path::new(env!("CARGO_BIN_EXE_mcp-agent")),
        )
        .unwrap()
        .preflight()
        .unwrap()
        .0;
        Self {
            _root: root,
            workspace,
            outside,
            manager: ProcessManager::new(Arc::new(sandbox)),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_and_pty_launches_use_reexec_and_keep_the_workspace_boundary() {
    let ReexecFixture {
        _root,
        workspace,
        outside,
        manager,
    } = ReexecFixture::new();
    let owner = OwnerId::from("native-reexec");

    for tty in [false, true] {
        let metadata = manager
            .exec_command(
                &owner,
                command(
                    "mkdir -p .git/refs/heads && printf ok > .git/refs/heads/u2",
                    tty,
                ),
            )
            .await
            .unwrap()
            .handoff()
            .await
            .unwrap();
        assert_eq!(metadata.exit_code, Some(0), "{metadata:?}");
        assert!(workspace.join(".git/refs/heads/u2").is_file());

        let sentinel = outside.join(if tty { "pty-sentinel" } else { "pipe-sentinel" });
        fs::write(&sentinel, b"unchanged").unwrap();
        let denied = manager
            .exec_command(
                &owner,
                command(&format!("printf changed > {}", shell_quote(&sentinel)), tty),
            )
            .await
            .unwrap()
            .handoff()
            .await
            .unwrap();
        assert_ne!(denied.exit_code, Some(0), "{denied:?}");
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    }

    let inherited_sentinel = outside.join("inherited-fd-sentinel");
    let mut inherited = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&inherited_sentinel)
        .unwrap();
    inherited.write_all(b"unchanged").unwrap();
    inherited.seek(SeekFrom::Start(0)).unwrap();
    let inherited_fd = inherited.as_raw_fd();
    nix::fcntl::fcntl(
        inherited_fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();
    for tty in [false, true] {
        let output = manager
            .exec_command(
                &owner,
                command(&format!("printf changed >&{inherited_fd}"), tty),
            )
            .await
            .unwrap()
            .handoff()
            .await
            .unwrap();
        assert_ne!(output.exit_code, Some(0), "{output:?}");
        assert_eq!(fs::read(&inherited_sentinel).unwrap(), b"unchanged");
    }

    let nested_sentinel = outside.join("nested-sandbox-sentinel");
    fs::write(&nested_sentinel, b"unchanged").unwrap();
    let nested = format!(
        "env -u MCP_AGENT_INTERNAL_SANDBOX_ACTIVE MCP_AGENT_INTERNAL_SANDBOX_TOKEN=token {} --__mcp-agent-sandbox-child token -- /usr/bin/sandbox-exec -p '(version 1)(allow default)' /bin/sh -c 'printf changed > {}'",
        shell_quote(std::path::Path::new(env!("CARGO_BIN_EXE_mcp-agent"))),
        nested_sentinel.display()
    );
    let nested_output = manager
        .exec_command(&owner, command(&nested, false))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_ne!(nested_output.exit_code, Some(0), "{nested_output:?}");
    assert_eq!(fs::read(nested_sentinel).unwrap(), b"unchanged");
}

#[test]
fn hidden_adapter_rejects_direct_recursive_and_unsandboxed_selection() {
    let executable = env!("CARGO_BIN_EXE_mcp-agent");
    let direct = std::process::Command::new(executable)
        .arg("--__mcp-agent-sandbox-child")
        .output()
        .unwrap();
    assert!(!direct.status.success());
    assert!(String::from_utf8_lossy(&direct.stderr).contains("sandbox child adapter rejected"));

    let recursive = std::process::Command::new(executable)
        .args([
            "--__mcp-agent-sandbox-child",
            "token",
            "--",
            "/usr/bin/sandbox-exec",
            "-p",
            "(version 1)",
            "/bin/true",
        ])
        .env("MCP_AGENT_INTERNAL_SANDBOX_TOKEN", "token")
        .env("MCP_AGENT_INTERNAL_SANDBOX_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(!recursive.status.success());
    assert!(String::from_utf8_lossy(&recursive.stderr).contains("recursive"));

    let unsandboxed = std::process::Command::new(executable)
        .args([
            "--__mcp-agent-sandbox-child",
            "token",
            "--",
            "/bin/sh",
            "-p",
            "-c",
            "true",
        ])
        .env("MCP_AGENT_INTERNAL_SANDBOX_TOKEN", "token")
        .output()
        .unwrap();
    assert!(!unsandboxed.status.success());
    assert!(String::from_utf8_lossy(&unsandboxed.stderr).contains("unsandboxed"));
}

fn command(script: &str, tty: bool) -> ExecCommandInput {
    ExecCommandInput {
        cmd: script.to_owned(),
        workdir: None,
        tty,
        yield_time_ms: 1_000,
        max_output_tokens: None,
        shell: Some("/bin/sh".to_owned()),
        login: Some(false),
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
