use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{
    FetchedRepository, GitFetcher, InstallLimits, RepositoryEntry, SkillCatalog, SkillInstallInput,
    SkillInstaller, SkillScope,
};
use std::fs;
use std::sync::{Arc, Barrier};

#[derive(Debug)]
struct SameFetcher;

#[derive(Debug)]
struct BlockingFetcher {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl GitFetcher for SameFetcher {
    fn fetch(
        &self,
        _source: &skill_store::NormalizedGitSource,
        _limits: &InstallLimits,
    ) -> Result<FetchedRepository, skill_store::SkillInstallError> {
        Ok(FetchedRepository {
            repository: "https://github.com/example/demo.git".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            entries: vec![RepositoryEntry::regular(
                "SKILL.md",
                b"---\nname: demo\ndescription: demo\n---\nbody".to_vec(),
            )],
        })
    }
}

impl GitFetcher for BlockingFetcher {
    fn fetch(
        &self,
        source: &skill_store::NormalizedGitSource,
        limits: &InstallLimits,
    ) -> Result<FetchedRepository, skill_store::SkillInstallError> {
        self.entered.wait();
        self.release.wait();
        SameFetcher.fetch(source, limits)
    }
}

#[test]
fn concurrent_same_scope_installs_have_exactly_one_winner() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installers = [
        Arc::new(
            SkillInstaller::with_fetcher(&authority, Arc::clone(&catalog), Arc::new(SameFetcher))
                .unwrap(),
        ),
        Arc::new(SkillInstaller::with_fetcher(&authority, catalog, Arc::new(SameFetcher)).unwrap()),
    ];
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for installer in installers {
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            installer.install(&SkillInstallInput {
                repository: "https://github.com/example/demo.git".to_string(),
                selector: None,
                revision: None,
                scope: SkillScope::Project,
            })
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert!(
        authority
            .project_skills()
            .root()
            .join("demo/SKILL.md")
            .is_file()
    );
}

#[test]
fn command_created_destination_is_never_replaced() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer =
        SkillInstaller::with_fetcher(&authority, catalog, Arc::new(SameFetcher)).unwrap();
    let destination = authority.project_skills().root().join("demo");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("command-sentinel"), "unchanged").unwrap();

    assert!(
        installer
            .install(&SkillInstallInput {
                repository: "https://github.com/example/demo.git".to_string(),
                selector: None,
                revision: None,
                scope: SkillScope::Project,
            })
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(destination.join("command-sentinel")).unwrap(),
        "unchanged"
    );
    assert!(!destination.join("SKILL.md").exists());
    assert!(
        fs::read_dir(authority.staging().root())
            .unwrap()
            .all(|entry| !entry.unwrap().file_type().unwrap().is_dir())
    );
}

#[test]
fn existing_different_package_with_same_canonical_name_blocks_install() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let alias = authority.project_skills().root().join("alias");
    fs::create_dir(&alias).unwrap();
    fs::write(
        alias.join("SKILL.md"),
        "---\nname: demo\ndescription: existing alias\n---\nbody",
    )
    .unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer =
        SkillInstaller::with_fetcher(&authority, catalog, Arc::new(SameFetcher)).unwrap();

    let result = installer.install(&SkillInstallInput {
        repository: "https://github.com/example/demo.git".to_string(),
        selector: None,
        revision: None,
        scope: SkillScope::Project,
    });

    assert!(matches!(
        result,
        Err(skill_store::SkillInstallError::Collision)
    ));
    assert!(!authority.project_skills().root().join("demo").exists());
}

#[test]
fn command_created_canonical_collision_during_fetch_is_revalidated() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let installer = SkillInstaller::with_fetcher(
        &authority,
        catalog,
        Arc::new(BlockingFetcher {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
    )
    .unwrap();
    let thread = std::thread::spawn(move || {
        installer.install(&SkillInstallInput {
            repository: "https://github.com/example/demo.git".to_string(),
            selector: None,
            revision: None,
            scope: SkillScope::Project,
        })
    });

    entered.wait();
    let alias = authority
        .project_skills()
        .root()
        .join("created-during-fetch");
    fs::create_dir(&alias).unwrap();
    fs::write(
        alias.join("SKILL.md"),
        "---\nname: demo\ndescription: concurrent alias\n---\nbody",
    )
    .unwrap();
    release.wait();

    assert!(matches!(
        thread.join().unwrap(),
        Err(skill_store::SkillInstallError::Collision)
    ));
    assert!(!authority.project_skills().root().join("demo").exists());
}

#[test]
fn replaced_project_root_is_rejected_before_commit() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer =
        SkillInstaller::with_fetcher(&authority, catalog, Arc::new(SameFetcher)).unwrap();
    let original = authority.project_skills().root();
    let renamed = workspace.join(".agents/skills-before-replacement");
    fs::rename(original, &renamed).unwrap();
    fs::create_dir(original).unwrap();

    let result = installer.install(&SkillInstallInput {
        repository: "https://github.com/example/demo.git".to_string(),
        selector: None,
        revision: None,
        scope: SkillScope::Project,
    });

    assert!(matches!(
        result,
        Err(skill_store::SkillInstallError::CommitFailed)
    ));
    assert!(!original.join("demo").exists());
    assert!(!renamed.join("demo").exists());
    assert!(
        fs::read_dir(authority.staging().root())
            .unwrap()
            .all(|entry| !entry.unwrap().file_type().unwrap().is_dir())
    );
}

#[test]
fn global_collision_cleans_staging_and_preserves_both_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let global = fixture.path().join("global");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    let destination = authority.global_skills().root().join("demo");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), "unchanged").unwrap();
    let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
    let installer =
        SkillInstaller::with_fetcher(&authority, catalog, Arc::new(SameFetcher)).unwrap();

    let result = installer.install(&SkillInstallInput {
        repository: "https://github.com/example/demo.git".to_string(),
        selector: None,
        revision: None,
        scope: SkillScope::Global,
    });

    assert!(matches!(
        result,
        Err(skill_store::SkillInstallError::Collision)
    ));
    assert_eq!(
        fs::read_to_string(destination.join("sentinel")).unwrap(),
        "unchanged"
    );
    assert!(!destination.join("SKILL.md").exists());
    assert!(
        fs::read_dir(authority.global_staging().root())
            .unwrap()
            .all(|entry| !entry.unwrap().file_type().unwrap().is_dir())
    );
    assert!(
        fs::read_dir(authority.project_skills().root())
            .unwrap()
            .next()
            .is_none()
    );
}
