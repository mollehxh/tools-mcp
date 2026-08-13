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
fn recoverable_errors_redact_host_paths_from_structured_and_text_content() {
    let cases = [
        (
            "failed to read /Users/alice/private/project/config.json: permission denied",
            "failed to read [redacted-path]: permission denied",
            "/Users/alice/private/project/config.json",
        ),
        (
            r"failed to read C:\Users\alice\private\project\config.json: access denied",
            "failed to read [redacted-path]: access denied",
            r"C:\Users\alice\private\project\config.json",
        ),
        (
            "failed to read /secret: permission denied",
            "failed to read [redacted-path]: permission denied",
            "/secret",
        ),
    ];

    for (message, expected, path) in cases {
        let result = error_result(
            "adapter_failed",
            message,
            Some(json!({
                "kind": "io",
                "path": path,
            })),
        );

        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["error"], "adapter_failed");
        assert_eq!(structured["message"], expected);
        assert_eq!(structured["details"]["kind"], "io");
        assert_eq!(structured["details"]["path"], "[redacted-path]");

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text).unwrap(),
            *structured
        );
        assert!(!text.contains("/Users/alice"));
        assert!(!text.contains(r"C:\Users\alice"));
    }
}

#[test]
fn error_redaction_preserves_urls_resource_handles_and_ordinary_text() {
    let message = "see https://example.com/docs/errors and skill://catalog/rust at /mcp";
    let details = json!({
        "documentation": "https://example.com/docs/errors",
        "resource": "skill://catalog/rust",
        "endpoint": "/mcp",
    });
    let result = error_result("adapter_failed", message, Some(details.clone()));
    let structured = result.structured_content.unwrap();

    assert_eq!(structured["message"], message);
    assert_eq!(structured["details"], details);
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
