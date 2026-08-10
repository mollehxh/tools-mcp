use skill_store::upstream::parse_skill_frontmatter_metadata;
use skill_store::{
    HostSkillMetadata, SkillListInput, SkillListOutput, SkillReadOutput, SkillScope,
};

#[test]
fn pinned_skill_parser_slice_parses_valid_frontmatter() {
    let parsed = parse_skill_frontmatter_metadata(
        "---\nname: sample\ndescription: A sample skill\n---\nbody\n",
        || "fallback".to_string(),
    )
    .unwrap();
    assert_eq!(parsed.name, "sample");
    assert_eq!(parsed.description, "A sample skill");
}

#[test]
fn pinned_skill_parser_slice_rejects_missing_frontmatter() {
    let error =
        parse_skill_frontmatter_metadata("body only", || "fallback".to_string()).unwrap_err();
    assert!(error.to_string().contains("missing YAML frontmatter"));
}

#[test]
fn host_list_contract_composes_exact_read_handle_and_pagination() {
    let input: SkillListInput = serde_json::from_value(serde_json::json!({
        "scope": "project",
        "cursor": "next-page"
    }))
    .unwrap();
    assert_eq!(input.scope, SkillScope::Project);
    assert_eq!(input.cursor.as_deref(), Some("next-page"));

    let page = SkillListOutput::from_host_page(
        SkillScope::Project,
        vec![HostSkillMetadata {
            package: "rust-checks".to_string(),
            name: "rust-checks".to_string(),
            description: "Run the Rust checks".to_string(),
        }],
        vec!["one warning".to_string()],
        Some("page-2".to_string()),
    );

    assert_eq!(page.next_cursor.as_deref(), Some("page-2"));
    assert_eq!(
        page.skills[0].main_resource,
        "skill://host/project/rust-checks/SKILL.md"
    );
    assert_eq!(
        page.skills[0].read_input(Some("resource-page".to_string())),
        serde_json::from_value(serde_json::json!({
            "scope": "project",
            "package": "rust-checks",
            "resource": "skill://host/project/rust-checks/SKILL.md",
            "cursor": "resource-page"
        }))
        .unwrap()
    );
}

#[test]
fn host_read_contract_preserves_resource_and_next_cursor() {
    let response = SkillReadOutput {
        resource: "skill://host/global/release/SKILL.md".to_string(),
        contents: "# Release".to_string(),
        next_cursor: Some("read-page-2".to_string()),
    };

    assert_eq!(
        serde_json::to_value(response).unwrap(),
        serde_json::json!({
            "resource": "skill://host/global/release/SKILL.md",
            "contents": "# Release",
            "next_cursor": "read-page-2"
        })
    );
}
