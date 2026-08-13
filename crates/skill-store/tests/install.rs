use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{
    FetchedRepository, GitFetcher, InstallLimits, RepositoryEntry, SkillCatalog, SkillInstallInput,
    SkillInstaller, SkillListInput, SkillScope,
};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../tests/conformance/skills_install.rs"]
mod conformance;

#[derive(Debug)]
struct FixtureFetcher {
    repository: FetchedRepository,
}

#[derive(Debug)]
struct FailingFetcher;

impl GitFetcher for FailingFetcher {
    fn fetch(
        &self,
        _source: &skill_store::NormalizedGitSource,
        _limits: &InstallLimits,
    ) -> Result<FetchedRepository, skill_store::SkillInstallError> {
        Err(skill_store::SkillInstallError::FetchFailed)
    }
}

impl GitFetcher for FixtureFetcher {
    fn fetch(
        &self,
        source: &skill_store::NormalizedGitSource,
        _limits: &InstallLimits,
    ) -> Result<FetchedRepository, skill_store::SkillInstallError> {
        let mut repository = self.repository.clone();
        repository.repository.clone_from(&source.repository);
        Ok(repository)
    }
}

fn fixture() -> (tempfile::TempDir, WorkspaceAuthority) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let global = root.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    (root, authority)
}

fn repository(entries: Vec<RepositoryEntry>) -> FetchedRepository {
    FetchedRepository {
        repository: "https://github.com/example/skills.git".to_string(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        entries,
    }
}

fn file(path: &str, contents: &str) -> RepositoryEntry {
    RepositoryEntry::regular(path, contents.as_bytes().to_vec())
}

#[test]
fn installs_one_root_skill_without_returning_its_body_and_is_immediately_visible() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        Arc::clone(&catalog),
        Arc::new(FixtureFetcher {
            repository: repository(conformance::representative_tree()),
        }),
    )
    .unwrap();

    let output = installer
        .install(&conformance::representative_input(SkillScope::Project))
        .unwrap();

    conformance::assert_success_contract(&output);
    let listed = catalog
        .list(&SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        })
        .unwrap();
    assert_eq!(listed.skills.len(), 1);
    assert_eq!(listed.skills[0].package, "demo");
}

#[test]
fn cancellation_after_fetch_prevents_durable_publication() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        Arc::clone(&catalog),
        Arc::new(FixtureFetcher {
            repository: repository(conformance::representative_tree()),
        }),
    )
    .unwrap();
    let checks = AtomicUsize::new(0);

    let error = installer
        .install_cancellable(
            &conformance::representative_input(SkillScope::Project),
            || checks.fetch_add(1, Ordering::SeqCst) > 0,
        )
        .unwrap_err();

    assert!(matches!(error, skill_store::SkillInstallError::Cancelled));
    assert!(
        catalog
            .list(&SkillListInput {
                scope: SkillScope::Project,
                cursor: None,
            })
            .unwrap()
            .skills
            .is_empty()
    );
}

#[test]
fn multiple_skills_return_retryable_selectors_without_mutating_either_root() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        catalog,
        Arc::new(FixtureFetcher {
            repository: repository(vec![
                file(
                    "skills/one/SKILL.md",
                    "---\nname: one\ndescription: one\n---\n",
                ),
                file(
                    "skills/two/SKILL.md",
                    "---\nname: two\ndescription: two\n---\n",
                ),
            ]),
        }),
    )
    .unwrap();

    let error = installer
        .install(&SkillInstallInput {
            repository: "https://github.com/example/skills".to_string(),
            selector: None,
            revision: None,
            scope: SkillScope::Global,
        })
        .unwrap_err();
    assert_eq!(
        error.candidate_selectors(),
        &["skills/one".to_string(), "skills/two".to_string()]
    );
    assert!(
        fs::read_dir(authority.project_skills().root())
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        fs::read_dir(authority.global_skills().root())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn selector_and_cross_scope_same_name_work_but_same_scope_collides() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        Arc::clone(&catalog),
        Arc::new(FixtureFetcher {
            repository: repository(vec![
                file(
                    "skills/demo/SKILL.md",
                    "---\nname: demo\ndescription: scoped demo\n---\nbody",
                ),
                file(
                    "skills/other/SKILL.md",
                    "---\nname: other\ndescription: other\n---\n",
                ),
            ]),
        }),
    )
    .unwrap();
    let input = |scope| SkillInstallInput {
        repository: "https://github.com/example/skills.git".to_string(),
        selector: Some("skills/demo".to_string()),
        revision: None,
        scope,
    };

    installer.install(&input(SkillScope::Global)).unwrap();
    installer.install(&input(SkillScope::Project)).unwrap();
    assert!(installer.install(&input(SkillScope::Project)).is_err());
    assert_eq!(
        catalog.resolve_name("demo").unwrap().unwrap().scope,
        SkillScope::Project
    );
    assert!(
        authority
            .global_skills()
            .root()
            .join("demo/SKILL.md")
            .is_file()
    );
    assert!(
        authority
            .project_skills()
            .root()
            .join("demo/SKILL.md")
            .is_file()
    );
}

