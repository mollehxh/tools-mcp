use codex_tools_runtime::contracts::ApplyPatchInput;
use codex_tools_runtime::patch::{ApplyPatchError, apply_patch};
use mcp_agent_authority::WorkspaceAuthority;
use std::fs;

#[path = "../../../tests/conformance/apply_patch.rs"]
mod conformance;

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

#[test]
fn applies_add_update_delete_move_multiple_hunks_and_unicode() {
    let (_root, authority) = fixture();
    fs::write(
        authority.workspace_root().join("source.txt"),
        "alpha\nbeta\ngamma\ndelta\n",
    )
    .unwrap();
    fs::write(authority.workspace_root().join("delete.txt"), "obsolete\n").unwrap();

    let output = apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: conformance::representative_patch().to_string(),
        },
    )
    .unwrap();

    assert_eq!(
        output.output,
        concat!(
            "Success. Updated the following files:\n",
            "A nested/hello.txt\n",
            "M source.txt\n",
            "D delete.txt\n",
        )
    );
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join("nested/hello.txt")).unwrap(),
        "Привет, мир!\n"
    );
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join("moved.txt")).unwrap(),
        "alpha\nBETA\ngamma\nDELTA\n"
    );
    assert!(!authority.workspace_root().join("source.txt").exists());
    assert!(!authority.workspace_root().join("delete.txt").exists());
}

#[test]
fn preserves_pinned_default_line_ending_behavior_for_crlf_updates() {
    let (_root, authority) = fixture();
    fs::write(
        authority.workspace_root().join("windows.txt"),
        b"one\r\ntwo\r\n",
    )
    .unwrap();

    apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: concat!(
                "*** Begin Patch\n",
                "*** Update File: windows.txt\n",
                "@@\n",
                "-two\n",
                "+three\n",
                "*** End Patch"
            )
            .to_string(),
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(authority.workspace_root().join("windows.txt")).unwrap(),
        b"one\r\nthree\n"
    );
}

#[test]
fn reports_pinned_parse_and_missing_context_diagnostics() {
    let (_root, authority) = fixture();
    fs::write(authority.workspace_root().join("context.txt"), "actual\n").unwrap();

    let malformed = apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: "not a patch".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        malformed.to_string(),
        "Invalid patch: The first line of the patch must be '*** Begin Patch'"
    );
    assert!(matches!(malformed, ApplyPatchError::Parse(_)));

    let missing = apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: concat!(
                "*** Begin Patch\n",
                "*** Update File: context.txt\n",
                "@@\n",
                "-missing\n",
                "+replacement\n",
                "*** End Patch"
            )
            .to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Failed to find expected lines in context.txt:\nmissing"
    );
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join("context.txt")).unwrap(),
        "actual\n"
    );
}

#[test]
fn preserves_upstream_partial_patch_semantics_after_safe_preflight() {
    let (_root, authority) = fixture();
    fs::write(authority.workspace_root().join("later.txt"), "actual\n").unwrap();
    let error = apply_patch(
        &authority,
        &ApplyPatchInput {
            patch: concat!(
                "*** Begin Patch\n",
                "*** Add File: first.txt\n",
                "+committed\n",
                "*** Update File: later.txt\n",
                "@@\n",
                "-missing\n",
                "+replacement\n",
                "*** End Patch"
            )
            .to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("Failed to find expected lines"));
    assert_eq!(
        fs::read_to_string(authority.workspace_root().join("first.txt")).unwrap(),
        "committed\n"
    );
}
