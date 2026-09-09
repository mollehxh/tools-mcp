#![cfg(windows)]

use codex_tools_runtime::contracts::{ExecCommandInput, WriteStdinInput};
use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
use std::fs;
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    _root: tempfile::TempDir,
    manager: ProcessManager,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global-skills");
        let release = root.path().join("release");
        for path in [&workspace, &global, &release] {
            fs::create_dir_all(path).unwrap();
        }
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        expected_manifest()
            .unwrap()
            .write_release_relative(&release)
            .unwrap();
        let sandbox = Sandbox::load(authority, &release)
            .unwrap()
            .preflight()
            .unwrap()
            .0;
        Self {
            _root: root,
            manager: ProcessManager::new(Arc::new(sandbox)),
        }
    }
}

fn command(script: &str, tty: bool) -> ExecCommandInput {
    ExecCommandInput {
        cmd: script.to_owned(),
        workdir: None,
        tty,
        yield_time_ms: 250,
        max_output_tokens: None,
        shell: None,
        login: Some(false),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn powershell_is_the_default_and_explicit_shell_for_pipe_execution() {
    let fixture = Fixture::new();
    let owner = OwnerId::from("windows");
    let default = fixture
        .manager
        .exec_command(&owner, command("Write-Output -NoNewline default", false))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(default.output, "default");
    assert_eq!(default.exit_code, Some(0));

    let mut explicit = command("Write-Output -NoNewline explicit", false);
    explicit.shell = Some("powershell.exe".to_owned());
    let explicit = fixture
        .manager
        .exec_command(&owner, explicit)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(explicit.output, "explicit");
    fixture.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_pty_accepts_incremental_input() {
    let fixture = Fixture::new();
    let owner = OwnerId::from("windows-pty");
    let initial = fixture
        .manager
        .exec_command(
            &owner,
            command("$line=Read-Host; Write-Output \"got:$line\"", true),
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let session_id = initial
        .session_id
        .expect("PowerShell should await PTY input");
    let output = fixture
        .manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id,
                chars: "hello\r\n".to_owned(),
                yield_time_ms: 5_000,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(output.output.contains("got:hello"), "{}", output.output);
    assert_eq!(output.exit_code, Some(0));
    fixture.manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_and_shutdown_release_native_process_capacity() {
    let fixture = Fixture::new();
    let owner = OwnerId::from("windows-lifecycle");
    let initial = fixture
        .manager
        .exec_command(
            &owner,
            command("while ($true) { Start-Sleep -Milliseconds 100 }", false),
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let session_id = initial.session_id.expect("PowerShell should remain live");
    let interrupted = fixture
        .manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id,
                chars: "\u{3}".to_owned(),
                yield_time_ms: 5_000,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(interrupted.exit_code.is_some());

    let initial = fixture
        .manager
        .exec_command(
            &owner,
            command("while ($true) { Start-Sleep -Milliseconds 100 }", false),
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(initial.session_id.is_some());
    tokio::time::timeout(Duration::from_secs(5), fixture.manager.shutdown())
        .await
        .expect("shutdown must close the Windows job tree");
    assert_eq!(fixture.manager.stats().occupied, 0);
}
