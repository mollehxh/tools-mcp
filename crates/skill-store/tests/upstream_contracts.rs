use skill_store::upstream::parse_skill_frontmatter_metadata;

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
