use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{SkillCatalog, SkillListInput, SkillScope};
use std::fs;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    authority: WorkspaceAuthority,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("home").join(".agents/skills");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&global).unwrap();
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        Self { root, authority }
    }

    fn root(&self, scope: SkillScope) -> &std::path::Path {
        match scope {
            SkillScope::System => panic!("system root requires a capability fixture"),
            SkillScope::Project => self.authority.project_skills().root(),
            SkillScope::Global => self.authority.global_skills().root(),
        }
    }

    fn write_skill(&self, scope: SkillScope, package: &str, name: &str, description: &str) {
        let package_dir = self.root(scope).join(package);
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(
            package_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
        )
        .unwrap();
    }
}

fn list(catalog: &SkillCatalog, scope: SkillScope) -> skill_store::SkillListOutput {
    catalog
        .list(&SkillListInput {
            scope,
            cursor: None,
        })
        .unwrap()
}

#[test]
fn empty_roots_return_an_empty_catalog() {
    let fixture = Fixture::new();
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    for scope in [SkillScope::Project, SkillScope::Global] {
        let page = list(&catalog, scope);
        assert!(page.skills.is_empty());
        assert!(page.warnings.is_empty());
        assert_eq!(page.next_cursor, None);
    }
}

#[test]
fn valid_skills_are_listed_and_malformed_skills_become_safe_warnings() {
    let fixture = Fixture::new();
    fixture.write_skill(SkillScope::Project, "valid", "valid", "A valid skill");
    let malformed = fixture.root(SkillScope::Project).join("malformed");
    fs::create_dir_all(&malformed).unwrap();
    fs::write(malformed.join("SKILL.md"), "not frontmatter").unwrap();

    let page = list(
        &SkillCatalog::new(&fixture.authority).unwrap(),
        SkillScope::Project,
    );
    assert_eq!(page.skills.len(), 1);
    assert_eq!(page.skills[0].package, "valid");
    assert_eq!(
        page.skills[0].main_resource,
        "skill://host/project/valid/SKILL.md"
    );
    assert_eq!(page.warnings.len(), 1);
    assert!(page.warnings[0].contains("malformed"));
    assert!(!page.warnings[0].contains(fixture.root.path().to_string_lossy().as_ref()));
}

#[test]
fn skills_are_ordered_by_canonical_name_then_package() {
    let fixture = Fixture::new();
    fixture.write_skill(SkillScope::Project, "a-package", "zulu", "Last by name");
    fixture.write_skill(SkillScope::Project, "z-package", "alpha", "First by name");
    fixture.write_skill(
        SkillScope::Project,
        "c-package",
        "shared",
        "Second shared package",
    );
    fixture.write_skill(
        SkillScope::Project,
        "b-package",
        "shared",
        "First shared package",
    );

    let page = list(
        &SkillCatalog::new(&fixture.authority).unwrap(),
        SkillScope::Project,
    );
    let ordered = page
        .skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.package.as_str()))
        .collect::<Vec<_>>();

    assert_eq!(
        ordered,
        vec![
            ("alpha", "z-package"),
            ("shared", "b-package"),
            ("shared", "c-package"),
            ("zulu", "a-package"),
        ]
    );
}

#[test]
fn first_page_warnings_are_finally_bounded_and_utf8_safe() {
    let fixture = Fixture::new();
    let long_package = "a".repeat(240);
    for package in [&long_package, "b", "c", "d", "e"] {
        let package_dir = fixture.root(SkillScope::Project).join(package);
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(package_dir.join("SKILL.md"), "not frontmatter").unwrap();
    }

    let page = list(
        &SkillCatalog::new(&fixture.authority).unwrap(),
        SkillScope::Project,
    );

    assert_eq!(page.warnings.len(), 4);
    assert!(page.warnings.iter().all(|warning| warning.len() <= 256));
    assert_eq!(page.warnings[0].len(), 256);
    assert!(page.warnings[0].contains(&long_package[..180]));
}

#[test]
fn project_precedence_keeps_both_origins_visible_and_exactly_addressable() {
    let fixture = Fixture::new();
    fixture.write_skill(
        SkillScope::Global,
        "shared-global",
        "shared",
        "Global version",
    );
    fixture.write_skill(
        SkillScope::Project,
        "shared-project",
        "shared",
        "Project version",
    );
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    assert_eq!(
        list(&catalog, SkillScope::Project).skills[0].description,
        "Project version"
    );
    assert_eq!(
        list(&catalog, SkillScope::Global).skills[0].description,
        "Global version"
    );
    let selected = catalog.resolve_name("shared").unwrap().unwrap();
    assert_eq!(selected.scope, SkillScope::Project);
    assert_eq!(selected.package, "shared-project");
}

