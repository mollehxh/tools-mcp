use xtask::upstream::{
    read_required_file, verify_bytes_hash, verify_contract_delta_coverage, verify_crate_imports,
    verify_requirement_ids, verify_rmcp_versions, verify_root, verify_trusted_source_hash,
};

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
        "crates/skill-store/src/contracts.rs",
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
    let deltas = serde_json::json!({"version": 1, "baseline": "fixture.json", "deltas": []});
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
