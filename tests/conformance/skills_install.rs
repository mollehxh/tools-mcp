use skill_store::{RepositoryEntry, SkillInstallInput, SkillInstallOutput, SkillScope};

pub fn representative_input(scope: SkillScope) -> SkillInstallInput {
    SkillInstallInput {
        repository: "https://github.com/example/skills.git".to_string(),
        selector: None,
        revision: Some("main".to_string()),
        scope,
    }
}

pub fn representative_tree() -> Vec<RepositoryEntry> {
    vec![
        RepositoryEntry::regular(
            "SKILL.md",
            b"---\nname: demo\ndescription: Demonstration\n---\nsecret instructions".to_vec(),
        ),
        RepositoryEntry::regular("references/guide.md", b"guide".to_vec()),
    ]
}

pub fn assert_success_contract(output: &SkillInstallOutput) {
    assert_eq!(output.package, "demo");
    assert_eq!(output.name, "demo");
    assert_eq!(output.main_resource, "skill://host/project/demo/SKILL.md");
    assert_eq!(
        output.source.commit.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert!(
        !serde_json::to_string(output)
            .unwrap()
            .contains("secret instructions")
    );
}
