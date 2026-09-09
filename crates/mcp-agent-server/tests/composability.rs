mod common;

use common::{Fixture, arguments, complete};
use mcp_agent_tool_contracts::{
    CallContext, CallIdentity, ExecCommandInput, SkillListInput, SkillScope, ToolBackend,
    ToolOutput, ToolRequest,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn write_project_skill(project: &std::path::Path, body: &str) {
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let skill = project.join(".agents/skills/repository-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: repository-skill\ndescription: repository scoped\n---\n{body}"),
    )
    .unwrap();
}

fn call_context(session: &str) -> CallContext {
    CallContext::new(CancellationToken::new(), None).with_identity(CallIdentity {
        principal_fingerprint: "principal-test".to_owned(),
        session_fingerprint: session.to_owned(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_is_visible_to_a_fresh_skill_handler() {
    let fixture = Fixture::new();
    let patch = "*** Begin Patch\n*** Add File: note.txt\n+hello\n*** End Patch";
    let result = complete(
        fixture
            .handler()
            .call("apply_patch", Some(arguments(json!({"patch": patch}))))
            .await,
    );
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("note.txt")).unwrap(),
        "hello\n"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_installer_guidance_lists_and_reads_the_exact_system_origin() {
    let fixture = Fixture::new();
    let listed = complete(
        fixture
            .handler()
            .call("skills.list", Some(arguments(json!({"scope": "system"}))))
            .await,
    );
    assert_eq!(listed.is_error, Some(false));
    let skill = &listed.structured_content.as_ref().unwrap()["skills"][0];
    assert_eq!(skill["scope"], "system");
    assert_eq!(skill["package"], "skill-installer");
    assert_eq!(
        skill["main_resource"],
        "skill://host/system/skill-installer/SKILL.md"
    );

    let read = complete(
        fixture
            .handler()
            .call(
                "skills.read",
                Some(arguments(json!({
                    "scope": "system",
                    "package": "skill-installer",
                    "resource": "skill://host/system/skill-installer/SKILL.md"
                }))),
            )
            .await,
    );
    assert_eq!(read.is_error, Some(false));
    assert!(
        read.structured_content.unwrap()["contents"]
            .as_str()
            .unwrap()
            .contains("name: skill-installer")
    );
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_installed_project_and_global_skills_are_visible_without_a_restart() {
    let fixture = Fixture::new();
    #[cfg(unix)]
    let command = "mkdir -p .agents/skills/command-installed && printf '%s' '---\nname: command-installed\ndescription: installed by command\n---\nbody' > .agents/skills/command-installed/SKILL.md";
    #[cfg(windows)]
    let command = "$p='.agents/skills/command-installed'; New-Item -ItemType Directory -Path $p -Force | Out-Null; [IO.File]::WriteAllText((Join-Path $p 'SKILL.md'),\"---`nname: command-installed`ndescription: installed by command`n---`nbody\")";
    let installed = complete(
        fixture
            .handler()
            .call(
                "exec_command",
                Some(arguments(json!({"cmd": command, "yield_time_ms": 10000}))),
            )
            .await,
    );
    assert_eq!(installed.is_error, Some(false));

    let listed = complete(
        fixture
            .handler()
            .call("skills.list", Some(arguments(json!({"scope": "project"}))))
            .await,
    );
    assert_eq!(listed.is_error, Some(false));
    assert_eq!(
        listed.structured_content.unwrap()["skills"][0]["package"],
        "command-installed"
    );

    #[cfg(unix)]
    let global_command = "mkdir -p \"$CODEX_HOME/skills/global-command\" && printf '%s' '---\nname: global-command\ndescription: installed globally\n---\nbody' > \"$CODEX_HOME/skills/global-command/SKILL.md\"";
    #[cfg(windows)]
    let global_command = "$p=Join-Path $env:CODEX_HOME 'skills/global-command'; New-Item -ItemType Directory -Path $p -Force | Out-Null; [IO.File]::WriteAllText((Join-Path $p 'SKILL.md'),\"---`nname: global-command`ndescription: installed globally`n---`nbody\")";
    let installed = complete(
        fixture
            .handler()
            .call(
                "exec_command",
                Some(arguments(
                    json!({"cmd": global_command, "yield_time_ms": 10000}),
                )),
            )
            .await,
    );
    assert_eq!(installed.is_error, Some(false));
    let global = complete(
        fixture
            .handler()
            .call("skills.list", Some(arguments(json!({"scope": "global"}))))
            .await,
    );
    assert_eq!(global.is_error, Some(false));
    assert_eq!(
        global.structured_content.unwrap()["skills"][0]["package"],
        "global-command"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_skills_follow_the_git_repository_selected_by_command_workdir() {
    let fixture = Fixture::new();
    let first = fixture.workspace.join("first-repository");
    let second = fixture.workspace.join("second-repository");
    write_project_skill(&first, "first repository body");
    write_project_skill(&second, "second repository body");

    let mut first_handle = None;
    for (project, expected) in [
        (&first, "first repository body"),
        (&second, "second repository body"),
    ] {
        let workdir = project.strip_prefix(&fixture.workspace).unwrap();
        let selected = complete(
            fixture
                .handler()
                .call(
                    "exec_command",
                    Some(arguments(json!({
                        "cmd": "pwd",
                        "workdir": workdir,
                        "yield_time_ms": 10000
                    }))),
                )
                .await,
        );
        assert_eq!(selected.is_error, Some(false), "{selected:?}");

        // A command that uses the default execution directory must not silently
        // discard the explicit project selection for subsequent skill calls.
        let default_command = complete(
            fixture
                .handler()
                .call(
                    "exec_command",
                    Some(arguments(json!({"cmd": "pwd", "yield_time_ms": 10000}))),
                )
                .await,
        );
        assert_eq!(default_command.is_error, Some(false));

        let listed = complete(
            fixture
                .handler()
                .call("skills.list", Some(arguments(json!({"scope": "project"}))))
                .await,
        );
        assert_eq!(listed.is_error, Some(false));
        let skill = &listed.structured_content.as_ref().unwrap()["skills"][0];
        assert_eq!(skill["name"], "repository-skill");
        let package = skill["package"].as_str().unwrap().to_owned();
        let resource = skill["main_resource"].as_str().unwrap().to_owned();
        let read = complete(
            fixture
                .handler()
                .call(
                    "skills.read",
                    Some(arguments(json!({
                        "scope": "project",
                        "package": package.clone(),
                        "resource": resource.clone()
                    }))),
                )
                .await,
        );
        assert_eq!(read.is_error, Some(false));
        assert!(
            read.structured_content.unwrap()["contents"]
                .as_str()
                .unwrap()
                .contains(expected)
        );
        if first_handle.is_none() {
            first_handle = Some((package, resource));
        }
    }

    let (package, resource) = first_handle.unwrap();
    let stale = complete(
        fixture
            .handler()
            .call(
                "skills.read",
                Some(arguments(json!({
                    "scope": "project",
                    "package": package,
                    "resource": resource
                }))),
            )
            .await,
    );
    assert_eq!(stale.is_error, Some(true));
    assert_eq!(
        stale.structured_content.unwrap()["error"],
        "invalid_resource"
    );

    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_project_selection_is_isolated_between_mcp_sessions() {
    let fixture = Fixture::new();
    let first = fixture.workspace.join("first-repository");
    let second = fixture.workspace.join("second-repository");
    write_project_skill(&first, "first repository body");
    write_project_skill(&second, "second repository body");

    for (session, project) in [("chat-a", &first), ("chat-b", &second)] {
        fixture
            .backend
            .call(
                call_context(session),
                ToolRequest::ExecCommand(ExecCommandInput {
                    cmd: "pwd".to_owned(),
                    workdir: Some(
                        project
                            .strip_prefix(&fixture.workspace)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    tty: false,
                    yield_time_ms: 10_000,
                    max_output_tokens: None,
                    shell: None,
                    login: None,
                }),
            )
            .await
            .unwrap();
    }

    for (session, expected) in [
        ("chat-a", "first repository body"),
        ("chat-b", "second repository body"),
    ] {
        let ToolOutput::SkillsList(listed) = fixture
            .backend
            .call(
                call_context(session),
                ToolRequest::SkillsList(SkillListInput {
                    scope: SkillScope::Project,
                    cursor: None,
                }),
            )
            .await
            .unwrap()
        else {
            panic!("expected project skill list");
        };
        let skill = &listed.skills[0];
        let ToolOutput::SkillsRead(read) = fixture
            .backend
            .call(
                call_context(session),
                ToolRequest::SkillsRead(skill.read_input(None)),
            )
            .await
            .unwrap()
        else {
            panic!("expected project skill read");
        };
        assert!(read.contents.contains(expected));
    }

    fixture.processes.shutdown().await;
}
