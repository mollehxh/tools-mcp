#[test]
fn inspector_is_pinned_and_requires_the_complete_tool_surface() {
    assert_eq!(
        xtask::inspector::inspector_package(),
        "@modelcontextprotocol/inspector@2.1.0"
    );
    xtask::inspector::validate_tool_names([
        "exec_command",
        "write_stdin",
        "apply_patch",
        "skills.list",
        "skills.read",
    ])
    .unwrap();
    assert!(xtask::inspector::validate_tool_names(["exec_command", "write_stdin"]).is_err());
}
