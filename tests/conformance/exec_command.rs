use codex_tools_runtime::contracts::{ExecCommandInput, ExecCommandOutput, frozen_tool_contracts};

#[test]
fn exec_command_contract_and_defaults_match_the_pinned_transcript() {
    let contract = frozen_tool_contracts()
        .iter()
        .find(|contract| contract.name == "exec_command")
        .unwrap();
    assert_eq!(
        contract.description,
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
    );
    assert_eq!(
        contract.input_schema["properties"]["yield_time_ms"]["description"],
        "Wait before yielding output. Defaults to 10000 ms; effective range is 250-30000 ms."
    );

    let input: ExecCommandInput = serde_json::from_value(serde_json::json!({
        "cmd": "printf transcript"
    }))
    .unwrap();
    assert_eq!(input.yield_time_ms, 10_000);
    assert!(!input.tty);
    assert_eq!(input.max_output_tokens, None);
    assert_eq!(input.login, None);

    let result = ExecCommandOutput {
        chunk_id: Some("a1b2c3".to_owned()),
        wall_time_seconds: 0.25,
        exit_code: Some(17),
        session_id: None,
        original_token_count: Some(3),
        output: "failure".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::json!({
            "chunk_id": "a1b2c3",
            "wall_time_seconds": 0.25,
            "exit_code": 17,
            "original_token_count": 3,
            "output": "failure"
        })
    );
}
