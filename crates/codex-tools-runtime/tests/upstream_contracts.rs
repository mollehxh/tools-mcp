use codex_tools_runtime::contracts::frozen_tool_contracts;

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
fn every_registered_delta_is_keyed_to_requirements() {
    let deltas: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/conformance/fixtures/compatibility-deltas.json"
    ))
    .unwrap();
    for delta in deltas.as_array().unwrap() {
        let requirements = delta["requirements"].as_array().unwrap();
        assert!(!requirements.is_empty());
        assert!(
            requirements
                .iter()
                .all(|id| id.as_str().unwrap().starts_with('R'))
        );
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
