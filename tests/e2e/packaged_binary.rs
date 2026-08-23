use mcp_agent_authority::release::{
    RELEASE_MANIFEST_FILE, ReleaseArtifactKind, ReleaseError, ReleaseManifest,
    current_release_target, verify_release, verify_release_assets,
};
use std::fs;
use xtask::package::{PackageOptions, assemble, ensure_supported_os};

const INSTALLER_FILES: &[&str] = &[
    "SKILL.md",
    "LICENSE.txt",
    "agents/openai.yaml",
    "assets/skill-installer-small.svg",
    "assets/skill-installer.png",
    "scripts/github_utils.py",
    "scripts/install-skill-from-github.py",
];

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let repository = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let source_repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for relative in [
        "third_party/openai-codex/LICENSE",
        "third_party/openai-codex/NOTICE",
        "THIRD_PARTY_NOTICES.md",
    ] {
        let path = repository.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source_repository.join(relative), path).unwrap();
    }
    for relative in INSTALLER_FILES {
        let source = source_repository
            .join("third_party/openai-codex/skill-installer")
            .join(relative);
        let destination = repository
            .path()
            .join("third_party/openai-codex/skill-installer")
            .join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(source, destination).unwrap();
    }
    let binary = repository.path().join("mcp-agent");
    fs::write(&binary, b"fixture executable").unwrap();
    (repository, output, binary)
}

fn options(
    repository: &tempfile::TempDir,
    output: &tempfile::TempDir,
    binary: std::path::PathBuf,
) -> PackageOptions {
    PackageOptions {
        repository_root: repository.path().to_path_buf(),
        binary_path: binary,
        output_root: output.path().join("output with spaces"),
        source_commit: "0123456789abcdef".to_owned(),
        source_tree_state: "dirty".to_owned(),
        version: "0.1.0".to_owned(),
        target: current_release_target().unwrap().to_owned(),
    }
}

#[test]
fn package_is_macos_only_until_native_backends_are_delivered() {
    ensure_supported_os("macos").unwrap();
    for unsupported in ["linux", "windows"] {
        let error = ensure_supported_os(unsupported).unwrap_err();
        assert!(error.to_string().contains("macOS-only"));
        assert!(error.to_string().contains("deferred"));
    }
}

#[test]
fn assembles_an_idempotent_release_with_manifest_notices_and_checksums() {
    let (repository, output, binary) = fixture();
    let options = options(&repository, &output, binary);

    let first = assemble(&options).unwrap();
    let first_archive = fs::read(&first.archive).unwrap();
    let first_manifest = fs::read(first.release_dir.join(RELEASE_MANIFEST_FILE)).unwrap();
    let second = assemble(&options).unwrap();

    assert_eq!(first.release_dir, second.release_dir);
    assert_eq!(first_archive, fs::read(&second.archive).unwrap());
    assert_eq!(
        first_manifest,
        fs::read(second.release_dir.join(RELEASE_MANIFEST_FILE)).unwrap()
    );
    assert!(first.release_dir.join("mcp-agent").is_file());
    assert!(first.release_dir.join("sandbox-manifest.json").is_file());
    assert!(
        first
            .release_dir
            .join("sandbox/macos-seatbelt.marker")
            .is_file()
    );
    assert!(first.release_dir.join("sandbox/preflight-canary").is_file());
    for relative in INSTALLER_FILES {
        assert!(
            first
                .release_dir
                .join("system-skills/skill-installer")
                .join(relative)
                .is_file(),
            "missing packaged installer file {relative}"
        );
    }
    assert!(
        first
            .release_dir
            .join("sandbox/workspace-write.policy")
            .is_file()
    );
    for notice in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"] {
        assert!(first.release_dir.join(notice).is_file());
    }

    let manifest = verify_release(
        &first.release_dir,
        &first.release_dir.join("mcp-agent"),
        "0.1.0",
    )
    .unwrap();
    assert_eq!(manifest.target, current_release_target().unwrap());
    assert_eq!(manifest.source_commit, "0123456789abcdef");
    assert_eq!(manifest.source_tree_state, "dirty");
    assert_eq!(manifest.supported_os, ["macos"]);
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.path == "mcp-agent")
    );
    assert!(manifest.artifacts.iter().any(|artifact| {
        artifact.path == "system-skills/skill-installer/scripts/install-skill-from-github.py"
            && artifact.mode == 0o755
            && artifact.kind == ReleaseArtifactKind::SystemSkill
    }));
    assert!(manifest.artifacts.iter().any(|artifact| {
        artifact.path == "sandbox/preflight-canary"
            && artifact.mode == 0o644
            && artifact.kind == ReleaseArtifactKind::PreflightCanary
    }));

    let sums = fs::read_to_string(first.release_dir.join("SHA256SUMS")).unwrap();
    assert!(sums.contains("  mcp-agent\n"));
    assert!(sums.contains("  release-manifest.json\n"));
    assert!(sums.contains("  sandbox/preflight-canary\n"));
    assert!(
        sums.contains("  system-skills/skill-installer/scripts/install-skill-from-github.py\n")
    );
}