#[test]
fn controlled_fetch_failure_leaves_both_skill_roots_unchanged() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer =
        SkillInstaller::with_fetcher(&authority, catalog, Arc::new(FailingFetcher)).unwrap();

    assert!(
        installer
            .install(&SkillInstallInput {
                repository: "https://github.com/example/skills.git".to_string(),
                selector: Some("skills/demo".to_string()),
                revision: Some("main".to_string()),
                scope: SkillScope::Project,
            })
            .is_err()
    );
    for root in [
        authority.project_skills().root(),
        authority.global_skills().root(),
    ] {
        assert!(fs::read_dir(root).unwrap().next().is_none());
    }
}

#[test]
fn nameless_skill_uses_the_selected_directory_as_its_package_name() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        Arc::clone(&catalog),
        Arc::new(FixtureFetcher {
            repository: repository(vec![file(
                "skills/fallback/SKILL.md",
                "---\ndescription: Uses its directory name\n---\nbody",
            )]),
        }),
    )
    .unwrap();

    let output = installer
        .install(&SkillInstallInput {
            repository: "https://github.com/example/skills.git".to_string(),
            selector: Some("skills/fallback".to_string()),
            revision: None,
            scope: SkillScope::Project,
        })
        .unwrap();

    assert_eq!(output.package, "fallback");
    assert_eq!(output.name, "fallback");
    assert_eq!(
        catalog.resolve_name("fallback").unwrap().unwrap().package,
        "fallback"
    );
}

#[test]
fn reserved_package_namespace_is_rejected_before_commit() {
    let (_root, authority) = fixture();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer = SkillInstaller::with_fetcher(
        &authority,
        catalog,
        Arc::new(FixtureFetcher {
            repository: repository(vec![file(
                "SKILL.md",
                "---\nname: .mcp-agent-hidden\ndescription: reserved\n---\nbody",
            )]),
        }),
    )
    .unwrap();

    let result = installer.install(&SkillInstallInput {
        repository: "https://github.com/example/skills.git".to_string(),
        selector: None,
        revision: None,
        scope: SkillScope::Global,
    });

    assert!(matches!(
        result,
        Err(skill_store::SkillInstallError::InvalidPackageName)
    ));
    assert!(
        fs::read_dir(authority.global_skills().root())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn installer_recovers_interrupted_staging_without_touching_skill_roots() {
    let (_root, authority) = fixture();
    let project_orphan = authority
        .staging()
        .root()
        .join(".mcp-agent-install-stale-project");
    let global_orphan = authority
        .global_staging()
        .root()
        .join(".mcp-agent-install-stale-global");
    for orphan in [&project_orphan, &global_orphan] {
        fs::create_dir(orphan).unwrap();
        fs::write(orphan.join("partial"), "partial").unwrap();
    }

    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    SkillInstaller::with_fetcher(&authority, catalog, Arc::new(FailingFetcher)).unwrap();

    assert!(!project_orphan.exists());
    assert!(!global_orphan.exists());
    for root in [
        authority.project_skills().root(),
        authority.global_skills().root(),
    ] {
        assert!(fs::read_dir(root).unwrap().next().is_none());
    }
}
