use codex_tools_runtime::contracts::{WriteStdinInput, frozen_tool_contracts};

#[test]
fn write_stdin_contract_and_defaults_match_the_pinned_transcript() {
    let contract = frozen_tool_contracts()
        .iter()
        .find(|contract| contract.name == "write_stdin")
        .unwrap();
    assert_eq!(
        contract.description,
        "Writes characters to an existing unified exec session and returns recent output."
    );
    assert_eq!(
        contract.input_schema["properties"]["yield_time_ms"]["description"],
        "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms; empty polls wait 5000-300000 ms by default."
    );

    let write: WriteStdinInput = serde_json::from_value(serde_json::json!({
        "session_id": 1000,
        "chars": "hello\\n"
    }))
    .unwrap();
    assert_eq!(write.yield_time_ms, 250);
    assert_eq!(write.max_output_tokens, None);

    for poll_json in [
        serde_json::json!({ "session_id": 1000 }),
        serde_json::json!({ "session_id": 1000, "chars": "" }),
    ] {
        let poll: WriteStdinInput = serde_json::from_value(poll_json).unwrap();
        assert_eq!(poll.chars, "");
        assert_eq!(poll.yield_time_ms, 5_000);
    }

    let explicit: WriteStdinInput = serde_json::from_value(serde_json::json!({
        "session_id": 1000,
        "yield_time_ms": 1_000
    }))
    .unwrap();
    assert_eq!(explicit.yield_time_ms, 1_000);
}
