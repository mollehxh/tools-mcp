use mcp_agent_authority::sandbox::{
    CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT, Sandbox, SandboxError, SandboxManifest,
    VerifiedSandbox, expected_manifest,
};
use sha2::Digest;
use std::fs;
use std::path::Path;
use std::process::Output;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("native sandbox conformance requires a supported platform backend");

#[path = "../../../tests/conformance/workspace_write.rs"]
mod conformance;

fn loaded_sandbox(fixture: &conformance::Fixture) -> Sandbox {
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    Sandbox::load(fixture.authority(), &release).unwrap()
}

fn installed_sandbox(fixture: &conformance::Fixture) -> VerifiedSandbox {
    let sentinel = fixture.outside.join("typestate-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    loaded_sandbox(fixture).preflight(&sentinel).unwrap().0
}

#[test]
fn manifest_carries_protocol_target_checksum_and_pinned_provenance() {
    let manifest = expected_manifest().unwrap();

    assert_eq!(manifest.capability_protocol, CAPABILITY_PROTOCOL);
    assert_eq!(manifest.upstream_commit, PINNED_CODEX_COMMIT);
    assert!(!manifest.target.is_empty());
    assert_eq!(manifest.artifact_sha256.len(), 64);
    assert_eq!(manifest.policy_sha256.len(), 64);
}

#[test]
fn missing_or_mismatched_manifest_fails_closed() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let authority = fixture.authority();
    assert!(matches!(
        Sandbox::load(authority.clone(), &release),
        Err(SandboxError::ManifestMissing)
    ));

    let mut manifest = expected_manifest().unwrap();
    manifest.capability_protocol = "wrong".to_owned();
    manifest.write_release_relative(&release).unwrap();
    assert!(matches!(
        Sandbox::load(authority, &release),
        Err(SandboxError::ProtocolMismatch { .. })
    ));
}

#[test]
fn manifest_replacement_after_load_fails_closed() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let sandbox = loaded_sandbox(&fixture);
    let manifest_path = SandboxManifest::release_path(&release);
    fs::write(&manifest_path, b"{}").unwrap();
    let sentinel = fixture.outside.join("sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();

    assert!(matches!(
        sandbox.preflight(&sentinel),
        Err(SandboxError::ArtifactReplaced)
    ));
}

#[test]
fn policy_or_helper_replacement_after_load_fails_closed() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    let sandbox = Sandbox::load(fixture.authority(), &release).unwrap();

    fs::write(release.join(&manifest.policy_path), b"replaced").unwrap();
    let sentinel = fixture.outside.join("sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    assert!(matches!(
        sandbox.preflight(&sentinel),
        Err(SandboxError::ArtifactReplaced)
    ));
}

#[test]
fn mutually_replaced_manifest_and_artifact_do_not_replace_build_trust() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let mut manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    let replacement = b"attacker-controlled-helper";
    fs::write(release.join(&manifest.artifact_path), replacement).unwrap();
    manifest.artifact_sha256 = format!("{:x}", sha2::Sha256::digest(replacement));
    fs::write(
        SandboxManifest::release_path(&release),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        Sandbox::load(fixture.authority(), &release),
        Err(SandboxError::ArtifactReplaced)
    ));
}

#[test]
fn missing_packaged_helper_fails_closed() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    fs::remove_file(release.join(&manifest.artifact_path)).unwrap();

    assert!(matches!(
        Sandbox::load(fixture.authority(), &release),
        Err(SandboxError::BackendMissing)
    ));
}

#[test]
fn helper_replacement_after_load_is_denied_or_detected() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    let helper = release.join(&manifest.artifact_path);
    let original = fs::read(&helper).unwrap();
    let sandbox = Sandbox::load(fixture.authority(), &release).unwrap();

    if fs::write(&helper, b"replaced-helper").is_ok() {
        let sentinel = fixture.outside.join("replacement-sentinel");
        fs::write(&sentinel, b"unchanged").unwrap();
        assert!(matches!(
            sandbox.preflight(&sentinel),
            Err(SandboxError::ArtifactReplaced)
        ));
    } else {
        assert_eq!(fs::read(&helper).unwrap(), original);
    }
}

#[test]
fn native_packaged_clean_install_preflight_proves_required_capabilities() {
    let fixture = conformance::Fixture::new();
    let outside_sentinel = fixture.outside.join("clean-install-sentinel");
    fs::write(&outside_sentinel, b"host-readable").unwrap();
    let (sandbox, receipt) = loaded_sandbox(&fixture)
        .preflight(&outside_sentinel)
        .expect("packaged native sandbox preflight must pass");

    assert!(receipt.outside_read_allowed);
    assert!(receipt.local_service_allowed);
    assert!(receipt.workspace_write_allowed);
    assert!(receipt.outside_write_denied);
    assert_eq!(sandbox.preflight_receipt(), &receipt);
    assert_eq!(fs::read(&outside_sentinel).unwrap(), b"host-readable");
}

