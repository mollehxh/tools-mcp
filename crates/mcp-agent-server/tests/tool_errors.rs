mod common;

use common::{Fixture, arguments, complete};
use mcp_agent_server::result::{error_result, success_result};
use serde_json::json;

#[test]
fn successful_results_have_equivalent_structured_and_text_content() {
    let result = success_result(&json!({"output": "ok", "exit_code": 0})).unwrap();

    assert_eq!(result.is_error, Some(false));
    let structured = result.structured_content.as_ref().unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        *structured
    );
}

#[test]
fn recoverable_errors_are_structured_and_redacted() {
    let result = error_result("unknown_session", "session is not available", None);

    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["error"],
        "unknown_session"
    );
    assert!(
        !result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("/Users/")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_arguments_are_recoverable_without_runtime_action() {
    let fixture = Fixture::new();
    let before = fixture.processes.stats();
    let result = complete(
        fixture
            .handler()
            .call(
                "exec_command",
                Some(arguments(json!({"cmd": 42, "approval_policy": "ask"}))),
            )
            .await,
    );

    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.unwrap()["error"],
        "invalid_arguments"
    );
    assert_eq!(fixture.processes.stats(), before);
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonzero_command_exit_is_a_successful_tool_result() {
    let fixture = Fixture::new();
    #[cfg(unix)]
    let command = "printf failure; exit 17";
    #[cfg(windows)]
    let command = "Write-Output -NoNewline failure; exit 17";
    let result = complete(
        fixture
            .handler()
            .call(
                "exec_command",
                Some(arguments(json!({"cmd": command, "login": false}))),
            )
            .await,
    );

    assert_eq!(result.is_error, Some(false));
    let output = result.structured_content.unwrap();
    assert_eq!(output["exit_code"], 17);
    assert_eq!(output["output"], "failure");
    fixture.processes.shutdown().await;
}
