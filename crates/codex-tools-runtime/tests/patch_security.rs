use codex_tools_runtime::contracts::ApplyPatchInput;
use codex_tools_runtime::patch::apply_patch;
use mcp_agent_authority::WorkspaceAuthority;
use std::fs;
use std::path::Path;

fn fixture() -> (tempfile::TempDir, WorkspaceAuthority) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let global = root.path().join("global-skills");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    (root, authority)
}

fn apply(authority: &WorkspaceAuthority, patch: String) -> String {
    apply_patch(authority, &ApplyPatchInput { patch })
        .unwrap_err()
        .to_string()
}

#[cfg(windows)]
fn make_dir_reparse_point(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap_or_else(|error| {
        panic!(
            "failed to create Windows directory reparse point {} -> {}: {error}",
            link.display(),
            target.display()
        )
    });
}

#[test]
fn rejects_absolute_traversal_and_protected_paths_before_any_change() {
    let (root, authority) = fixture();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "unchanged\n").unwrap();

    for unsafe_path in [
        outside.display().to_string(),
        "../outside.txt".to_string(),
        ".git/config".to_string(),
        ".codex/state".to_string(),
        ".mcp-agent/staging/file".to_string(),
    ] {
        let safe = authority.workspace_root().join("must-not-exist.txt");
        let error = apply(
            &authority,
            format!(
                "*** Begin Patch\n*** Add File: must-not-exist.txt\n+unsafe\n*** Add File: {unsafe_path}\n+changed\n*** End Patch"
            ),
        );
        assert!(
            error.contains("outside the fixed workspace")
                || error.contains("protected authority root"),
            "unexpected policy error: {error}"
        );
        assert!(!safe.exists(), "preflight failure applied an earlier hunk");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged\n");
    }
}

#[cfg(unix)]
#[test]
fn rejects_static_source_and_destination_symlink_escapes() {
    let (root, authority) = fixture();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel.txt"), "unchanged\n").unwrap();
    std::os::unix::fs::symlink(&outside, authority.workspace_root().join("escape")).unwrap();
    fs::write(authority.workspace_root().join("source.txt"), "old\n").unwrap();

    let add_error = apply(
        &authority,
        "*** Begin Patch\n*** Add File: escape/sentinel.txt\n+changed\n*** End Patch".to_string(),
    );
    assert!(add_error.contains("outside the fixed workspace"));

    let move_error = apply(
        &authority,
        concat!(
            "*** Begin Patch\n",
            "*** Update File: source.txt\n",
            "*** Move to: escape/sentinel.txt\n",
            "@@\n",
            "-old\n",
            "+new\n",
            "*** End Patch"
        )
        .to_string(),
    );
    assert!(move_error.contains("outside the fixed workspace"));
    assert_eq!(
        fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "unchanged\n"
    );
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join("source.txt")).unwrap(),
        "old\n"
    );
}

