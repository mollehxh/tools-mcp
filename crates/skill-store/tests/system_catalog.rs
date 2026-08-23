use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use skill_store::{SkillCatalog, SkillListInput, SkillReadInput, SkillScope};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Fixture {
    root: tempfile::TempDir,
    authority: WorkspaceAuthority,
    system_skills: PathBuf,
    project_skills: PathBuf,
    global_skills: PathBuf,
}

impl Fixture {
    fn new(codex_home_in_workspace: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let release = root.path().join("release");
        let system_skills = release.join("system-skills");
        let home = root.path().join("home");
        let tmp = root.path().join("tmp");
        for directory in [&workspace, &system_skills, &home, &tmp] {
            fs::create_dir_all(directory).unwrap();
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/openai-codex/skill-installer");
        copy_tree(&source, &system_skills.join("skill-installer"));
        let codex_home = if codex_home_in_workspace {
            workspace.join(".codex")
        } else {
            home.join(".codex")
        };
        let mut environment = BTreeMap::<String, OsString>::new();
        environment.insert("HOME".to_string(), home.into_os_string());
        environment.insert(
            "CODEX_HOME".to_string(),
            codex_home.clone().into_os_string(),
        );
        let capabilities = Arc::new(
            CapabilitySnapshot::resolve_configured(
                &workspace,
                &system_skills,
                |name| environment.get(name).cloned(),
                tmp.clone(),
                tmp,
            )
            .unwrap(),
        );
        let global_skills = capabilities.global_skills().to_path_buf();
        let authority = WorkspaceAuthority::from_capabilities(capabilities).unwrap();
        let project_skills = workspace.join(".agents/skills");
        Self {
            root,
            authority,
            system_skills,
            project_skills,
            global_skills,
        }
    }

    fn write_user_collision(&self, scope: SkillScope, description: &str) {
        let root = match scope {
            SkillScope::Project => &self.project_skills,
            SkillScope::Global => &self.global_skills,
            SkillScope::System => panic!("system is packaged"),
        };
        let package = root.join("skill-installer");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("SKILL.md"),
            format!("---\nname: skill-installer\ndescription: {description}\n---\nuser body"),
        )
        .unwrap();
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
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
fn packaged_installer_is_listed_readable_and_reserved_over_user_collisions() {
    let fixture = Fixture::new(false);
    fixture.write_user_collision(SkillScope::Project, "project collision");
    fixture.write_user_collision(SkillScope::Global, "global collision");
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    let system = list(&catalog, SkillScope::System);
    assert_eq!(system.skills.len(), 1);
    assert_eq!(system.skills[0].package, "skill-installer");
    assert_eq!(
        system.skills[0].main_resource,
        "skill://host/system/skill-installer/SKILL.md"
    );
    let selected = catalog.resolve_name("skill-installer").unwrap().unwrap();
    assert_eq!(selected.scope, SkillScope::System);
    assert_eq!(list(&catalog, SkillScope::Project).skills.len(), 1);
    assert_eq!(list(&catalog, SkillScope::Global).skills.len(), 1);
    for scope in [SkillScope::Project, SkillScope::Global] {
        let user = list(&catalog, scope).skills.into_iter().next().unwrap();
        assert_eq!(user.package, "skill-installer");
        assert_eq!(
            catalog.read(&user.read_input(None)).unwrap().contents,
            match scope {
                SkillScope::Project | SkillScope::Global => {
                    let description = if scope == SkillScope::Project {
                        "project collision"
                    } else {
                        "global collision"
                    };
                    format!(
                        "---\nname: skill-installer\ndescription: {description}\n---\nuser body"
                    )
                }
                SkillScope::System => unreachable!(),
            }
        );
    }

    let read = catalog
        .read(&SkillReadInput {
            scope: SkillScope::System,
            package: "skill-installer".to_string(),
            resource: "skill://host/system/skill-installer/SKILL.md".to_string(),
            cursor: None,
        })
        .unwrap();
    assert!(read.contents.contains("name: skill-installer"));
    assert!(
        fixture
            .system_skills
            .join("skill-installer/assets/skill-installer.png")
            .is_file()
    );
    let binary = catalog
        .read(&SkillReadInput {
            scope: SkillScope::System,
            package: "skill-installer".to_string(),
            resource: "skill://host/system/skill-installer/assets/skill-installer.png".to_string(),
            cursor: None,
        })
        .unwrap_err();
    assert!(binary.to_string().contains("UTF-8"));
}

#[test]
fn nested_codex_home_and_both_mutable_roots_can_be_replaced() {
    let fixture = Fixture::new(true);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();

    for root in [&fixture.project_skills, &fixture.global_skills] {
        let old = root.with_extension("old");
        fs::rename(root, old).unwrap();
        fs::create_dir_all(root.join("fresh")).unwrap();
        fs::write(
            root.join("fresh/SKILL.md"),
            "---\nname: fresh\ndescription: recreated\n---\nbody",
        )
        .unwrap();
    }

    assert_eq!(list(&catalog, SkillScope::Project).skills[0].name, "fresh");
    assert_eq!(list(&catalog, SkillScope::Global).skills[0].name, "fresh");
}

#[test]
fn replacing_nested_codex_home_itself_is_visible() {
    let fixture = Fixture::new(true);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let codex_home = fixture.global_skills.parent().unwrap();
    let old = codex_home.with_extension("old");
    fs::rename(codex_home, old).unwrap();
    fs::create_dir_all(codex_home.join("skills/recreated")).unwrap();
    fs::write(
        codex_home.join("skills/recreated/SKILL.md"),
        "---\nname: recreated\ndescription: new CODEX_HOME\n---\nbody",
    )
    .unwrap();

    assert_eq!(
        list(&catalog, SkillScope::Global).skills[0].name,
        "recreated"
    );
}

#[cfg(unix)]
#[test]
fn external_global_skill_symlink_replacement_is_rejected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new(false);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let outside = fixture.root.path().join("outside-global");
    fs::create_dir_all(outside.join("escaped")).unwrap();
    fs::write(
        outside.join("escaped/SKILL.md"),
        "---\nname: escaped\ndescription: outside\n---\nbody",
    )
    .unwrap();
    let old = fixture.global_skills.with_extension("old");
    fs::rename(&fixture.global_skills, old).unwrap();
    symlink(&outside, &fixture.global_skills).unwrap();

    let page = list(&catalog, SkillScope::Global);
    assert!(page.skills.is_empty());
    assert_eq!(page.warnings.len(), 1);
}

#[test]
fn system_mutation_fails_closed_on_later_list_and_read() {
    let fixture = Fixture::new(false);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    assert_eq!(list(&catalog, SkillScope::System).skills.len(), 1);

    fs::write(
        fixture.system_skills.join("skill-installer/SKILL.md"),
        "---\nname: skill-installer\ndescription: tampered\n---\n",
    )
    .unwrap();

    assert!(
        catalog
            .list(&SkillListInput {
                scope: SkillScope::System,
                cursor: None
            })
            .is_err()
    );
    assert!(
        catalog
            .read(&SkillReadInput {
                scope: SkillScope::System,
                package: "skill-installer".to_string(),
                resource: "skill://host/system/skill-installer/SKILL.md".to_string(),
                cursor: None,
            })
            .is_err()
    );
}

#[test]
fn replacing_the_verified_system_root_fails_identity_revalidation() {
    let fixture = Fixture::new(false);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let old = fixture.system_skills.with_extension("old");
    fs::rename(&fixture.system_skills, &old).unwrap();
    copy_tree(&old, &fixture.system_skills);

    let error = catalog
        .list(&SkillListInput {
            scope: SkillScope::System,
            cursor: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("identity or digest"));
}

#[test]
fn replacing_the_verified_installer_package_fails_identity_revalidation() {
    let fixture = Fixture::new(false);
    let catalog = SkillCatalog::new(&fixture.authority).unwrap();
    let package = fixture.system_skills.join("skill-installer");
    let old = fixture.system_skills.join("skill-installer-old");
    fs::rename(&package, &old).unwrap();
    copy_tree(&old, &package);

    assert!(
        catalog
            .list(&SkillListInput {
                scope: SkillScope::System,
                cursor: None,
            })
            .is_err()
    );
}
