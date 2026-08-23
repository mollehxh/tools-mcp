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
