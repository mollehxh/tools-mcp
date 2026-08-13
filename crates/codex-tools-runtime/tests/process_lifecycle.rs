use codex_tools_runtime::contracts::{ExecCommandInput, WriteStdinInput};
use codex_tools_runtime::process::{OwnerId, ProcessError, ProcessManager, ProcessManagerConfig};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_authority::sandbox::{Sandbox, VerifiedSandbox, expected_manifest};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
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
        let outside = root.path().join("outside");
        let release = root.path().join("release");
        for path in [&workspace, &global, &outside, &release] {
            fs::create_dir_all(path).unwrap();
        }
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        let manifest = expected_manifest().unwrap();
        manifest.write_release_relative(&release).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"host-readable").unwrap();
        let sandbox = Sandbox::load(authority, &release)
            .unwrap()
            .preflight(&sentinel)
            .unwrap()
            .0;
        Self {
            _root: root,
            workspace,
            sandbox: Arc::new(sandbox),
        }
    }

    fn manager(&self, capacity: usize, retention: Duration) -> ProcessManager {
        ProcessManager::with_config(
            Arc::clone(&self.sandbox),
            ProcessManagerConfig {
                capacity,
                terminal_retention: retention,
                ..ProcessManagerConfig::default()
            },
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_rolls_back_and_shutdown_wins_publication_race() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let mut missing_shell = long_command("true");
    missing_shell.workdir = Some("../outside".to_owned());
    assert!(matches!(
        manager.exec_command(&owner, missing_shell).await,
        Err(ProcessError::Spawn(_))
    ));
    assert_eq!(manager.stats().occupied, 0);

    let pending = manager
        .exec_command(&owner, long_command("sleep 30"))
        .await
        .unwrap();
    manager.shutdown().await;
    assert!(matches!(
        pending.handoff().await,
        Err(ProcessError::ShuttingDown)
    ));
    assert_eq!(manager.stats().occupied, 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_terminates_descendants_in_the_child_process_group() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let pending = manager
        .exec_command(
            &owner,
            long_command("sleep 30 & child=$!; printf '%s' \"$child\" > child.pid; wait"),
        )
        .await
        .unwrap();
    let child_pid: u32 = fs::read_to_string(fixture.workspace.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();

    pending.cancel().await;
    let exited = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", &child_pid.to_string()])
                .status()
                .is_ok_and(|status| status.success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        exited.is_ok(),
        "descendant {child_pid} survived cancellation"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_descendant_after_direct_child_exits() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let pending = manager
        .exec_command(
            &owner,
            long_command("sleep 30 & child=$!; printf '%s' \"$child\" > child.pid"),
        )
        .await
        .unwrap();
    let child_pid: u32 = fs::read_to_string(fixture.workspace.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(pending.handoff().await.unwrap().session_id.is_some());

    tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .expect("shutdown must retain a kill path after the shell exits");
    let exited = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", &child_pid.to_string()])
                .status()
                .is_ok_and(|status| status.success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(exited.is_ok(), "descendant {child_pid} survived shutdown");
}

fn long_command(script: &str) -> ExecCommandInput {
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

async fn start(manager: &ProcessManager, owner: &OwnerId, script: &str) -> i32 {
    manager
        .exec_command(owner, long_command(script))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap()
        .session_id
        .expect("process should still be live")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_lookup_is_fail_closed_and_reconnect_keeps_session() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(64, Duration::from_mins(5));
    let alice = OwnerId::from("alice");
    let bob = OwnerId::from("bob");
    let session_id = start(&manager, &alice, "sleep 1; printf done").await;

    let foreign = manager
        .write_stdin(&bob, WriteStdinInput::poll(session_id))
        .await
        .unwrap_err();
    assert!(matches!(foreign, ProcessError::UnknownSession { .. }));

    let output = manager
        .write_stdin(&alice, WriteStdinInput::poll(session_id))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(output.output.contains("done"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_publication_terminates_and_releases_capacity() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let pending = manager
        .exec_command(&owner, long_command("sleep 30"))
        .await
        .unwrap();
    assert_eq!(manager.stats().occupied, 1);

    pending.cancel().await;
    manager.wait_for_idle().await;
    assert_eq!(manager.stats().occupied, 0);
    let replacement = start(&manager, &owner, "sleep 30").await;
    assert!(replacement > 0);
    manager.shutdown().await;
}

#[test]
fn dropping_initial_result_outside_a_runtime_releases_its_reservation() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pending = runtime
        .block_on(manager.exec_command(&owner, long_command("sleep 30")))
        .unwrap();

    drop(pending);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), manager.wait_for_idle())
            .await
            .expect("off-runtime drop must release the unpublished slot");
    });
    assert_eq!(manager.stats().occupied, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_session_survives_dropped_response_and_capacity_never_evicts() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let session_id = start(&manager, &owner, "sleep 30").await;

    let error = manager
        .exec_command(&owner, long_command("sleep 30"))
        .await
        .unwrap_err();
    assert!(matches!(error, ProcessError::Capacity { limit: 1 }));
    assert_eq!(manager.stats().live, 1);

    manager.interrupt(&owner, session_id).await.unwrap();
    let final_output = manager
        .write_stdin(&owner, WriteStdinInput::poll(session_id))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(final_output.exit_code.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_wait_for_a_blocked_pty_write() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(1, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let mut command = long_command("stty -echo; printf ready; sleep 30");
    command.tty = true;
    let initial = manager
        .exec_command(&owner, command)
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(initial.output.contains("ready"));
    let session_id = initial.session_id.unwrap();

    let write_manager = manager.clone();
    let write_owner = owner.clone();
    let write = tokio::spawn(async move {
        write_manager
            .write_stdin(
                &write_owner,
                WriteStdinInput {
                    session_id,
                    chars: "x".repeat(8 * 1024 * 1024),
                    yield_time_ms: 250,
                    max_output_tokens: None,
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .expect("shutdown must not wait behind a PTY write");
    let _ = tokio::time::timeout(Duration::from_secs(2), write)
        .await
        .expect("PTY writer must unblock after shutdown")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_capacity_never_exceeds_the_hard_process_limit() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(usize::MAX, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let mut joins = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let manager = manager.clone();
        let owner = owner.clone();
        joins.spawn(async move { manager.exec_command(&owner, long_command("sleep 30")).await });
    }

    let mut pending = Vec::new();
    while let Some(result) = joins.join_next().await {
        pending.push(result.unwrap().unwrap());
    }
    assert!(matches!(
        manager.exec_command(&owner, long_command("sleep 30")).await,
        Err(ProcessError::Capacity { limit: 64 })
    ));
    for result in pending {
        result.cancel().await;
    }
    manager.wait_for_idle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_serializes_calls_and_terminal_is_consumed_once() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(64, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let session_id = start(&manager, &owner, "sleep 1; printf final").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    let first = {
        let manager = manager.clone();
        let owner = owner.clone();
        tokio::spawn(async move {
            let pending = manager
                .write_stdin(&owner, WriteStdinInput::poll(session_id))
                .await?;
            pending.handoff().await
        })
    };
    let second = {
        let manager = manager.clone();
        let owner = owner.clone();
        tokio::spawn(async move {
            let pending = manager
                .write_stdin(&owner, WriteStdinInput::poll(session_id))
                .await?;
            pending.handoff().await
        })
    };
    let (first, second) = tokio::join!(first, second);
    let results = [first.unwrap(), second.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ProcessError::UnknownSession { .. })))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_poll_restores_drained_output_before_handoff() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(64, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let session_id = start(&manager, &owner, "sleep 1; printf recoverable; sleep 2").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    let pending = manager
        .write_stdin(&owner, WriteStdinInput::poll(session_id))
        .await
        .unwrap();
    assert!(pending.output().output.contains("recoverable"));
    pending.cancel().await;

    let replay = manager
        .write_stdin(&owner, WriteStdinInput::poll(session_id))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(replay.output.contains("recoverable"));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_collection_restores_output_for_the_next_poll() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(64, Duration::from_mins(5));
    let owner = OwnerId::from("alice");
    let session_id = start(&manager, &owner, "sleep 0.4; printf recoverable; sleep 2").await;

    let timed_out = tokio::time::timeout(
        Duration::from_millis(300),
        manager.write_stdin(
            &owner,
            WriteStdinInput {
                session_id,
                chars: String::new(),
                yield_time_ms: 1_000,
                max_output_tokens: None,
            },
        ),
    )
    .await;
    assert!(timed_out.is_err(), "the collection should be cancelled");

    let replay = manager
        .write_stdin(&owner, WriteStdinInput::poll(session_id))
        .await
        .unwrap()
        .handoff()
        .await
        .unwrap();
    assert!(replay.output.contains("recoverable"), "{replay:?}");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstone_expires_and_shutdown_terminates_every_session() {
    let fixture = RuntimeFixture::new();
    let manager = fixture.manager(64, Duration::from_millis(50));
    let alice = OwnerId::from("alice");
    let bob = OwnerId::from("bob");
    let alice_id = start(&manager, &alice, "sleep 30").await;
    let _bob_id = start(&manager, &bob, "sleep 30").await;
    assert_eq!(manager.stats().live, 2);

    manager.interrupt(&alice, alice_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        manager.stats().tombstones,
        0,
        "terminal tombstones must expire without a later client request"
    );
    let error = manager
        .write_stdin(&alice, WriteStdinInput::poll(alice_id))
        .await
        .unwrap_err();
    assert!(matches!(error, ProcessError::UnknownSession { .. }));

    manager.shutdown().await;
    manager.wait_for_idle().await;
    let stats = manager.stats();
    assert_eq!(stats.occupied, 0);
    assert_eq!(stats.live, 0);
    assert_eq!(stats.tombstones, 0);
    assert!(matches!(
        manager.exec_command(&alice, long_command("true")).await,
        Err(ProcessError::ShuttingDown)
    ));
}