#[cfg(windows)]
#[test]
fn rejects_static_source_and_destination_reparse_point_escapes() {
    let (root, authority) = fixture();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel.txt");
    fs::write(&sentinel, "unchanged\n").unwrap();
    make_dir_reparse_point(&outside, &authority.workspace_root().join("escape"));
    let source = authority.workspace_root().join("source.txt");
    fs::write(&source, "old\n").unwrap();

    let unsafe_patches = [
        "*** Begin Patch\n*** Add File: escape/created.txt\n+created\n*** End Patch",
        concat!(
            "*** Begin Patch\n",
            "*** Update File: escape/sentinel.txt\n",
            "@@\n",
            "-unchanged\n",
            "+changed\n",
            "*** End Patch"
        ),
        "*** Begin Patch\n*** Delete File: escape/sentinel.txt\n*** End Patch",
        concat!(
            "*** Begin Patch\n",
            "*** Update File: source.txt\n",
            "*** Move to: escape/moved.txt\n",
            "@@\n",
            "-old\n",
            "+new\n",
            "*** End Patch"
        ),
    ];

    for patch in unsafe_patches {
        let error = apply(&authority, patch.to_string());
        assert!(
            error.contains("outside the fixed workspace"),
            "unexpected reparse-point policy error: {error}"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged\n");
        assert!(!outside.join("created.txt").exists());
        assert!(!outside.join("moved.txt").exists());
        assert_eq!(fs::read_to_string(&source).unwrap(), "old\n");
    }
}

#[cfg(unix)]
#[test]
fn racing_directory_symlink_replacement_never_changes_outside_sentinel() {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (root, authority) = fixture();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel.txt");
    fs::write(&sentinel, "unchanged\n").unwrap();

    for index in 0..100 {
        let race_name = format!("race-{index}");
        let race = authority.workspace_root().join(&race_name);
        fs::create_dir(&race).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let toggler_stop = Arc::clone(&stop);
        let start = Arc::new(Barrier::new(2));
        let toggler_start = Arc::clone(&start);
        let outside_for_thread = outside.clone();
        let race_for_thread = race.clone();
        let toggler = std::thread::spawn(move || {
            toggler_start.wait();
            while !toggler_stop.load(Ordering::Relaxed) {
                let _ = fs::remove_dir(&race_for_thread);
                let _ = std::os::unix::fs::symlink(&outside_for_thread, &race_for_thread);
                let _ = fs::remove_file(&race_for_thread);
                let _ = fs::create_dir(&race_for_thread);
            }
        });
        start.wait();
        let _ = apply_patch(
            &authority,
            &ApplyPatchInput {
                patch: format!(
                    "*** Begin Patch\n*** Add File: {race_name}/file-{index}.txt\n+safe\n*** End Patch"
                ),
            },
        );
        stop.store(true, Ordering::Relaxed);
        toggler.join().unwrap();
    }

    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged\n");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn racing_directory_reparse_point_replacement_never_writes_outside() {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let (root, authority) = fixture();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel.txt");
    fs::write(&sentinel, "unchanged\n").unwrap();
    let successful_reparse_points = Arc::new(AtomicUsize::new(0));

    for index in 0..100 {
        let race_name = format!("race-{index}");
        let race = authority.workspace_root().join(&race_name);
        fs::create_dir(&race).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let toggler_stop = Arc::clone(&stop);
        let reparse_count = Arc::clone(&successful_reparse_points);
        let start = Arc::new(Barrier::new(2));
        let toggler_start = Arc::clone(&start);
        let outside_for_thread = outside.clone();
        let race_for_thread = race.clone();
        let toggler = std::thread::spawn(move || {
            toggler_start.wait();
            while !toggler_stop.load(Ordering::Relaxed) {
                if fs::remove_dir(&race_for_thread).is_ok()
                    && std::os::windows::fs::symlink_dir(&outside_for_thread, &race_for_thread)
                        .is_ok()
                {
                    reparse_count.fetch_add(1, Ordering::Relaxed);
                }
                let _ = fs::remove_dir(&race_for_thread);
                let _ = fs::create_dir(&race_for_thread);
            }
        });
        start.wait();
        let _ = apply_patch(
            &authority,
            &ApplyPatchInput {
                patch: format!(
                    "*** Begin Patch\n*** Add File: {race_name}/attempt-{index}.txt\n+safe\n*** End Patch"
                ),
            },
        );
        stop.store(true, Ordering::Relaxed);
        toggler.join().unwrap();
    }

    assert!(
        successful_reparse_points.load(Ordering::Relaxed) > 0,
        "the race fixture never installed a Windows directory reparse point"
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged\n");
    assert_eq!(
        fs::read_dir(&outside).unwrap().count(),
        1,
        "a racing patch write escaped into the outside directory"
    );
}

#[cfg(unix)]
#[test]
fn destination_that_is_an_existing_hardlink_is_atomically_replaced() {
    let (root, authority) = fixture();
    let sentinel = root.path().join("sentinel.txt");
    let destination = authority.workspace_root().join("destination.txt");
    fs::write(&sentinel, "unchanged\n").unwrap();
    fs::hard_link(&sentinel, &destination).unwrap();

    apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: "*** Begin Patch\n*** Add File: destination.txt\n+replacement\n*** End Patch"
                .to_string(),
        },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged\n");
    assert_eq!(fs::read_to_string(&destination).unwrap(), "replacement\n");
}

#[test]
fn patch_paths_are_workspace_relative_not_process_cwd_relative() {
    let (_root, authority) = fixture();
    apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: "*** Begin Patch\n*** Add File: fixed.txt\n+workspace\n*** End Patch"
                .to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join(Path::new("fixed.txt"))).unwrap(),
        "workspace\n"
    );
}
