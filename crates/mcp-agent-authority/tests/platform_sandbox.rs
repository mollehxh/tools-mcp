use mcp_agent_authority::sandbox::{
    CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT, Sandbox, SandboxError, SandboxManifest,
    VerifiedSandbox, expected_manifest,
};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use sha2::Digest;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Output;
use std::sync::Arc;

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
    loaded_sandbox(fixture).preflight().unwrap().0
}

fn capability_sandbox(
    fixture: &conformance::Fixture,
) -> (VerifiedSandbox, Arc<CapabilitySnapshot>) {
    let system_skills = fixture.outside.join("system-skills");
    fs::create_dir_all(&system_skills).unwrap();
    let home = fixture.outside.join("home");
    let codex_home = fixture.outside.join("codex-home");
    let tmpdir = fixture.outside.join("tmpdir");
    let cargo_home = fixture.outside.join("cargo-home");
    let gradle_home = fixture.outside.join("gradle-home");
    let canonical_tmp = fixture.outside.join("canonical-tmp");
    fs::create_dir_all(&canonical_tmp).unwrap();
    let capabilities = Arc::new(
        CapabilitySnapshot::resolve_configured(
            &fixture.workspace,
            &system_skills,
            |name| match name {
                "HOME" => Some(OsString::from(&home)),
                "CODEX_HOME" => Some(OsString::from(&codex_home)),
                "TMPDIR" => Some(OsString::from(&tmpdir)),
                "CARGO_HOME" => Some(OsString::from(&cargo_home)),
                "GRADLE_USER_HOME" => Some(OsString::from(&gradle_home)),
                _ => None,
            },
            canonical_tmp,
            fixture.outside.join("fallback-tmp"),
        )
        .unwrap(),
    );
    let authority = WorkspaceAuthority::from_capabilities(Arc::clone(&capabilities)).unwrap();
    let release = fixture.release_dir();
    expected_manifest()
        .unwrap()
        .write_release_relative(&release)
        .unwrap();
    let sandbox = Sandbox::load(authority, &release)
        .unwrap()
        .preflight()
        .unwrap()
        .0;
    (sandbox, capabilities)
}

#[test]
fn manifest_carries_protocol_target_checksum_and_pinned_provenance() {
    let manifest = expected_manifest().unwrap();

    assert_eq!(manifest.capability_protocol, CAPABILITY_PROTOCOL);
    assert_eq!(manifest.upstream_commit, PINNED_CODEX_COMMIT);
    assert!(!manifest.target.is_empty());
    assert_eq!(manifest.artifact_sha256.len(), 64);
    assert_eq!(manifest.policy_sha256.len(), 64);
    assert_eq!(manifest.canary_sha256.len(), 64);
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
        sandbox.preflight(),
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
        sandbox.preflight(),
        Err(SandboxError::ArtifactReplaced)
    ));
}

#[test]
fn preflight_canary_replacement_after_load_fails_closed() {
    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    let sandbox = Sandbox::load(fixture.authority(), &release).unwrap();

    fs::write(release.join(&manifest.canary_path), b"replaced").unwrap();
    assert!(matches!(
        sandbox.preflight(),
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
            sandbox.preflight(),
            Err(SandboxError::ArtifactReplaced)
        ));
    } else {
        assert_eq!(fs::read(&helper).unwrap(), original);
    }
}

#[test]
fn native_packaged_clean_install_preflight_proves_required_capabilities() {
    let fixture = conformance::Fixture::new();
    let (sandbox, receipt) = loaded_sandbox(&fixture)
        .preflight()
        .expect("packaged native sandbox preflight must pass");

    assert!(receipt.outside_read_allowed);
    assert!(receipt.local_service_allowed);
    assert!(receipt.workspace_write_allowed);
    assert!(receipt.outside_write_denied);
    assert_eq!(sandbox.preflight_receipt(), &receipt);
    assert!(receipt.listener_bind_allowed);
    assert!(receipt.descendant_write_allowed);
    assert!(receipt.release_canary_verified);
}

#[test]
fn native_sandbox_allows_every_capability_snapshot_writable_root() {
    let fixture = conformance::Fixture::new();
    let (sandbox, capabilities) = capability_sandbox(&fixture);

    assert_eq!(
        sandbox.preflight_receipt().writable_roots_checked,
        capabilities.writable_roots().len()
    );
    for (index, root) in capabilities.writable_roots().iter().enumerate() {
        let destination = root.join(format!("root-{index}-write"));
        assert!(
            native_write_file(&sandbox, &destination).status.success(),
            "sandbox rejected writable root {}",
            root.display()
        );
        assert_eq!(fs::read(destination).unwrap(), b"allowed");
    }
}

