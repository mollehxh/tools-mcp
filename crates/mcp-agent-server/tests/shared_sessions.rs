mod common;

use common::{Fixture, arguments, complete, yielded_command};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_handlers_share_owner_scoped_process_state() {
    let fixture = Fixture::new();
    let initial = complete(
        fixture
            .handler()
            .call(
                "exec_command",
                Some(arguments(json!({
                    "cmd": yielded_command(),
                    "yield_time_ms": 250,
                    "login": false
                }))),
            )
            .await,
    );
    assert_eq!(initial.is_error, Some(false));
    let session_id = initial.structured_content.unwrap()["session_id"]
        .as_i64()
        .expect("command should yield");

    let final_result = complete(
        fixture
            .handler()
            .call(
                "write_stdin",
                Some(arguments(json!({
                    "session_id": session_id,
                    "yield_time_ms": 5_000
                }))),
            )
            .await,
    );
    assert_eq!(final_result.is_error, Some(false));
    let output = final_result.structured_content.unwrap();
    assert!(output["output"].as_str().unwrap().contains("second"));
    assert_eq!(output["exit_code"], 0);
    fixture.processes.shutdown().await;
}
