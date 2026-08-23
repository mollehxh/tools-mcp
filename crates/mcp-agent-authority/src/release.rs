use crate::sandbox::{CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT};
use crate::{ManagedEntryKind, ServerOperations};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const RELEASE_MANIFEST_FILE: &str = "release-manifest.json";
pub const RELEASE_CHECKSUMS_FILE: &str = "SHA256SUMS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleaseArtifactSpec {
    pub path: &'static str,
    pub kind: ReleaseArtifactKind,
    pub mode: u32,
    pub expected_sha256: Option<&'static str>,
}

pub const REQUIRED_RELEASE_ARTIFACTS: [ReleaseArtifactSpec; 15] = [
    spec(
        "LICENSE",
        ReleaseArtifactKind::License,
        0o644,
        Some("d17f227e4df5da1600391338865ce0f3055211760a36688f816941d58232d8dc"),
    ),
    spec(
        "NOTICE",
        ReleaseArtifactKind::Notice,
        0o644,
        Some("9d71575ecfd9a843fc1677b0efb08053c6ba9fd686a0de1a6f5382fd3c220915"),
    ),
    spec(
        "THIRD_PARTY_NOTICES.md",
        ReleaseArtifactKind::Notice,
        0o644,
        Some("155ce30c0b9edeac142dc1659a978b0d5dd65e48f636e251c72493b361500944"),
    ),
    spec("mcp-agent", ReleaseArtifactKind::Executable, 0o755, None),
    spec(
        "sandbox-manifest.json",
        ReleaseArtifactKind::SandboxManifest,
        0o644,
        None,
    ),
    spec(
        "sandbox/macos-seatbelt.marker",
        ReleaseArtifactKind::SandboxMarker,
        0o644,
        None,
    ),
    spec(
        "sandbox/preflight-canary",
        ReleaseArtifactKind::PreflightCanary,
        0o644,
        Some("41f77362a3bca39b73c56e88d50d5711d1cf07ac5bd92dd1a0b92056a91daab0"),
    ),
    spec(
        "sandbox/workspace-write.policy",
        ReleaseArtifactKind::SandboxPolicy,
        0o644,
        None,
    ),
    spec(
        "system-skills/skill-installer/LICENSE.txt",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"),
    ),
    spec(
        "system-skills/skill-installer/SKILL.md",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("72402ab63f95e7a0ee11ebffc0cf32015fbce4c72422d0fe6b290eabea42f506"),
    ),
    spec(
        "system-skills/skill-installer/agents/openai.yaml",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("5ce223d8b1070b82c42298538f1b8d376f788eb9e7a42a987e8c094070d73f0e"),
    ),
    spec(
        "system-skills/skill-installer/assets/skill-installer-small.svg",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("3928703ff00dc1a681e7a22401843b7edcbd4b2051651ce4c43b75f7e140504e"),
    ),
    spec(
        "system-skills/skill-installer/assets/skill-installer.png",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("d0a230b1a79b71b858b7c215a0fbb0768d6459c14ea4ef80c61592629bf0e605"),
    ),
    spec(
        "system-skills/skill-installer/scripts/github_utils.py",
        ReleaseArtifactKind::SystemSkill,
        0o644,
        Some("61c1bbe2ae217433b4b6f9f09f21aca4df52c12598068343ade719f706e4859b"),
    ),
    spec(
        "system-skills/skill-installer/scripts/install-skill-from-github.py",
        ReleaseArtifactKind::SystemSkill,
        0o755,
        Some("0fbbd36e8ea294442c0bd48d6f610a2e8656216bfef5c322f1dcf448ef2f09f1"),
    ),
];

const fn spec(
    path: &'static str,
    kind: ReleaseArtifactKind,
    mode: u32,
    expected_sha256: Option<&'static str>,
) -> ReleaseArtifactSpec {
    ReleaseArtifactSpec {
        path,
        kind,
        mode,
        expected_sha256,
    }
}
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub package: String,
    pub version: String,
    pub target: String,
    pub supported_os: Vec<String>,
    pub capability_protocol: String,
    pub upstream_commit: String,
    pub source_commit: String,
    pub source_tree_state: String,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: ReleaseArtifactKind,
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseArtifactKind {
    License,
    Notice,
    Executable,
    SandboxManifest,
    SandboxMarker,
    SandboxPolicy,
    PreflightCanary,
    SystemSkill,
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("release manifest is missing or invalid")]
    ManifestInvalid,
    #[error("release compatibility metadata does not match this mcp-agent build")]
    CompatibilityMismatch,
    #[error("release artifact checksum, size, path, or type mismatch")]
    ArtifactMismatch,
    #[error("only native macOS release artifacts are supported; Linux and Windows are deferred")]
    UnsupportedPlatform,
    #[error("release verification failed")]
    Io(#[from] std::io::Error),
}

#[must_use]
pub fn current_release_target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

