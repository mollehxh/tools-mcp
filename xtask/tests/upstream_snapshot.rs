use xtask::upstream::{
    read_required_file, verify_bytes_hash, verify_contract_delta_coverage, verify_crate_imports,
    verify_requirement_ids, verify_rmcp_versions, verify_root, verify_trusted_source_hash,
};

const INSTALLER_ROOT: &str = "third_party/openai-codex/skill-installer";

fn collect_installer_files(
    root: &std::path::Path,
    current: &std::path::Path,
    actual: &mut std::collections::BTreeMap<String, (String, u32)>,
) {
    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    for entry in std::fs::read_dir(current).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_installer_files(root, &path, actual);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        #[cfg(unix)]
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        #[cfg(not(unix))]
        let mode = if relative == "scripts/install-skill-from-github.py" {
            0o755
        } else {
            0o644
        };
        actual.insert(relative, (digest, mode));
    }
}

#[test]
fn installer_payload_matches_the_pinned_upstream_snapshot() {
    use std::collections::BTreeMap;

    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let expected = BTreeMap::from([
        (
            "LICENSE.txt",
            (
                "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
                0o644,
            ),
        ),
        (
            "SKILL.md",
            (
                // The local digest is intentionally distinct from the pinned upstream
                // digest and is independently registered after the instruction-only adaptation.
                "72402ab63f95e7a0ee11ebffc0cf32015fbce4c72422d0fe6b290eabea42f506",
                0o644,
            ),
        ),
        (
            "agents/openai.yaml",
            (
                "5ce223d8b1070b82c42298538f1b8d376f788eb9e7a42a987e8c094070d73f0e",
                0o644,
            ),
        ),
        (
            "assets/skill-installer-small.svg",
            (
                "3928703ff00dc1a681e7a22401843b7edcbd4b2051651ce4c43b75f7e140504e",
                0o644,
            ),
        ),
        (
            "assets/skill-installer.png",
            (
                "d0a230b1a79b71b858b7c215a0fbb0768d6459c14ea4ef80c61592629bf0e605",
                0o644,
            ),
        ),
        (
            "scripts/github_utils.py",
            (
                "61c1bbe2ae217433b4b6f9f09f21aca4df52c12598068343ade719f706e4859b",
                0o644,
            ),
        ),
        (
            "scripts/install-skill-from-github.py",
            (
                "0fbbd36e8ea294442c0bd48d6f610a2e8656216bfef5c322f1dcf448ef2f09f1",
                0o755,
            ),
        ),
    ])
    .into_iter()
    .map(|(path, (digest, mode))| (path.to_owned(), (digest.to_owned(), mode)))
    .collect::<BTreeMap<_, _>>();
    let root = repository.join(INSTALLER_ROOT);
    assert!(root.is_dir(), "pinned installer payload is missing");

    let mut actual = BTreeMap::new();
    collect_installer_files(&root, &root, &mut actual);

    assert_eq!(actual, expected);
    assert!(!root.join("scripts/list-skills.py").exists());
}

fn copy_file(source_root: &std::path::Path, target_root: &std::path::Path, relative: &str) {
    let target = target_root.join(relative);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(source_root.join(relative), target).unwrap();
}

fn copy_tree(source_root: &std::path::Path, target_root: &std::path::Path, relative: &str) {
    for entry in std::fs::read_dir(source_root.join(relative)).unwrap() {
        let entry = entry.unwrap();
        let child = entry.path();
        let child_relative = child.strip_prefix(source_root).unwrap();
        if child.is_dir() {
            copy_tree(source_root, target_root, child_relative.to_str().unwrap());
        } else {
            copy_file(source_root, target_root, child_relative.to_str().unwrap());
        }
    }
}

