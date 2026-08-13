use rmcp::model::Tool;

pub fn assert_exact_tool_surface(tools: &[Tool]) {
    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
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
    assert_eq!(tools.len(), 6, "a seventh tool must never be advertised");
}