pub fn verify_release(
    release: &Path,
    executable: &Path,
    expected_version: &str,
) -> Result<ReleaseManifest, ReleaseError> {
    let release = release.canonicalize()?;
    let executable = executable.canonicalize()?;
    let packaged_executable = release
        .join("mcp-agent")
        .canonicalize()
        .map_err(|_| ReleaseError::ArtifactMismatch)?;
    if executable != packaged_executable {
        return Err(ReleaseError::ArtifactMismatch);
    }
    verify_release_assets(&release, expected_version)
}

/// Verifies all build-owned assets in a release directory without requiring the
/// running executable to be colocated with them.
///
/// This is the only relaxation used for an explicit `--release-dir`: the same
/// exact file set, compatibility metadata, digests, and modes remain mandatory.
pub fn verify_release_assets(
    release: &Path,
    expected_version: &str,
) -> Result<ReleaseManifest, ReleaseError> {
    let expected_target = current_release_target().ok_or(ReleaseError::UnsupportedPlatform)?;
    let release = release.canonicalize()?;

    let manifest_path = release.join(RELEASE_MANIFEST_FILE);
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .map_err(|_| ReleaseError::ManifestInvalid)?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| ReleaseError::ManifestInvalid)?;
    if manifest.schema_version != 2
        || manifest.package != "mcp-agent"
        || manifest.version != expected_version
        || manifest.target != expected_target
        || manifest.supported_os != ["macos"]
        || manifest.capability_protocol != CAPABILITY_PROTOCOL
        || manifest.upstream_commit != PINNED_CODEX_COMMIT
        || manifest.source_commit.trim().is_empty()
        || !matches!(manifest.source_tree_state.as_str(), "clean" | "dirty")
    {
        return Err(ReleaseError::CompatibilityMismatch);
    }

    if manifest.artifacts.len() != REQUIRED_RELEASE_ARTIFACTS.len() {
        return Err(ReleaseError::ArtifactMismatch);
    }
    let mut observed = BTreeSet::new();
    for (artifact, expected) in manifest
        .artifacts
        .iter()
        .zip(REQUIRED_RELEASE_ARTIFACTS.iter())
    {
        validate_relative(&artifact.path)?;
        if !observed.insert(artifact.path.as_str()) {
            return Err(ReleaseError::ArtifactMismatch);
        }
        if artifact.path != expected.path
            || artifact.kind != expected.kind
            || artifact.mode != expected.mode
        {
            return Err(ReleaseError::ArtifactMismatch);
        }
        let path = release.join(&artifact.path);
        reject_symlink_components(&release, Path::new(&artifact.path))?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| ReleaseError::ArtifactMismatch)?;
        if !metadata.is_file()
            || metadata.len() != artifact.bytes
            || artifact.sha256.len() != 64
            || sha256_file(&path, MAX_ARTIFACT_BYTES)? != artifact.sha256
            || expected
                .expected_sha256
                .is_some_and(|digest| artifact.sha256 != digest)
            || file_mode(&metadata) != artifact.mode
        {
            return Err(ReleaseError::ArtifactMismatch);
        }
    }
    verify_checksum_file(&release, &manifest.artifacts)?;
    let expected_files = REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .map(|spec| spec.path)
        .chain([RELEASE_MANIFEST_FILE, RELEASE_CHECKSUMS_FILE])
        .collect::<BTreeSet<_>>();
    let mut actual_files = BTreeSet::new();
    collect_release_files(&release, &release, &mut actual_files)?;
    if actual_files != expected_files {
        return Err(ReleaseError::ArtifactMismatch);
    }
    Ok(manifest)
}