fn audit_fixture() -> tempfile::TempDir {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let fixture = tempfile::tempdir().unwrap();
    copy_file(source_root, fixture.path(), "Cargo.lock");
    for relative in [
        "crates/codex-tools-runtime/src/lib.rs",
        "crates/codex-tools-runtime/src/apply_patch_parser_boundary.rs",
        "crates/codex-tools-runtime/src/contracts/apply_patch.rs",
        "crates/codex-tools-runtime/src/contracts/mod.rs",
        "crates/codex-tools-runtime/src/contracts/exec_command.rs",
        "crates/codex-tools-runtime/src/contracts/write_stdin.rs",
        "crates/codex-tools-runtime/src/patch/adapter.rs",
        "crates/codex-tools-runtime/src/patch/mod.rs",
        "crates/codex-tools-runtime/src/process/manager.rs",
        "crates/skill-store/src/catalog.rs",
        "crates/skill-store/src/contracts.rs",
        "crates/skill-store/src/cursor.rs",
        "crates/skill-store/src/precedence.rs",
        "crates/skill-store/src/resource.rs",
        "crates/skill-store/src/roots.rs",
    ] {
        copy_file(source_root, fixture.path(), relative);
    }
    for relative in [
        "third_party/openai-codex",
        "crates/codex-tools-runtime/src/upstream",
        "crates/skill-store/src/upstream",
        "tests/conformance/fixtures",
    ] {
        copy_tree(source_root, fixture.path(), relative);
    }
    fixture
}

fn mutate_boundary_manifest(
    fixture: &std::path::Path,
    symbol: &str,
    mutate: impl FnOnce(&mut toml::map::Map<String, toml::Value>),
) {
    let path = fixture.join("third_party/openai-codex/SOURCE.toml");
    let mut manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let boundary = manifest["boundaries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find_map(|boundary| {
            let table = boundary.as_table_mut().unwrap();
            (table["symbol"].as_str() == Some(symbol)).then_some(table)
        })
        .unwrap();
    mutate(boundary);
    std::fs::write(path, toml::to_string(&manifest).unwrap()).unwrap();
}

#[test]
fn rejects_unmapped_local_import() {
    let error = verify_crate_imports(
        "use crate::local_adapter::Thing;",
        &["crate::unified_exec"],
        "fixture.rs",
    )
    .unwrap_err();
    assert!(error.to_string().contains("unmapped local import"));
}

#[test]
fn accepts_registered_boundary_import() {
    verify_crate_imports(
        "use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;",
        &["crate::unified_exec"],
        "fixture.rs",
    )
    .unwrap();
}

#[test]
fn rejects_near_prefix_boundary_import() {
    let error = verify_crate_imports(
        "use crate::unified_exec_evil::Thing;",
        &["crate::unified_exec"],
        "fixture.rs",
    )
    .unwrap_err();
    assert!(error.to_string().contains("unmapped local import"));
}

#[test]
fn rejects_wrong_sha_or_modified_unchanged_file() {
    let error = verify_bytes_hash(b"modified", &"0".repeat(64), "unchanged.rs").unwrap_err();
    assert!(error.to_string().contains("SHA-256 mismatch"));
}

#[test]
fn rejects_missing_notice() {
    let directory = tempfile::tempdir().unwrap();
    let error = read_required_file(&directory.path().join("NOTICE")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required provenance file is missing")
    );
}

#[test]
fn repository_audit_runs_end_to_end_and_rejects_notice_mutation() {
    let fixture = audit_fixture();
    verify_root(fixture.path()).unwrap();

    std::fs::write(
        fixture.path().join("third_party/openai-codex/NOTICE"),
        "substituted notice",
    )
    .unwrap();
    let error = verify_root(fixture.path()).unwrap_err();
    assert!(error.to_string().contains("notice verification"));
}

#[cfg(unix)]
#[test]
fn repository_audit_rejects_installer_mode_drift() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = audit_fixture();
    let installer = fixture
        .path()
        .join(INSTALLER_ROOT)
        .join("scripts/install-skill-from-github.py");
    std::fs::set_permissions(installer, std::fs::Permissions::from_mode(0o644)).unwrap();

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(error.to_string().contains("mode mismatch"));
}

#[test]
fn repository_audit_rejects_omitted_listing_script_reintroduction() {
    let fixture = audit_fixture();
    let listing = fixture
        .path()
        .join(INSTALLER_ROOT)
        .join("scripts/list-skills.py");
    std::fs::write(listing, b"unsupported catalog behavior").unwrap();

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(error.to_string().contains("untracked audited source"));
}

