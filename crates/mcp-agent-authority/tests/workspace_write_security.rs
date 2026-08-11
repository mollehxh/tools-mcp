use mcp_agent_authority::{
    AuthorityError, ManagedWriteScope, ServerOperations, WorkspaceAuthority,
};
use std::fs;
use std::path::Path;

#[path = "../../../tests/conformance/workspace_write.rs"]
mod conformance;

#[test]
fn workspace_root_is_canonical_and_immutable() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let authority = WorkspaceAuthority::new(workspace.join(".")).unwrap();

    assert!(authority.workspace_root().is_absolute());
    assert_eq!(
        authority.workspace_root(),
        workspace.canonicalize().unwrap()
    );
    assert_eq!(
        authority.command().workspace_root(),
        authority.workspace_root()
    );
}

#[cfg(unix)]
#[test]
fn authority_rejects_symlinked_managed_root_before_opening_it() {
    let fixture = conformance::Fixture::new();
    let project_skills = fixture.workspace.join(".agents/skills");
    fs::remove_dir_all(&project_skills).unwrap();
    std::os::unix::fs::symlink(&fixture.outside, &project_skills).unwrap();

    assert!(matches!(
        WorkspaceAuthority::with_global_skills(&fixture.workspace, fixture.outside.join("global")),
        Err(AuthorityError::Setup(_))
    ));
}

#[cfg(unix)]
#[test]
fn authority_rejects_symlinked_global_root_before_opening_it() {
    let fixture = conformance::Fixture::new();
    let global_link = fixture.outside.join("global-link");
    std::os::unix::fs::symlink(&fixture.outside, &global_link).unwrap();

    assert!(matches!(
        WorkspaceAuthority::with_global_skills(&fixture.workspace, global_link),
        Err(AuthorityError::Setup(_))
    ));
}

#[cfg(unix)]
#[test]
fn global_root_setup_race_never_grants_an_outside_handle() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = conformance::Fixture::new();
    let workspace = fixture.workspace.canonicalize().unwrap();
    let global_root = workspace.join("global-root");
    let sentinel = fixture.outside.join("global-root-race-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let toggler_stop = Arc::clone(&stop);
    let outside = fixture.outside.clone();
    let toggled_root = global_root.clone();
    let toggler = std::thread::spawn(move || {
        while !toggler_stop.load(Ordering::Relaxed) {
            let _ = fs::remove_dir_all(&toggled_root);
            let _ = fs::remove_file(&toggled_root);
            let _ = std::os::unix::fs::symlink(&outside, &toggled_root);
            let _ = fs::remove_file(&toggled_root);
            let _ = fs::create_dir(&toggled_root);
        }
    });

    for _ in 0..200 {
        if let Ok(authority) = WorkspaceAuthority::with_global_skills(&workspace, &global_root) {
            let operations = ServerOperations::new(authority.global_skills()).unwrap();
            let _ = operations.atomic_write(Path::new("global-root-race-sentinel"), b"changed");
        }
    }
    stop.store(true, Ordering::Relaxed);
    toggler.join().unwrap();

    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}

#[cfg(unix)]
#[test]
fn managed_root_setup_race_never_grants_an_outside_handle() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = conformance::Fixture::new();
    let skills = fixture.workspace.join(".agents/skills");
    let sentinel = fixture.outside.join("root-race-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let toggler_stop = Arc::clone(&stop);
    let outside = fixture.outside.clone();
    let toggler = std::thread::spawn(move || {
        while !toggler_stop.load(Ordering::Relaxed) {
            let _ = fs::remove_dir_all(&skills);
            let _ = fs::remove_file(&skills);
            let _ = std::os::unix::fs::symlink(&outside, &skills);
            let _ = fs::remove_file(&skills);
            let _ = fs::create_dir(&skills);
        }
    });

    for _ in 0..200 {
        if let Ok(authority) = WorkspaceAuthority::with_global_skills(
            &fixture.workspace,
            fixture.outside.join("global"),
        ) {
            let operations = ServerOperations::new(authority.project_skills()).unwrap();
            let _ = operations.atomic_write(Path::new("root-race-sentinel"), b"changed");
        }
    }
    stop.store(true, Ordering::Relaxed);
    toggler.join().unwrap();

    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}

#[test]
fn cwd_rejects_relative_absolute_and_symlink_escape() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();

    assert!(authority.command().resolve_cwd(Path::new("src")).is_ok());
    assert!(matches!(
        authority.command().resolve_cwd(Path::new("../outside")),
        Err(AuthorityError::OutsideWorkspace)
    ));
    assert!(matches!(
        authority.command().resolve_cwd(&fixture.outside),
        Err(AuthorityError::OutsideWorkspace)
    ));

    conformance::make_dir_symlink(&fixture.outside, &fixture.workspace.join("escape"));
    assert!(matches!(
        authority.command().resolve_cwd(Path::new("escape")),
        Err(AuthorityError::OutsideWorkspace)
    ));
}