/// Revalidates the packaged system-skill tree through an already opened,
/// no-follow capability. This is intentionally narrower than release startup
/// verification and is safe to run before every system catalog operation.
pub fn verify_system_skills(root: &ServerOperations) -> Result<(), ReleaseError> {
    let expected = REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .filter(|artifact| artifact.kind == ReleaseArtifactKind::SystemSkill)
        .map(|artifact| {
            let relative = artifact
                .path
                .strip_prefix("system-skills/")
                .ok_or(ReleaseError::ArtifactMismatch)?;
            Ok((
                relative,
                artifact
                    .expected_sha256
                    .ok_or(ReleaseError::ArtifactMismatch)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ReleaseError>>()?;
    let mut observed = BTreeMap::new();
    collect_system_skill_files(root, "", &mut observed)?;
    if observed.len() != expected.len()
        || !observed
            .keys()
            .map(String::as_str)
            .eq(expected.keys().copied())
    {
        return Err(ReleaseError::ArtifactMismatch);
    }
    for (path, digest) in observed {
        if expected.get(path.as_str()) != Some(&digest.as_str()) {
            return Err(ReleaseError::ArtifactMismatch);
        }
    }
    Ok(())
}

fn collect_system_skill_files(
    directory: &ServerOperations,
    prefix: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<(), ReleaseError> {
    let mut entries = directory
        .read_root()
        .map_err(|_| ReleaseError::ArtifactMismatch)?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        let name = entry.name.to_str().ok_or(ReleaseError::ArtifactMismatch)?;
        let relative = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.kind {
            ManagedEntryKind::Directory => {
                let child = directory
                    .open_directory(Path::new(name))
                    .map_err(|_| ReleaseError::ArtifactMismatch)?;
                collect_system_skill_files(&child, &relative, files)?;
            }
            ManagedEntryKind::RegularFile => {
                let mut reader = directory
                    .open_file(Path::new(name))
                    .map_err(|_| ReleaseError::ArtifactMismatch)?;
                let mut hasher = Sha256::new();
                let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
                let mut total = 0_u64;
                loop {
                    let read = reader
                        .read(&mut buffer)
                        .map_err(|_| ReleaseError::ArtifactMismatch)?;
                    if read == 0 {
                        break;
                    }
                    total = total
                        .checked_add(
                            u64::try_from(read).map_err(|_| ReleaseError::ArtifactMismatch)?,
                        )
                        .filter(|total| *total <= MAX_ARTIFACT_BYTES)
                        .ok_or(ReleaseError::ArtifactMismatch)?;
                    hasher.update(&buffer[..read]);
                }
                files.insert(relative, format!("{:x}", hasher.finalize()));
            }
            ManagedEntryKind::Symlink | ManagedEntryKind::Other => {
                return Err(ReleaseError::ArtifactMismatch);
            }
        }
    }
    Ok(())
}

fn verify_checksum_file(release: &Path, artifacts: &[ReleaseArtifact]) -> Result<(), ReleaseError> {
    let mut entries = artifacts
        .iter()
        .map(|artifact| (artifact.path.as_str(), artifact.sha256.clone()))
        .collect::<Vec<_>>();
    entries.push((
        RELEASE_MANIFEST_FILE,
        sha256_file(&release.join(RELEASE_MANIFEST_FILE), MAX_MANIFEST_BYTES)?,
    ));
    entries.sort_unstable_by_key(|(path, _)| *path);
    let mut expected = String::new();
    for (path, digest) in entries {
        writeln!(expected, "{digest}  {path}").map_err(|_| ReleaseError::ArtifactMismatch)?;
    }
    let actual = read_bounded(&release.join(RELEASE_CHECKSUMS_FILE), MAX_MANIFEST_BYTES)
        .map_err(|_| ReleaseError::ArtifactMismatch)?;
    if actual != expected.as_bytes() {
        return Err(ReleaseError::ArtifactMismatch);
    }
    let metadata = fs::symlink_metadata(release.join(RELEASE_CHECKSUMS_FILE))
        .map_err(|_| ReleaseError::ArtifactMismatch)?;
    if !metadata.is_file() || file_mode(&metadata) != 0o644 {
        return Err(ReleaseError::ArtifactMismatch);
    }
    Ok(())
}

fn collect_release_files<'a>(
    release: &'a Path,
    directory: &Path,
    files: &mut BTreeSet<&'a str>,
) -> Result<(), ReleaseError> {
    for entry in fs::read_dir(directory).map_err(|_| ReleaseError::ArtifactMismatch)? {
        let entry = entry.map_err(|_| ReleaseError::ArtifactMismatch)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| ReleaseError::ArtifactMismatch)?;
        if metadata.file_type().is_symlink() {
            return Err(ReleaseError::ArtifactMismatch);
        }
        if metadata.is_dir() {
            collect_release_files(release, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(release)
                .map_err(|_| ReleaseError::ArtifactMismatch)?
                .to_str()
                .ok_or(ReleaseError::ArtifactMismatch)?;
            // All expected paths are static UTF-8. Intern the observed path only
            // for the duration of this verification by matching it to that set.
            let expected = REQUIRED_RELEASE_ARTIFACTS
                .iter()
                .map(|spec| spec.path)
                .chain([RELEASE_MANIFEST_FILE, RELEASE_CHECKSUMS_FILE])
                .find(|expected| *expected == relative)
                .ok_or(ReleaseError::ArtifactMismatch)?;
            files.insert(expected);
        } else {
            return Err(ReleaseError::ArtifactMismatch);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

fn validate_relative(path: &str) -> Result<(), ReleaseError> {
    crate::operations::validate_relative(Path::new(path))
        .map_err(|_| ReleaseError::ArtifactMismatch)
}

fn reject_symlink_components(release: &Path, relative: &Path) -> Result<(), ReleaseError> {
    let mut current = PathBuf::from(release);
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| ReleaseError::ArtifactMismatch)?;
        if metadata.file_type().is_symlink() {
            return Err(ReleaseError::ArtifactMismatch);
        }
    }
    Ok(())
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, std::io::Error> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::other("file exceeds release limit"));
    }
    Ok(bytes)
}

pub(crate) fn sha256_file(path: &Path, max_bytes: u64) -> Result<String, ReleaseError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(ReleaseError::ArtifactMismatch);
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
