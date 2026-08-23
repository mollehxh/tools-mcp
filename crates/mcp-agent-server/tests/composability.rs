mod common;

use common::{Fixture, arguments, complete};
use serde_json::json;

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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_installed_project_and_global_skills_are_visible_without_a_restart() {
    let fixture = Fixture::new();
    let command = "mkdir -p .agents/skills/command-installed && printf '%s' '---\nname: command-installed\ndescription: installed by command\n---\nbody' > .agents/skills/command-installed/SKILL.md";
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

    let global_command = "mkdir -p \"$CODEX_HOME/skills/global-command\" && printf '%s' '---\nname: global-command\ndescription: installed globally\n---\nbody' > \"$CODEX_HOME/skills/global-command/SKILL.md\"";
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
