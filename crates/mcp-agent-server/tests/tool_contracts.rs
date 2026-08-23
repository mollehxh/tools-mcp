use mcp_agent_server::AgentHandler;

#[path = "../../../tests/conformance/tool_surface.rs"]
mod tool_surface;

#[test]
fn advertises_exactly_the_five_frozen_tools_in_order() {
    let tools = AgentHandler::tools();
    tool_surface::assert_exact_tool_surface(&tools);
    assert!(tools.iter().all(|tool| {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let text = schema.to_string();
        !text.contains("approval") && !text.contains("escalat")
    }));
}

#[test]
fn annotations_match_the_five_tool_contract() {
    let tools = AgentHandler::tools();
    let readonly = tools
        .iter()
        .filter(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|value| value.read_only_hint)
                == Some(true)
        })
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();

    assert_eq!(readonly, ["skills.list", "skills.read"]);
    tool_surface::assert_exact_tool_surface(&tools);
}