#[test]
fn replacing_mutable_skill_roots_is_visible_without_rebuilding_the_catalog() {
    let fixture = Fixture::new();
    fixture.write_skill(
        SkillScope::Project,
        "before",
        "before",
        "Before replacement",
    );
    fixture.write_skill(SkillScope::Global, "before", "before", "Before replacement");
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    for scope in [SkillScope::Project, SkillScope::Global] {
        let root = fixture.root(scope).to_path_buf();
        let displaced = root.with_extension(match scope {
            SkillScope::Project => "project-old",
            SkillScope::Global => "global-old",
            SkillScope::System => "system-old",
        });
        fs::rename(&root, &displaced).unwrap();
        fs::create_dir_all(&root).unwrap();
        fixture.write_skill(scope, "after", "after", "After replacement");

        let page = list(&catalog, scope);
        assert_eq!(page.skills.len(), 1);
        assert_eq!(page.skills[0].package, "after");
    }
}

#[test]
fn command_or_patch_style_edits_are_reconciled_before_each_list() {
    let fixture = Fixture::new();
    fixture.write_skill(SkillScope::Project, "editable", "editable", "Before");
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    assert_eq!(
        list(&catalog, SkillScope::Project).skills[0].description,
        "Before"
    );

    fixture.write_skill(
        SkillScope::Project,
        "editable",
        "editable",
        "After command edit",
    );
    assert_eq!(
        list(&catalog, SkillScope::Project).skills[0].description,
        "After command edit"
    );

    fixture.write_skill(
        SkillScope::Project,
        "editable",
        "editable",
        "After patch edit",
    );
    assert_eq!(
        list(&catalog, SkillScope::Project).skills[0].description,
        "After patch edit"
    );

    fs::rename(
        fixture.root(SkillScope::Project).join("editable"),
        fixture.root(SkillScope::Project).join("renamed"),
    )
    .unwrap();
    assert_eq!(
        list(&catalog, SkillScope::Project).skills[0].package,
        "renamed"
    );
    fs::remove_dir_all(fixture.root(SkillScope::Project).join("renamed")).unwrap();
    assert!(list(&catalog, SkillScope::Project).skills.is_empty());

    fixture.write_skill(
        SkillScope::Project,
        "editable",
        "editable",
        "After recreation",
    );

    let rebuilt = SkillCatalog::new(&fixture.authority).unwrap();
    assert_eq!(
        list(&rebuilt, SkillScope::Project).skills[0].description,
        "After recreation"
    );
}

#[test]
fn pagination_cursor_becomes_stale_after_a_catalog_mutation() {
    let fixture = Fixture::new();
    for index in 0..21 {
        fixture.write_skill(
            SkillScope::Project,
            &format!("package-{index:02}"),
            &format!("skill-{index:02}"),
            "paged",
        );
    }
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let first = list(&catalog, SkillScope::Project);
    let cursor = first.next_cursor.unwrap();
    fixture.write_skill(SkillScope::Project, "new-package", "new-skill", "mutation");

    let error = catalog
        .list(&SkillListInput {
            scope: SkillScope::Project,
            cursor: Some(cursor),
        })
        .unwrap_err();
    assert!(error.to_string().contains("stale"));
}

#[cfg(unix)]
#[test]
fn symlink_root_replacement_is_rejected_and_later_recreation_is_visible() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let outside = fixture.root.path().join("outside");
    fs::create_dir_all(outside.join("skills/escaped")).unwrap();
    fs::write(
        outside.join("skills/escaped/SKILL.md"),
        "---\nname: escaped\ndescription: outside\n---\nbody",
    )
    .unwrap();
    let project_agents = fixture.authority.workspace_root().join(".agents");
    let old_agents = fixture.authority.workspace_root().join(".agents-old");
    fs::rename(&project_agents, &old_agents).unwrap();
    symlink(&outside, &project_agents).unwrap();

    let rejected = list(&catalog, SkillScope::Project);
    assert!(rejected.skills.is_empty());
    assert_eq!(rejected.warnings.len(), 1);

    fs::remove_file(&project_agents).unwrap();
    fs::create_dir_all(project_agents.join("skills/fresh")).unwrap();
    fs::write(
        project_agents.join("skills/fresh/SKILL.md"),
        "---\nname: fresh\ndescription: recreated\n---\nbody",
    )
    .unwrap();
    assert_eq!(list(&catalog, SkillScope::Project).skills[0].name, "fresh");
}

#[test]
fn oversized_description_is_truncated_without_exceeding_the_response_limit() {
    let fixture = Fixture::new();
    fixture.write_skill(
        SkillScope::Project,
        "oversized",
        "oversized",
        &"x".repeat(600 * 1024),
    );
    let page = list(
        &SkillCatalog::new(&fixture.authority).unwrap(),
        SkillScope::Project,
    );
    assert_eq!(page.skills.len(), 1);
    assert_eq!(page.skills[0].description.chars().count(), 1_027);
    assert!(page.skills[0].description.ends_with("..."));
    assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
}