#[test]
fn configured_release_assets_receive_full_verification_and_revalidation() {
    let (repository, output, binary) = fixture();
    let result = assemble(&options(&repository, &output, binary)).unwrap();

    verify_release_assets(&result.release_dir, "0.1.0").unwrap();
    fs::write(
        result
            .release_dir
            .join("system-skills/skill-installer/scripts/github_utils.py"),
        b"mutated after startup",
    )
    .unwrap();
    assert!(matches!(
        verify_release_assets(&result.release_dir, "0.1.0"),
        Err(ReleaseError::ArtifactMismatch)
    ));
}

#[test]
fn release_verification_rejects_extra_missing_modified_and_mode_changed_nested_assets() {
    let (repository, output, binary) = fixture();
    let options = options(&repository, &output, binary);

    let result = assemble(&options).unwrap();
    fs::write(
        result
            .release_dir
            .join("system-skills/skill-installer/scripts/unexpected.py"),
        b"unexpected",
    )
    .unwrap();
    assert!(matches!(
        verify_release_assets(&result.release_dir, "0.1.0"),
        Err(ReleaseError::ArtifactMismatch)
    ));

    let result = assemble(&options).unwrap();
    fs::remove_file(
        result
            .release_dir
            .join("system-skills/skill-installer/assets/skill-installer.png"),
    )
    .unwrap();
    assert!(matches!(
        verify_release_assets(&result.release_dir, "0.1.0"),
        Err(ReleaseError::ArtifactMismatch)
    ));

    let result = assemble(&options).unwrap();
    fs::write(
        result.release_dir.join("sandbox/preflight-canary"),
        b"changed",
    )
    .unwrap();
    assert!(matches!(
        verify_release_assets(&result.release_dir, "0.1.0"),
        Err(ReleaseError::ArtifactMismatch)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let result = assemble(&options).unwrap();
        let installer = result
            .release_dir
            .join("system-skills/skill-installer/scripts/install-skill-from-github.py");
        fs::set_permissions(&installer, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            verify_release_assets(&result.release_dir, "0.1.0"),
            Err(ReleaseError::ArtifactMismatch)
        ));
    }
}

#[test]
fn release_verification_rejects_a_swapped_binary() {
    let (repository, output, binary) = fixture();
    let result = assemble(&options(&repository, &output, binary)).unwrap();
    let installed_binary = result.release_dir.join("mcp-agent");
    fs::write(&installed_binary, b"replacement").unwrap();

    assert!(matches!(
        verify_release(&result.release_dir, &installed_binary, "0.1.0"),
        Err(ReleaseError::ArtifactMismatch)
    ));
}

#[test]
fn release_verification_rejects_a_tampered_artifact_kind() {
    let (repository, output, binary) = fixture();
    let result = assemble(&options(&repository, &output, binary)).unwrap();
    let manifest_path = result.release_dir.join(RELEASE_MANIFEST_FILE);
    let mut manifest: ReleaseManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.artifacts[0].kind = ReleaseArtifactKind::Executable;
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        verify_release(
            &result.release_dir,
            &result.release_dir.join("mcp-agent"),
            "0.1.0"
        ),
        Err(ReleaseError::ArtifactMismatch)
    ));
}