#[cfg(unix)]
#[test]
fn preflight_fails_when_unix_permissions_could_explain_canary_denial() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = conformance::Fixture::new();
    let release = fixture.release_dir();
    let manifest = expected_manifest().unwrap();
    manifest.write_release_relative(&release).unwrap();
    fs::set_permissions(
        release.join(&manifest.canary_path),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let sandbox = Sandbox::load(fixture.authority(), &release).unwrap();

    assert!(matches!(
        sandbox.preflight(),
        Err(SandboxError::Preflight(_))
    ));
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
fn native_sandbox_allows_creation_of_workspace_metadata_roots() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);

    for protected in [".git", ".codex"] {
        let protected_path = fixture.workspace.join(protected);
        fs::remove_dir_all(&protected_path).unwrap();
        assert!(!protected_path.exists());

        let output = native_replace_protected_root(&sandbox, &protected_path);

        assert!(output.status.success());
        assert_eq!(fs::read(protected_path.join("owned")).unwrap(), b"owned");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_policy_is_deny_default_with_indexed_writable_roots() {
    let fixture = conformance::Fixture::new();
    let sandbox = loaded_sandbox(&fixture);
    let policy = sandbox.render_native_policy().unwrap();

    assert!(policy.contains("(deny default)"));
    assert!(policy.contains("(allow file-read*)"));
    assert!(policy.contains("(allow network-outbound)"));
    assert!(policy.contains("(allow network-inbound)"));
    assert!(policy.contains("(allow system-socket)"));
    assert!(!policy.contains("127.0.0.1"));
    assert!(policy.contains("(allow file-write*"));
    assert!(policy.contains("(subpath (param \"WRITABLE_ROOT_0\"))"));
    assert!(!policy.contains("PROTECTED_"));
    assert!(!policy.contains("(allow default)"));
}

#[test]
fn native_sandbox_supports_normal_git_metadata_workflows() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let script = concat!(
        "printf source > tracked.txt && ",
        "/usr/bin/git init -q && ",
        "/usr/bin/git add tracked.txt && ",
        "/usr/bin/git -c user.name=U2 -c user.email=u2@example.invalid commit -qm initial && ",
        "/usr/bin/git branch feature/u2 && ",
        "mkdir -p .codex/cache .mcp-agent/runtime .agents/state && ",
        "mv .codex/cache .codex/cache-renamed && ",
        "rmdir .codex/cache-renamed"
    );

    let output = sandbox
        .command("/bin/sh", &["-c", script], &fixture.workspace)
        .unwrap()
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture
            .workspace
            .join(".git/refs/heads/feature/u2")
            .is_file()
    );
    assert!(fixture.workspace.join(".mcp-agent/runtime").is_dir());
    assert!(fixture.workspace.join(".agents/state").is_dir());
}

#[cfg(unix)]
#[test]
fn native_symlink_alias_and_daemonized_descendants_cannot_escape() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let sentinel = fixture.outside.join("alias-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&fixture.outside, fixture.workspace.join("escape")).unwrap();

    let alias = native_write_file(&sandbox, &fixture.workspace.join("escape/alias-sentinel"));
    assert!(!alias.status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");

    let script = format!(
        "(/bin/sh -c 'sleep 0.05; printf changed > {}') >/dev/null 2>&1 & wait",
        shell_quote(&sentinel)
    );
    let _daemonized = sandbox
        .command("/bin/sh", &["-c", &script], &fixture.workspace)
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
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
fn native_sandbox_allows_workspace_including_metadata_roots() {
    let fixture = conformance::Fixture::new();
    let sandbox = installed_sandbox(&fixture);
    let workspace_file = fixture.workspace.join("allowed");

    assert!(
        native_write_file(&sandbox, &workspace_file)
            .status
            .success()
    );
    assert_eq!(fs::read(&workspace_file).unwrap(), b"allowed");

    for metadata in [".git", ".codex", ".mcp-agent", ".agents"] {
        let metadata_file = fixture.workspace.join(metadata).join("allowed");
        assert!(native_write_file(&sandbox, &metadata_file).status.success());
        assert_eq!(fs::read(metadata_file).unwrap(), b"allowed");
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
