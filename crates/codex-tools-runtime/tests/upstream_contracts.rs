use codex_tools_runtime::contracts::frozen_tool_contracts;
use codex_tools_runtime::contracts::{ExecCommandInput, ExecCommandOutput, UnifiedExecResult};
use codex_tools_runtime::extraction::{ApplyPatchInput, ApplyPatchOperation, parse_apply_patch};
use std::time::Duration;

#[path = "../../../tests/conformance/exec_command.rs"]
mod exec_command_conformance;
#[path = "../../../tests/conformance/write_stdin.rs"]
mod write_stdin_conformance;

#[test]
fn exactly_six_frozen_contracts_are_exposed() {
    let contracts = frozen_tool_contracts();
    let names = contracts
        .iter()
        .map(|contract| contract.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "exec_command",
            "write_stdin",
            "apply_patch",
            "skills.install",
            "skills.list",
            "skills.read",
        ]
    );
}

#[test]
fn frozen_contracts_reject_undeclared_properties() {
    for contract in frozen_tool_contracts() {
        assert_eq!(
            contract.input_schema["additionalProperties"], false,
            "{} must reject undeclared input fields",
            contract.name
        );
    }
}

#[test]
fn approval_only_fields_are_not_advertised() {
    let rendered = serde_json::to_string(frozen_tool_contracts()).expect("contracts serialize");
    for forbidden in [
        "sandbox_permissions",
        "justification",
        "prefix_rule",
        "additional_permissions",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "found forbidden field {forbidden}"
        );
    }
}

#[test]
fn descriptions_defaults_outputs_and_annotations_are_frozen() {
    let contracts = frozen_tool_contracts();
    let exec = &contracts[0];
    assert_eq!(
        exec.input_schema["properties"]["yield_time_ms"]["description"],
        "Wait before yielding output. Defaults to 10000 ms; effective range is 250-30000 ms."
    );
    assert_eq!(
        exec.output_schema.as_ref().unwrap()["required"],
        serde_json::json!(["wall_time_seconds", "output"])
    );
    assert!(exec.annotations.open_world_hint);
    assert!(!exec.annotations.read_only_hint);

    let apply_patch = &contracts[2];
    assert!(apply_patch.annotations.destructive_hint);
    assert_eq!(
        apply_patch.input_schema["required"],
        serde_json::json!(["patch"])
    );
}

#[test]
fn all_tool_annotations_match_the_six_tool_contract() {
    let actual = frozen_tool_contracts()
        .iter()
        .map(|contract| {
            (
                contract.name.as_str(),
                contract.annotations.read_only_hint,
                contract.annotations.destructive_hint,
                contract.annotations.open_world_hint,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        [
            ("exec_command", false, true, true),
            ("write_stdin", false, true, true),
            ("apply_patch", false, true, false),
            ("skills.install", false, true, true),
            ("skills.list", true, false, false),
            ("skills.read", true, false, false),
        ]
    );
}

#[test]
fn skills_list_items_freeze_host_handles_and_source_metadata() {
    let list = &frozen_tool_contracts()[4];
    let output = list.output_schema.as_ref().unwrap();
    let item = &output["properties"]["skills"]["items"];

    assert_eq!(item["type"], "object");
    assert_eq!(item["additionalProperties"], false);
    assert_eq!(
        item["required"],
        serde_json::json!([
            "authority",
            "scope",
            "package",
            "name",
            "description",
            "main_resource",
            "source"
        ])
    );

    let properties = item["properties"].as_object().unwrap();
    let mut property_names = properties.keys().map(String::as_str).collect::<Vec<_>>();
    property_names.sort_unstable();
    assert_eq!(
        property_names,
        [
            "authority",
            "description",
            "main_resource",
            "name",
            "package",
            "scope",
            "source",
        ]
    );
    assert_eq!(properties["authority"]["type"], "string");
    assert_eq!(properties["authority"]["enum"], serde_json::json!(["host"]));
    assert_eq!(properties["scope"]["type"], "string");
    assert_eq!(
        properties["scope"]["enum"],
        serde_json::json!(["project", "global"])
    );
    for field in ["package", "name", "description", "main_resource"] {
        assert_eq!(
            properties[field]["type"], "string",
            "wrong type for {field}"
        );
    }

    let source = &properties["source"];
    assert_eq!(source["type"], "object");
    assert_eq!(source["required"], serde_json::json!(["kind"]));
    assert_eq!(source["additionalProperties"], false);
    let source_properties = source["properties"].as_object().unwrap();
    let mut source_property_names = source_properties
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    source_property_names.sort_unstable();
    assert_eq!(
        source_property_names,
        ["commit", "kind", "repository", "selector"]
    );
    assert_eq!(source_properties["kind"]["type"], "string");
    assert_eq!(
        source_properties["kind"]["enum"],
        serde_json::json!(["host", "git"])
    );
    for field in ["repository", "commit", "selector"] {
        assert_eq!(
            source_properties[field]["type"], "string",
            "wrong source type for {field}"
        );
    }
}

#[test]
fn every_registered_delta_is_keyed_to_requirements() {
    let registry: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/conformance/fixtures/compatibility-deltas.json"
    ))
    .unwrap();
    let deltas = registry["deltas"].as_array().unwrap();
    assert!(!deltas.is_empty());
    for delta in deltas {
        let requirements = delta["requirements"].as_array().unwrap();
        assert!(!requirements.is_empty());
        let mut seen = std::collections::BTreeSet::new();
        assert!(requirements.iter().all(|id| {
            id.as_str().is_some_and(|value| {
                value
                    .strip_prefix('R')
                    .and_then(|number| number.parse::<u8>().ok())
                    .is_some_and(|number| (1..=22).contains(&number))
                    && seen.insert(value)
            })
        }));
    }
}

