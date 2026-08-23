use codex_tools_runtime::contracts::{ExecCommandInput, WriteStdinInput};
use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::sandbox::{Sandbox, VerifiedSandbox, expected_manifest};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;

struct RuntimeFixture {
    _root: TempDir,
    workspace: std::path::PathBuf,
    sandbox: Arc<VerifiedSandbox>,
}

impl RuntimeFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global-skills");
        let release = root.path().join("release");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&release).unwrap();
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        let manifest = expected_manifest().unwrap();
        manifest.write_release_relative(&release).unwrap();
        let sandbox = Sandbox::load(authority, &release)
            .unwrap()
            .preflight()
            .unwrap()
            .0;
        Self {
            _root: root,
            workspace,
            sandbox: Arc::new(sandbox),
        }
    }

    fn manager(&self) -> ProcessManager {
        ProcessManager::new(Arc::clone(&self.sandbox))
    }

    fn with_capabilities() -> (Self, Arc<CapabilitySnapshot>) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let release = root.path().join("release");
        let system_skills = release.join("system-skills");
        let home = root.path().join("home");
        let tmp = root.path().join("tmp");
        for path in [&workspace, &system_skills, &home, &tmp] {
            fs::create_dir_all(path).unwrap();
        }
        let environment = BTreeMap::<String, OsString>::from([
            ("HOME".to_owned(), home.clone().into_os_string()),
            (
                "CODEX_HOME".to_owned(),
                home.join("custom-codex").into_os_string(),
            ),
            (
                "CARGO_HOME".to_owned(),
                home.join("cargo-state").into_os_string(),
            ),
            (
                "GRADLE_USER_HOME".to_owned(),
                home.join("gradle-state").into_os_string(),
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
        let authority = WorkspaceAuthority::from_capabilities(Arc::clone(&capabilities)).unwrap();
        let manifest = expected_manifest().unwrap();
        manifest.write_release_relative(&release).unwrap();
        let sandbox = Sandbox::load(authority, &release)
            .unwrap()
            .preflight()
            .unwrap()
            .0;
        (
            Self {
                _root: root,
                workspace,
                sandbox: Arc::new(sandbox),
            },
            capabilities,
        )
    }
}

fn command(script: &str) -> ExecCommandInput {
    ExecCommandInput {
        cmd: script.to_owned(),
        workdir: None,
        tty: false,
        yield_time_ms: 250,
        max_output_tokens: None,
        shell: Some("/bin/sh".to_owned()),
        login: Some(false),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_capability_environment_reaches_login_nonlogin_pty_and_pipe_launches() {
    let (fixture, capabilities) = RuntimeFixture::with_capabilities();
    let manager = fixture.manager();
    let owner = OwnerId::from("environment");
    let expected = capabilities
        .environment()
        .iter()
        .map(|(name, value)| format!("{name}={}", value.to_string_lossy()))
        .collect::<Vec<_>>();
    let script = capabilities
        .environment()
        .keys()
        .map(|name| format!("printf '%s=%s\\n' '{name}' \"${name}\""))
        .collect::<Vec<_>>()
        .join("; ");

    for (shell, tty, login) in [
        ("/bin/sh", false, Some(false)),
        ("/bin/bash", true, Some(false)),
        ("/bin/zsh", false, Some(false)),
        ("/bin/sh", true, None),
    ] {
        let mut input = command(&script);
        input.shell = Some(shell.to_owned());
        input.tty = tty;
        input.login = login;
        let output = manager
            .exec_command(&owner, input)
            .await
            .unwrap()
            .handoff()
            .await
            .unwrap();
        for line in &expected {
            assert!(
                output.output.contains(line),
                "{shell} tty={tty}: {output:?}"
            );
        }
        assert_eq!(output.exit_code, Some(0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_shell_is_rejected_without_reserving_a_session() {
    let (fixture, _) = RuntimeFixture::with_capabilities();
    let manager = fixture.manager();
    let mut input = command("printf must-not-run");
    input.shell = Some("/bin/tcsh".to_owned());
    let error = manager
        .exec_command(&OwnerId::from("environment"), input)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsupported workload shell"));
    assert_eq!(manager.stats().occupied, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_environment_mutation_does_not_change_later_launches() {
    let (fixture, capabilities) = RuntimeFixture::with_capabilities();
    let manager = fixture.manager();
    let owner = OwnerId::from("environment");
    let fixed_tmpdir = capabilities.tmpdir().to_string_lossy();

    let first = manager
        .exec_command(
            &owner,
            command("export TMPDIR=/outside; printf '%s' \"$TMPDIR\""),
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(first.output, "/outside");

    let second = manager
        .exec_command(&owner, command("printf '%s' \"$TMPDIR\""))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(second.output, fixed_tmpdir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stdin_continues_the_fixed_environment_process_tree() {
    let (fixture, capabilities) = RuntimeFixture::with_capabilities();
    let manager = fixture.manager();
    let owner = OwnerId::from("environment");
    let mut input = command("read line; printf '%s:%s' \"$TMPDIR\" \"$line\"");
    input.tty = true;
    let initial = manager
        .exec_command(&owner, input)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let session_id = initial.session_id.unwrap();
    let output = manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id,
                chars: "continued\n".to_owned(),
                yield_time_ms: 1_000,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(
        output.output.contains(&format!(
            "{}:continued",
            capabilities.tmpdir().to_string_lossy()
        )),
        "{output:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_and_nonzero_commands_preserve_compatible_results() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");

    let output = manager
        .exec_command(&owner, command("printf hello"))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(output.output, "hello");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.session_id, None);
    assert!(output.chunk_id.is_some());

    let output = manager
        .exec_command(&owner, command("printf failure; exit 17"))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(output.output, "failure");
    assert_eq!(output.exit_code, Some(17));
    assert_eq!(output.session_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_refreshes_a_process_that_exits_after_collection() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");
    let pending = manager
        .exec_command(&owner, command("printf final; sleep 0.1"))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let output = pending.handoff().await.unwrap();
    assert_eq!(output.output, "final");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.session_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_process_is_reached_by_a_fresh_owner_scoped_call() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");

    let initial = manager
        .exec_command(&owner, command("printf first; sleep 1; printf second"))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let session_id = initial.session_id.expect("command should yield");
    assert!(initial.output.contains("first"));

    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let final_output = manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id,
                chars: String::new(),
                yield_time_ms: 5_000,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(final_output.output.contains("second"));
    assert_eq!(final_output.exit_code, Some(0));
    assert_eq!(final_output.session_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_accepts_input_while_pipe_rejects_non_interrupt_input() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");

    let mut pty_command = command("read line; printf 'got:%s\\n' \"$line\"");
    pty_command.tty = true;
    let initial = manager
        .exec_command(&owner, pty_command)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let pty_id = initial.session_id.expect("PTY should remain active");
    let output = manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id: pty_id,
                chars: "hello\n".to_owned(),
                yield_time_ms: 1_000,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(output.output.contains("got:hello"), "{}", output.output);

    let initial = manager
        .exec_command(&owner, command("sleep 2"))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let pipe_id = initial.session_id.expect("pipe should remain active");
    let error = manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id: pipe_id,
                chars: "not allowed".to_owned(),
                yield_time_ms: 250,
                max_output_tokens: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stdin is closed"));

    let initial = manager
        .exec_command(&owner, command("sleep 30"))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    let pipe_id = initial.session_id.expect("pipe should remain active");
    let output = manager
        .write_stdin(
            &owner,
            WriteStdinInput {
                session_id: pipe_id,
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
    assert_eq!(output.exit_code, Some(130));

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_bounded_then_model_budget_is_reported() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");
    let mut input = command("yes 0123456789 | head -c 1100000");
    input.yield_time_ms = 30_000;
    input.max_output_tokens = Some(40);

    let output = manager
        .exec_command(&owner, input)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert!(output.original_token_count.unwrap() > 40);
    assert!(output.output.len() < 2_000);
    assert!(output.output.contains("truncated output"));
    assert!(output.output.contains("bytes omitted"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_max_output_tokens_uses_the_pinned_default_budget() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");
    let mut input = command("yes 0123456789 | head -c 100000");
    input.yield_time_ms = 30_000;

    let output = manager
        .exec_command(&owner, input)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert!(output.original_token_count.unwrap() > 10_000);
    assert!(output.output.len() < 50_000, "{}", output.output.len());
    assert!(output.output.contains("truncated output"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workdir_is_resolved_inside_the_immutable_workspace() {
    let fixture = RuntimeFixture::new();
    fs::create_dir(fixture.workspace.join("nested")).unwrap();
    let manager = fixture.manager();
    let owner = OwnerId::from("alice");
    let mut input = command("pwd");
    input.workdir = Some("nested".to_owned());

    let output = manager
        .exec_command(&owner, input)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert_eq!(
        std::path::Path::new(output.output.trim()),
        fixture.workspace.join("nested").canonicalize().unwrap()
    );
}
