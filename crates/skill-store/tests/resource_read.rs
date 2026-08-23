use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{SkillCatalog, SkillReadInput, SkillScope};
use std::fs;

struct Fixture {
    authority: WorkspaceAuthority,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&global).unwrap();
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        Self {
            authority,
            _root: root,
        }
    }

    fn root(&self, scope: SkillScope) -> &std::path::Path {
        match scope {
            SkillScope::System => panic!("system root requires a capability fixture"),
            SkillScope::Project => self.authority.project_skills().root(),
            SkillScope::Global => self.authority.global_skills().root(),
        }
    }

    fn package(&self, scope: SkillScope, package: &str) -> std::path::PathBuf {
        let path = self.root(scope).join(package);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {package}\ndescription: {package} description\n---\nmain body"),
        )
        .unwrap();
        path
    }

    fn read(
        catalog: &SkillCatalog,
        scope: SkillScope,
        package: &str,
        relative: &str,
        cursor: Option<String>,
    ) -> Result<skill_store::SkillReadOutput, skill_store::SkillStoreError> {
        catalog.read(&SkillReadInput {
            scope,
            package: package.to_string(),
            resource: format!(
                "skill://host/{}/{package}/{relative}",
                match scope {
                    SkillScope::System => "system",
                    SkillScope::Project => "project",
                    SkillScope::Global => "global",
                }
            ),
            cursor,
        })
    }
}

#[test]
fn exact_project_global_and_referenced_resources_are_readable() {
    let fixture = Fixture::new();
    let project = fixture.package(SkillScope::Project, "project-skill");
    let global = fixture.package(SkillScope::Global, "global-skill");
    fs::create_dir_all(project.join("references")).unwrap();
    fs::write(project.join("references/guide.md"), "project guide").unwrap();
    fs::write(global.join("notes.md"), "global notes").unwrap();
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    let project_main = Fixture::read(
        &catalog,
        SkillScope::Project,
        "project-skill",
        "SKILL.md",
        None,
    )
    .unwrap();
    assert!(project_main.contents.contains("main body"));
    assert_eq!(
        project_main.resource,
        "skill://host/project/project-skill/SKILL.md"
    );
    assert_eq!(
        Fixture::read(
            &catalog,
            SkillScope::Project,
            "project-skill",
            "references/guide.md",
            None
        )
        .unwrap()
        .contents,
        "project guide"
    );
    assert_eq!(
        Fixture::read(
            &catalog,
            SkillScope::Global,
            "global-skill",
            "notes.md",
            None
        )
        .unwrap()
        .contents,
        "global notes"
    );
}

#[test]
fn traversal_wrong_scope_and_non_exact_handles_are_rejected_without_path_leaks() {
    let fixture = Fixture::new();
    fixture.package(SkillScope::Project, "safe");
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    for resource in [
        "skill://host/project/safe/../outside",
        "skill://host/project/safe/references\\outside",
        "skill://host/global/safe/SKILL.md",
        "file:///tmp/SKILL.md",
    ] {
        let error = catalog
            .read(&SkillReadInput {
                scope: SkillScope::Project,
                package: "safe".to_string(),
                resource: resource.to_string(),
                cursor: None,
            })
            .unwrap_err();
        assert!(
            !error
                .to_string()
                .contains(fixture.root(SkillScope::Project).to_string_lossy().as_ref())
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_resources_and_symlink_components_are_rejected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let package = fixture.package(SkillScope::Project, "safe");
    let outside = fixture.root(SkillScope::Global).join("outside.txt");
    fs::write(&outside, "secret").unwrap();
    symlink(&outside, package.join("link.txt")).unwrap();
    symlink(fixture.root(SkillScope::Global), package.join("linked-dir")).unwrap();
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    for relative in ["link.txt", "linked-dir/outside.txt"] {
        let error =
            Fixture::read(&catalog, SkillScope::Project, "safe", relative, None).unwrap_err();
        assert!(error.to_string().contains("failed to read skill resource"));
        assert!(
            !error
                .to_string()
                .contains(outside.to_string_lossy().as_ref())
        );
    }
}

#[test]
fn invalid_utf8_is_rejected_and_large_utf8_resources_page_on_char_boundaries() {
    let fixture = Fixture::new();
    let package = fixture.package(SkillScope::Project, "large");
    fs::write(package.join("invalid.bin"), [0xff, 0xfe]).unwrap();
    fs::write(package.join("large.md"), "💡".repeat(200_000)).unwrap();
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    let invalid =
        Fixture::read(&catalog, SkillScope::Project, "large", "invalid.bin", None).unwrap_err();
    assert!(invalid.to_string().contains("UTF-8"));

    let first = Fixture::read(&catalog, SkillScope::Project, "large", "large.md", None).unwrap();
    assert!(first.next_cursor.is_some());
    assert!(serde_json::to_vec(&first).unwrap().len() <= 512 * 1024);
    assert!(first.contents.chars().all(|character| character == '💡'));
    let second = Fixture::read(
        &catalog,
        SkillScope::Project,
        "large",
        "large.md",
        first.next_cursor,
    )
    .unwrap();
    assert!(serde_json::to_vec(&second).unwrap().len() <= 512 * 1024);
}

#[test]
fn handles_longer_than_the_pinned_limit_are_rejected() {
    let fixture = Fixture::new();
    fixture.package(SkillScope::Project, "bounded");
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let error = catalog
        .read(&SkillReadInput {
            scope: SkillScope::Project,
            package: "bounded".to_string(),
            resource: format!("skill://host/project/bounded/{}", "x".repeat(2_048)),
            cursor: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("at most 2048 bytes"));
}