#[test]
fn rejects_adapter_boundary_with_removed_provenance() {
    let fixture = audit_fixture();
    mutate_boundary_manifest(fixture.path(), "crate::patch::adapter", |boundary| {
        boundary.remove("sources");
        boundary.remove("local_sha256");
    });

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("lacks sources and a pinned provenance exemption")
    );
}

#[test]
fn rejects_unpinned_boundary_provenance_exemption() {
    let fixture = audit_fixture();
    mutate_boundary_manifest(fixture.path(), "crate::patch::adapter", |boundary| {
        boundary.remove("sources");
        boundary.remove("local_sha256");
        boundary.insert(
            "provenance_exemption".to_string(),
            toml::Value::String("newly declared exemption".to_string()),
        );
    });

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(error.to_string().contains("is not eligible"));
}

#[test]
fn rejects_modified_pinned_boundary_provenance_exemption() {
    let fixture = audit_fixture();
    mutate_boundary_manifest(fixture.path(), "crate::unified_exec", |boundary| {
        boundary.insert(
            "provenance_exemption".to_string(),
            toml::Value::String("different rationale".to_string()),
        );
    });

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("disagrees with the pinned rationale")
    );
}

#[test]
fn rejects_empty_pinned_boundary_provenance_exemption() {
    let fixture = audit_fixture();
    mutate_boundary_manifest(fixture.path(), "crate::unified_exec", |boundary| {
        boundary.insert(
            "provenance_exemption".to_string(),
            toml::Value::String(String::new()),
        );
    });

    let error = verify_root(fixture.path()).unwrap_err();
    assert!(error.to_string().contains("empty provenance exemption"));
}

#[test]
fn rejects_manifest_hash_that_disagrees_with_independent_trust_root() {
    let error = verify_trusted_source_hash(
        "codex-rs/core/src/unified_exec/head_tail_buffer.rs",
        &"f".repeat(64),
    )
    .unwrap_err();
    assert!(error.to_string().contains("independent pinned digest"));
}

#[test]
fn rejects_coordinated_unchanged_file_and_manifest_substitution() {
    use sha2::{Digest, Sha256};

    let substituted = b"coordinated replacement";
    let substituted_hash = format!("{:x}", Sha256::digest(substituted));
    verify_bytes_hash(substituted, &substituted_hash, "substituted.rs").unwrap();
    let error = verify_trusted_source_hash(
        "codex-rs/core/src/unified_exec/head_tail_buffer.rs",
        &substituted_hash,
    )
    .unwrap_err();
    assert!(error.to_string().contains("independent pinned digest"));
}

#[test]
fn rejects_unknown_or_duplicate_requirement_ids() {
    assert!(verify_requirement_ids(&["R23".to_string()], "fixture").is_err());
    assert!(verify_requirement_ids(&["garbage".to_string()], "fixture").is_err());
    assert!(verify_requirement_ids(&["R3".to_string(), "R3".to_string()], "fixture").is_err());
}

#[test]
fn rejects_missing_or_multiple_rmcp_versions() {
    verify_rmcp_versions(&["3.0.1".to_string()]).unwrap();
    assert!(verify_rmcp_versions(&[]).is_err());
    assert!(verify_rmcp_versions(&["3.0.1".to_string(), "3.1.0".to_string()]).is_err());
    assert!(verify_rmcp_versions(&["3.1.0".to_string()]).is_err());
}

#[test]
fn rejects_unregistered_contract_difference() {
    let baseline = serde_json::json!([{"name": "exec_command", "description": "upstream"}]);
    let local = serde_json::json!([{"name": "exec_command", "description": "changed"}]);
    let deltas = serde_json::json!({"version": 2, "baseline": "fixture.json", "deltas": []});
    let error = verify_contract_delta_coverage(&baseline, &local, &deltas).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unregistered contract difference")
    );
}

#[test]
fn current_contract_delta_registry_exactly_covers_the_pinned_baseline() {
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/conformance/fixtures/pinned-upstream-tool-contracts.json"
    ))
    .unwrap();
    let local: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/conformance/fixtures/tool-contracts.json"
    ))
    .unwrap();
    let registry: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/conformance/fixtures/compatibility-deltas.json"
    ))
    .unwrap();

    verify_contract_delta_coverage(&baseline, &local, &registry).unwrap();
}