#[test]
fn minimal_upstream_extraction_slices_compile_and_execute() {
    let retained = codex_tools_runtime::extraction::retain_head_and_tail(b"abcdef", 4);
    assert_eq!(
        String::from_utf8(retained).unwrap(),
        "ab\n... 2 bytes omitted ...\nef"
    );

    let lines = vec!["alpha".to_string(), "beta".to_string()];
    let pattern = vec![" beta ".to_string()];
    assert_eq!(
        codex_tools_runtime::extraction::find_patch_context(&lines, &pattern),
        Some(1)
    );
}

#[test]
fn unified_exec_adapter_maps_frozen_input_defaults_and_original_result_fields() {
    let input: ExecCommandInput = serde_json::from_value(serde_json::json!({
        "cmd": "cargo test",
        "workdir": "crates/codex-tools-runtime"
    }))
    .unwrap();
    let request = input.into_unified_exec_request();

    assert_eq!(request.cmd, "cargo test");
    assert_eq!(
        request.workdir.as_deref(),
        Some("crates/codex-tools-runtime")
    );
    assert!(!request.tty);
    assert_eq!(request.yield_time_ms, 10_000);
    assert_eq!(request.max_output_tokens, None);
    assert_eq!(request.shell, None);
    assert_eq!(request.login, None);

    let output = ExecCommandOutput::from(UnifiedExecResult {
        chunk_id: Some("abc123".to_string()),
        wall_time: Duration::from_millis(1250),
        exit_code: None,
        process_id: Some(7),
        original_token_count: Some(42),
        output: "still running".to_string(),
    });
    assert_eq!(
        serde_json::to_value(output).unwrap(),
        serde_json::json!({
            "chunk_id": "abc123",
            "wall_time_seconds": 1.25,
            "session_id": 7,
            "original_token_count": 42,
            "output": "still running"
        })
    );
}

#[test]
fn original_apply_patch_parser_is_reached_through_application_seam() {
    let parsed = parse_apply_patch(&ApplyPatchInput {
        patch: "*** Begin Patch\n*** Add File: notes.txt\n+hello\n*** End Patch".to_string(),
    })
    .unwrap();

    assert_eq!(
        parsed.patch,
        "*** Begin Patch\n*** Add File: notes.txt\n+hello\n*** End Patch"
    );
    assert_eq!(
        parsed.operations,
        vec![ApplyPatchOperation::AddFile {
            path: "notes.txt".into(),
            contents: "hello\n".to_string(),
        }]
    );

    let error = parse_apply_patch(&ApplyPatchInput {
        patch: "*** Begin Patch\n*** Add File: notes.txt\nhello\n*** End Patch".to_string(),
    })
    .unwrap_err();
    assert!(error.to_string().contains("not a valid hunk header"));

    let heredoc = parse_apply_patch(&ApplyPatchInput {
        patch: "<<'EOF'\n*** Begin Patch\n*** Delete File: old.txt\n*** End Patch\nEOF\n"
            .to_string(),
    })
    .unwrap();
    assert_eq!(
        heredoc.operations,
        vec![ApplyPatchOperation::DeleteFile {
            path: "old.txt".into(),
        }]
    );
}
