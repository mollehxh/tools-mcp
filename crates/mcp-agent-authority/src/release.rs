use crate::sandbox::{CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const RELEASE_MANIFEST_FILE: &str = "release-manifest.json";
pub const REQUIRED_RELEASE_ARTIFACTS: [(&str, ReleaseArtifactKind); 7] = [
    ("LICENSE", ReleaseArtifactKind::License),
    ("NOTICE", ReleaseArtifactKind::Notice),
    ("THIRD_PARTY_NOTICES.md", ReleaseArtifactKind::Notice),
    ("mcp-agent", ReleaseArtifactKind::Executable),
    (
        "sandbox-manifest.json",
        ReleaseArtifactKind::SandboxManifest,
    ),
    (
        "sandbox/macos-seatbelt.marker",
        ReleaseArtifactKind::SandboxMarker,
    ),
    (
        "sandbox/workspace-write.policy",
        ReleaseArtifactKind::SandboxPolicy,
    ),
];
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
    let expected_target = current_release_target().ok_or(ReleaseError::UnsupportedPlatform)?;
    let release = release.canonicalize()?;
    let executable = executable.canonicalize()?;
    if executable.parent() != Some(release.as_path()) {
        return Err(ReleaseError::ArtifactMismatch);
    }

    let manifest_path = release.join(RELEASE_MANIFEST_FILE);
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .map_err(|_| ReleaseError::ManifestInvalid)?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| ReleaseError::ManifestInvalid)?;
    if manifest.schema_version != 1
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

    let required = REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .map(|(path, _)| *path)
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for artifact in &manifest.artifacts {
        validate_relative(&artifact.path)?;
        if !observed.insert(artifact.path.as_str()) {
            return Err(ReleaseError::ArtifactMismatch);
        }
        let expected_kind = REQUIRED_RELEASE_ARTIFACTS
            .iter()
            .find_map(|(path, kind)| (*path == artifact.path).then_some(*kind));
        if expected_kind != Some(artifact.kind) {
            return Err(ReleaseError::ArtifactMismatch);
        }
        let path = release.join(&artifact.path);
        reject_symlink_components(&release, Path::new(&artifact.path))?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| ReleaseError::ArtifactMismatch)?;
        if !metadata.is_file()
            || metadata.len() != artifact.bytes
            || artifact.sha256.len() != 64
            || sha256_file(&path, MAX_ARTIFACT_BYTES)? != artifact.sha256
        {
            return Err(ReleaseError::ArtifactMismatch);
        }
    }
    if observed != required {
        return Err(ReleaseError::ArtifactMismatch);
    }
    if !observed.contains(
        executable
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ReleaseError::ArtifactMismatch)?,
    ) {
        return Err(ReleaseError::ArtifactMismatch);
    }
    Ok(manifest)
}

fn validate_relative(path: &str) -> Result<(), ReleaseError> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ReleaseError::ArtifactMismatch);
    }
    Ok(())
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
