mod common;

use common::{Fixture, arguments, complete};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_result_composes_with_fresh_list_and_read_handlers() {
    let fixture = Fixture::new();
    let installed = complete(
        fixture
            .handler()
            .call(
                "skills.install",
                Some(arguments(json!({
                    "source": "https://example.com/skills.git",
                    "scope": "project"
                }))),
            )
            .await,
    );
    assert_eq!(installed.is_error, Some(false));
    let output = installed.structured_content.unwrap();
    assert_eq!(
        output.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["commit", "main_resource", "package"]
    );

    let listed = complete(
        fixture
            .handler()
            .call("skills.list", Some(arguments(json!({"scope": "project"}))))
            .await,
    );
    assert_eq!(
        listed.structured_content.unwrap()["skills"][0]["name"],
        "installed"
    );

    let read = complete(
        fixture
            .handler()
            .call(
                "skills.read",
                Some(arguments(json!({
                    "scope": "project",
                    "package": output["package"],
                    "resource": output["main_resource"]
                }))),
            )
            .await,
    );
    assert_eq!(read.is_error, Some(false));
    assert!(
        read.structured_content.unwrap()["contents"]
            .as_str()
            .unwrap()
            .contains("installed fixture")
    );
    fixture.processes.shutdown().await;
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