#[test]
fn native_sandbox_denies_direct_outside_write() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let sentinel = fixture.outside.join("outside-write-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();

    let output = native_write_file(&sandbox, &sentinel);

    assert!(!output.status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}

#[test]
fn native_sandbox_denies_creation_of_absent_optional_protected_roots() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);

    // `.mcp-agent/staging` is a held authority root and is intentionally
    // materialized before sandbox launch, so only optional protected roots
    // can truthfully exercise the initially-absent case.
    for protected in [".git", ".codex"] {
        let protected_path = fixture.workspace.join(protected);
        fs::remove_dir_all(&protected_path).unwrap();
        assert!(!protected_path.exists());

        let output = native_replace_protected_root(&sandbox, &protected_path);

        assert!(!output.status.success());
        assert!(!protected_path.join("owned").exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_policy_is_deny_default_with_narrow_workspace_write() {
    let fixture = conformance::Fixture::new();
    let sandbox = loaded_sandbox(&fixture);
    let policy = sandbox.render_native_policy().unwrap();

    assert!(policy.contains("(deny default)"));
    assert!(policy.contains("(allow file-read*)"));
    assert!(policy.contains("(allow network-outbound)"));
    assert!(policy.contains("(allow file-write*"));
    assert!(policy.contains("(subpath (param \"WORKSPACE\"))"));
    for key in [
        "PROTECTED_GIT",
        "PROTECTED_CODEX",
        "PROTECTED_AGENT",
        "PROTECTED_STAGING",
    ] {
        assert!(policy.contains(&format!("(require-not (literal (param \"{key}\")))")));
        assert!(policy.contains(&format!("(require-not (subpath (param \"{key}\")))")));
    }
    assert!(!policy.contains("(allow default)"));
}

#[test]
fn native_child_process_inherits_outside_write_denial() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let sentinel = fixture.outside.join("child-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();

    let output = native_child_write_file(&sandbox, &sentinel);

    assert!(!output.status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
}

#[test]
fn native_sandbox_allows_workspace_but_denies_protected_roots() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let workspace_file = fixture.workspace.join("allowed");

    assert!(
        native_write_file(&sandbox, &workspace_file)
            .status
            .success()
    );
    assert_eq!(fs::read(&workspace_file).unwrap(), b"allowed");

    for protected in [".git", ".codex", ".mcp-agent"] {
        let protected_file = fixture.workspace.join(protected).join("denied");
        assert!(
            !native_write_file(&sandbox, &protected_file)
                .status
                .success()
        );
        assert!(!protected_file.exists());
    }
}

#[cfg(unix)]
fn native_write_file(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let script = format!("printf allowed > {}", shell_quote(path));
    sandbox
        .command("/bin/sh", &["-c", &script], Path::new("."))
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(windows)]
fn native_write_file(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let script = format!(
        "$ErrorActionPreference='Stop'; [System.IO.File]::WriteAllBytes('{}',[System.Text.Encoding]::UTF8.GetBytes('allowed'))",
        powershell_quote(path)
    );
    sandbox
        .command(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            Path::new("."),
        )
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(unix)]
fn native_child_write_file(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let path = path.to_string_lossy();
    sandbox
        .command(
            "/bin/sh",
            &[
                "-c",
                "/bin/sh -c 'printf changed > \"$1\"' child \"$1\"",
                "outer",
                &path,
            ],
            Path::new("."),
        )
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(windows)]
fn native_child_write_file(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let child = format!(
        "[System.IO.File]::WriteAllBytes('{}',[System.Text.Encoding]::UTF8.GetBytes('changed'))",
        powershell_quote(path)
    );
    let script = format!(
        "$child=\"{}\"; & powershell.exe -NoProfile -NonInteractive -Command $child; exit $LASTEXITCODE",
        child.replace('"', "`\"")
    );
    sandbox
        .command(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            Path::new("."),
        )
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(unix)]
fn native_replace_protected_root(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let owned = path.join("owned");
    let script = format!(
        "rmdir {} >/dev/null 2>&1 || true; mkdir -p {} >/dev/null 2>&1 || true; printf owned > {}",
        shell_quote(path),
        shell_quote(path),
        shell_quote(&owned)
    );
    sandbox
        .command("/bin/sh", &["-c", &script], Path::new("."))
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(windows)]
fn native_replace_protected_root(sandbox: &VerifiedSandbox, path: &Path) -> Output {
    let script = format!(
        "$ErrorActionPreference='Stop'; try {{ Remove-Item -LiteralPath '{}' -Recurse -Force }} catch {{}}; try {{ New-Item -ItemType Directory -Path '{}' -Force | Out-Null }} catch {{}}; [System.IO.File]::WriteAllBytes('{}',[System.Text.Encoding]::UTF8.GetBytes('owned'))",
        powershell_quote(path),
        powershell_quote(path),
        powershell_quote(&path.join("owned"))
    );
    sandbox
        .command(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            Path::new("."),
        )
        .unwrap()
        .output()
        .unwrap()
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(windows)]
fn powershell_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}