#[test]
fn managed_writes_allow_project_skills_but_protect_authority_roots() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let command = authority.command();

    assert!(command.authorize_write(Path::new("src/new.rs")).is_ok());
    assert!(
        command
            .authorize_write(Path::new(".agents/skills/example/SKILL.md"))
            .is_ok()
    );

    for protected in [
        ".git/config",
        ".codex/state.json",
        ".mcp-agent/state",
        ".mcp-agent/staging/install",
    ] {
        assert!(matches!(
            command.authorize_write(Path::new(protected)),
            Err(AuthorityError::ProtectedRoot)
        ));
    }
}

#[test]
fn server_operations_are_handle_relative_no_follow_and_atomic() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let operations = ServerOperations::new(authority.project_skills()).unwrap();

    operations
        .atomic_write(Path::new("demo/SKILL.md"), b"---\nname: demo\n---\n")
        .unwrap();
    assert_eq!(
        fs::read(fixture.workspace.join(".agents/skills/demo/SKILL.md")).unwrap(),
        b"---\nname: demo\n---\n"
    );

    conformance::make_dir_symlink(
        &fixture.outside,
        &fixture.workspace.join(".agents/skills/escape"),
    );
    let result = operations.atomic_write(Path::new("escape/pwned"), b"no");
    assert!(result.is_err());
    assert!(!fixture.outside.join("pwned").exists());
}

#[test]
fn global_and_staging_capabilities_are_not_command_writable() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    assert_eq!(
        authority.global_skills().scope(),
        ManagedWriteScope::GlobalSkills
    );
    assert_eq!(
        authority.staging().scope(),
        ManagedWriteScope::ServerStaging
    );
    assert!(
        authority
            .global_skills()
            .root()
            .starts_with(fixture.outside.canonicalize().unwrap())
    );
    assert!(
        authority
            .staging()
            .root()
            .starts_with(fixture.workspace.canonicalize().unwrap())
    );
    assert!(matches!(
        authority
            .command()
            .authorize_write(authority.global_skills().root()),
        Err(AuthorityError::OutsideWorkspace)
    ));
    assert!(matches!(
        authority
            .command()
            .authorize_write(authority.staging().root()),
        Err(AuthorityError::ProtectedRoot)
    ));
}

#[test]
fn static_symlink_replacement_never_changes_outside_sentinel() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let operations = ServerOperations::new(authority.project_skills()).unwrap();
    let sentinel = fixture.outside.join("sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    conformance::make_dir_symlink(
        &fixture.outside,
        &fixture.workspace.join(".agents/skills/replaced"),
    );

    assert!(
        operations
            .atomic_write(Path::new("replaced/sentinel"), b"changed")
            .is_err()
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}

#[test]
fn managed_operations_reject_traversal_and_absolute_paths() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let operations = ServerOperations::new(authority.project_skills()).unwrap();

    assert!(
        operations
            .atomic_write(Path::new("../outside/pwned"), b"no")
            .is_err()
    );
    assert!(
        operations
            .atomic_write(&fixture.outside.join("pwned"), b"no")
            .is_err()
    );
    assert!(!fixture.outside.join("pwned").exists());
}

#[cfg(unix)]
#[test]
fn atomic_replacement_does_not_mutate_an_external_hardlink() {
    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let operations = ServerOperations::new(authority.project_skills()).unwrap();
    let sentinel = fixture.outside.join("hardlink-sentinel");
    let destination = fixture.workspace.join(".agents/skills/hardlink/SKILL.md");
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(&sentinel, b"unchanged").unwrap();
    fs::hard_link(&sentinel, &destination).unwrap();

    operations
        .atomic_write(Path::new("hardlink/SKILL.md"), b"replacement")
        .unwrap();

    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(fs::read(&destination).unwrap(), b"replacement");
}

#[cfg(unix)]
#[test]
fn racing_symlink_replacement_cannot_change_outside_content() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = conformance::Fixture::new();
    let authority = fixture.authority();
    let operations = ServerOperations::new(authority.project_skills()).unwrap();
    let sentinel = fixture.outside.join("race-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    let race = fixture.workspace.join(".agents/skills/race");
    fs::create_dir_all(&race).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let toggler_stop = Arc::clone(&stop);
    let outside = fixture.outside.clone();
    let toggler = std::thread::spawn(move || {
        while !toggler_stop.load(Ordering::Relaxed) {
            let _ = fs::remove_dir(&race);
            let _ = std::os::unix::fs::symlink(&outside, &race);
            let _ = fs::remove_file(&race);
            let _ = fs::create_dir(&race);
        }
    });

    for _ in 0..200 {
        let _ = operations.atomic_write(Path::new("race/race-sentinel"), b"changed");
    }
    stop.store(true, Ordering::Relaxed);
    toggler.join().unwrap();

    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}
