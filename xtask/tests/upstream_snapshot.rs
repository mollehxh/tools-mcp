use xtask::upstream::{read_required_file, verify_bytes_hash, verify_crate_imports};

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
