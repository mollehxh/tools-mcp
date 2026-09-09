use anyhow::{Context as _, ensure};
use flate2::{Compression, GzBuilder};
use mcp_agent_authority::release::{REQUIRED_RELEASE_ARTIFACTS, ReleaseArtifactKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const PACKAGE_NAME: &str = "tools-mcp-vps";
const MANIFEST_FILE: &str = "release-manifest.json";
const CHECKSUMS_FILE: &str = "SHA256SUMS";
const SYSTEM_SKILL_MANIFEST: &str = "system-skills.sha256";
const BINARIES: &[(&str, &str)] = &[
    ("mcp-agent-gateway", "mcp-agent-gateway"),
    ("tools-mcp-admin", "tools-mcp-admin"),
    ("mcp-agent-vps-runner", "mcp-agent-vps-runner"),
];
const REQUIRED_DEPLOY_ASSETS: &[&str] = &[
    "deploy/vps/Containerfile",
    "deploy/vps/container/tool-versions.sh",
    "deploy/vps/haproxy/shared-router.cfg.in",
    "deploy/vps/scripts/storage-preflight",
    "deploy/vps/scripts/rootless-preflight",
    "deploy/vps/scripts/supervise-container",
    "deploy/vps/scripts/verify-container",
    "deploy/vps/scripts/privacy-scan",
    "deploy/vps/scripts/configure-owner-egress",
    "deploy/vps/scripts/owner-ssh-egress",
    "deploy/vps/scripts/provision-tenant-mvp",
    "deploy/vps/systemd/owner-ssh-container.conf",
    "deploy/vps/systemd/owner-ssh-runner.conf",
    "deploy/vps/systemd/tools-mcp-gateway.service",
    "deploy/vps/systemd/tools-mcp-runner.service",
    "deploy/vps/systemd/tools-mcp-tenant-gateway@.service",
    "deploy/vps/systemd/tools-mcp-tenant-container@.service",
    "deploy/vps/systemd/tools-mcp-tenant-runner@.service",
    "deploy/vps/tmpfiles.d/tools-mcp.conf",
];
const REQUIRED_PACKAGED_ASSETS: &[&str] = &[
    "libexec/rootless-preflight",
    "libexec/configure-owner-egress",
    "libexec/owner-ssh-egress",
    "libexec/record-operator-checkpoint",
    "libexec/verify-container",
    "system-skills/skill-installer/SKILL.md",
    SYSTEM_SKILL_MANIFEST,
];

#[derive(Clone, Debug)]
pub struct VpsPackageOptions {
    pub repository_root: PathBuf,
    pub gateway_path: PathBuf,
    pub admin_path: PathBuf,
    pub runner_path: PathBuf,
    pub output_root: PathBuf,
    pub source_commit: String,
    pub source_tree_state: String,
    pub version: String,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct VpsPackageResult {
    pub release_dir: PathBuf,
    pub archive: PathBuf,
    pub archive_checksum: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VpsReleaseManifest {
    schema_version: u32,
    package: String,
    version: String,
    target: String,
    supported_os: Vec<String>,
    source_commit: String,
    source_tree_state: String,
    artifacts: Vec<VpsArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VpsArtifact {
    path: String,
    sha256: String,
    bytes: u64,
    mode: u32,
    kind: String,
}

pub fn run() -> anyhow::Result<()> {
    ensure!(
        std::env::consts::OS == "linux",
        "VPS packaging requires a native Linux builder"
    );
    let repository_root = repository_root()?;
    let target = linux_target()?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&repository_root)
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "mcp-agent-gateway",
            "-p",
            "mcp-agent-vps-runner",
        ])
        .status()
        .context("build VPS release binaries")?;
    ensure!(status.success(), "VPS release build failed");

    let source_commit = command_stdout(
        Command::new("git")
            .current_dir(&repository_root)
            .args(["rev-parse", "HEAD"]),
        "read source commit",
    )?;
    let source_status = command_stdout(
        Command::new("git").current_dir(&repository_root).args([
            "status",
            "--porcelain",
            "--untracked-files=all",
        ]),
        "read source tree state",
    )?;
    let result = assemble(&VpsPackageOptions {
        repository_root: repository_root.clone(),
        gateway_path: repository_root.join("target/release/mcp-agent-gateway"),
        admin_path: repository_root.join("target/release/tools-mcp-admin"),
        runner_path: repository_root.join("target/release/mcp-agent-vps-runner"),
        output_root: repository_root.join("target/release-artifacts"),
        source_commit,
        source_tree_state: if source_status.is_empty() {
            "clean".to_owned()
        } else {
            "dirty".to_owned()
        },
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target,
    })?;
    println!("VPS release directory: {}", result.release_dir.display());
    println!("VPS release archive: {}", result.archive.display());
    println!(
        "VPS archive checksum: {}",
        result.archive_checksum.display()
    );
    Ok(())
}

pub fn assemble(options: &VpsPackageOptions) -> anyhow::Result<VpsPackageResult> {
    ensure!(
        !options.version.trim().is_empty()
            && !options.source_commit.trim().is_empty()
            && matches!(options.source_tree_state.as_str(), "clean" | "dirty"),
        "release version, source commit, and tree state are required"
    );
    ensure!(
        options.target.ends_with("-unknown-linux-gnu"),
        "VPS package target must be Linux GNU"
    );
    for &relative in REQUIRED_DEPLOY_ASSETS {
        ensure!(
            options.repository_root.join(relative).is_file(),
            "required VPS asset is missing: {relative}"
        );
    }

    fs::create_dir_all(&options.output_root).context("create VPS release output root")?;
    let release_name = format!("{PACKAGE_NAME}-{}-{}", options.version, options.target);
    let staging = tempfile::Builder::new()
        .prefix(".tools-mcp-vps-package-")
        .tempdir_in(&options.output_root)
        .context("create same-filesystem VPS package staging")?;
    let staged_release = staging.path().join(&release_name);
    fs::create_dir(&staged_release).context("create staged VPS release directory")?;

    let binary_sources = [
        &options.gateway_path,
        &options.admin_path,
        &options.runner_path,
    ];
    for ((_, packaged_name), source) in BINARIES.iter().zip(binary_sources) {
        let destination = staged_release.join("bin").join(packaged_name);
        copy_file(source, &destination)?;
        set_mode(&destination, 0o755)?;
    }
    copy_tree(
        &options.repository_root.join("deploy/vps"),
        &staged_release.join("deploy/vps"),
    )?;
    copy_tree(
        &options.repository_root.join("deploy/vps/scripts"),
        &staged_release.join("libexec"),
    )?;
    stage_system_skills(&options.repository_root, &staged_release)?;
    for (source, destination) in [
        ("docs/vps-deployment.md", "docs/vps-deployment.md"),
        ("third_party/openai-codex/LICENSE", "LICENSE"),
        ("THIRD_PARTY_NOTICES.md", "THIRD_PARTY_NOTICES.md"),
    ] {
        copy_file(
            &options.repository_root.join(source),
            &staged_release.join(destination),
        )?;
    }

    let mut artifacts = collect_artifacts(&staged_release)?;
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = VpsReleaseManifest {
        schema_version: 1,
        package: PACKAGE_NAME.to_owned(),
        version: options.version.clone(),
        target: options.target.clone(),
        supported_os: vec!["linux".to_owned()],
        source_commit: options.source_commit.clone(),
        source_tree_state: options.source_tree_state.clone(),
        artifacts,
    };
    fs::write(
        staged_release.join(MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).context("serialize VPS release manifest")?,
    )
    .context("write VPS release manifest")?;
    set_mode(&staged_release.join(MANIFEST_FILE), 0o644)?;
    write_checksums(&staged_release, &manifest.artifacts)?;
    set_mode(&staged_release.join(CHECKSUMS_FILE), 0o644)?;
    verify(&staged_release, &options.version, &options.target)?;

    let staged_archive = staging.path().join(format!("{release_name}.tar.gz"));
    create_reproducible_archive(&staged_release, &staged_archive, &release_name)?;
    let release_dir = options.output_root.join(&release_name);
    let archive = options.output_root.join(format!("{release_name}.tar.gz"));
    let archive_checksum = checksum_path(&archive);
    if release_dir.exists() {
        fs::remove_dir_all(&release_dir).context("replace prior VPS release directory")?;
    }
    for path in [&archive, &archive_checksum] {
        if path.exists() {
            fs::remove_file(path).context("replace prior VPS release artifact")?;
        }
    }
    fs::rename(&staged_release, &release_dir).context("install VPS release directory")?;
    fs::rename(&staged_archive, &archive).context("install VPS release archive")?;
    let digest = sha256_file(&archive)?;
    fs::write(
        &archive_checksum,
        format!(
            "{digest}  {}\n",
            archive.file_name().unwrap_or_default().to_string_lossy()
        ),
    )
    .context("write VPS archive checksum")?;
    Ok(VpsPackageResult {
        release_dir,
        archive,
        archive_checksum,
    })
}

pub fn verify(release: &Path, expected_version: &str, expected_target: &str) -> anyhow::Result<()> {
    let manifest: VpsReleaseManifest = serde_json::from_slice(
        &fs::read(release.join(MANIFEST_FILE)).context("read VPS release manifest")?,
    )
    .context("parse VPS release manifest")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported VPS manifest schema"
    );
    ensure!(
        manifest.package == PACKAGE_NAME,
        "unexpected VPS package name"
    );
    ensure!(
        manifest.version == expected_version,
        "VPS package version mismatch"
    );
    ensure!(
        manifest.target == expected_target,
        "VPS package target mismatch"
    );
    ensure!(
        manifest.supported_os == ["linux"],
        "VPS package must claim only Linux"
    );
    ensure!(
        !manifest.source_commit.trim().is_empty()
            && matches!(manifest.source_tree_state.as_str(), "clean" | "dirty"),
        "VPS package provenance is incomplete"
    );
    let mut seen = BTreeSet::new();
    for artifact in &manifest.artifacts {
        validate_relative_path(&artifact.path)?;
        ensure!(
            seen.insert(artifact.path.clone()),
            "duplicate VPS artifact path"
        );
        let path = release.join(&artifact.path);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("read VPS artifact {}", artifact.path))?;
        ensure!(
            metadata.file_type().is_file(),
            "VPS artifact must be a regular file"
        );
        ensure!(
            metadata.len() == artifact.bytes,
            "VPS artifact size mismatch"
        );
        ensure!(
            sha256_file(&path)? == artifact.sha256,
            "VPS artifact checksum mismatch"
        );
        ensure!(
            file_mode(&path)? == artifact.mode,
            "VPS artifact mode mismatch"
        );
        if artifact.path.starts_with("libexec/") {
            ensure!(
                artifact.mode == 0o755,
                "installed VPS scripts must be executable"
            );
        }
    }
    for (_, binary) in BINARIES {
        ensure!(
            seen.contains(&format!("bin/{binary}")),
            "required VPS binary is missing"
        );
    }
    for &relative in REQUIRED_DEPLOY_ASSETS {
        ensure!(
            seen.contains(relative),
            "required VPS deployment asset is missing"
        );
    }
    for relative in REQUIRED_PACKAGED_ASSETS {
        ensure!(
            seen.contains(*relative),
            "required installed VPS asset is missing"
        );
    }
    let mut packaged_files = Vec::new();
    collect_files(release, release, &mut packaged_files)?;
    let packaged_files = packaged_files
        .into_iter()
        .map(|path| slash_path(&path))
        .collect::<anyhow::Result<BTreeSet<_>>>()?;
    let mut expected_files = seen;
    expected_files.insert(MANIFEST_FILE.to_owned());
    expected_files.insert(CHECKSUMS_FILE.to_owned());
    ensure!(
        packaged_files == expected_files,
        "VPS package contains an unmanifested or missing file"
    );
    verify_checksums(release, &manifest.artifacts)
}

fn validate_relative_path(path: &str) -> anyhow::Result<()> {
    use std::path::Component;

    ensure!(!path.is_empty(), "VPS artifact path is empty");
    ensure!(
        Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "VPS artifact path must be a normalized relative path"
    );
    Ok(())
}

fn collect_artifacts(root: &Path) -> anyhow::Result<Vec<VpsArtifact>> {
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|relative| {
            let path = root.join(&relative);
            let metadata = fs::metadata(&path)?;
            let relative = slash_path(&relative)?;
            let kind = if relative.starts_with("bin/") {
                "binary"
            } else if relative.starts_with("deploy/") {
                "deployment"
            } else {
                "documentation"
            };
            Ok(VpsArtifact {
                path: relative,
                sha256: sha256_file(&path)?,
                bytes: metadata.len(),
                mode: file_mode(&path)?,
                kind: kind.to_owned(),
            })
        })
        .collect()
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        ensure!(
            !file_type.is_symlink(),
            "VPS package source must not contain symlinks"
        );
        if file_type.is_dir() {
            collect_files(root, &entry.path(), output)?;
        } else {
            ensure!(
                file_type.is_file(),
                "VPS package source must contain only files"
            );
            output.push(entry.path().strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let mut files = Vec::new();
    collect_files(source, source, &mut files)?;
    for relative in files {
        let target = destination.join(&relative);
        copy_file(&source.join(&relative), &target)?;
        set_mode(&target, file_mode(&source.join(&relative))?)?;
    }
    Ok(())
}

fn stage_system_skills(repository: &Path, release: &Path) -> anyhow::Result<()> {
    let mut manifest = String::new();
    for spec in REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .filter(|spec| spec.kind == ReleaseArtifactKind::SystemSkill)
    {
        let relative = Path::new(spec.path)
            .strip_prefix("system-skills/skill-installer")
            .context("system skill escaped the packaged installer root")?;
        let destination = release.join(spec.path);
        copy_file(
            &repository
                .join("third_party/openai-codex/skill-installer")
                .join(relative),
            &destination,
        )?;
        set_mode(&destination, spec.mode)?;
        writeln!(manifest, "{}  {}", sha256_file(&destination)?, spec.path)?;
    }
    fs::write(release.join(SYSTEM_SKILL_MANIFEST), manifest)
        .context("write VPS system-skill manifest")?;
    set_mode(&release.join(SYSTEM_SKILL_MANIFEST), 0o644)
}

fn write_checksums(release: &Path, artifacts: &[VpsArtifact]) -> anyhow::Result<()> {
    let mut checksums = artifacts
        .iter()
        .map(|artifact| (artifact.path.clone(), artifact.sha256.clone()))
        .collect::<Vec<_>>();
    checksums.push((
        MANIFEST_FILE.to_owned(),
        sha256_file(&release.join(MANIFEST_FILE))?,
    ));
    checksums.sort_by(|left, right| left.0.cmp(&right.0));
    let mut output = String::new();
    for (path, digest) in checksums {
        writeln!(output, "{digest}  {path}")?;
    }
    fs::write(release.join(CHECKSUMS_FILE), output).context("write VPS checksums")
}

fn verify_checksums(release: &Path, artifacts: &[VpsArtifact]) -> anyhow::Result<()> {
    let expected = fs::read_to_string(release.join(CHECKSUMS_FILE))?;
    let mut actual = String::new();
    let mut checksums = artifacts
        .iter()
        .map(|artifact| (artifact.path.clone(), artifact.sha256.clone()))
        .collect::<Vec<_>>();
    checksums.push((
        MANIFEST_FILE.to_owned(),
        sha256_file(&release.join(MANIFEST_FILE))?,
    ));
    checksums.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, digest) in checksums {
        writeln!(actual, "{digest}  {path}")?;
    }
    ensure!(
        expected == actual,
        "VPS checksum file does not match manifest"
    );
    Ok(())
}

fn create_reproducible_archive(
    release: &Path,
    archive: &Path,
    release_name: &str,
) -> anyhow::Result<()> {
    let mut files = Vec::new();
    collect_files(release, release, &mut files)?;
    files.sort();
    let mut directories = BTreeSet::new();
    for relative in &files {
        let mut parent = relative.parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }
    let file = fs::File::create(archive)?;
    let encoder = GzBuilder::new().mtime(0).write(file, Compression::best());
    let mut builder = tar::Builder::new(encoder);
    builder.mode(tar::HeaderMode::Deterministic);
    append_directory(&mut builder, Path::new(release_name))?;
    for directory in directories {
        append_directory(&mut builder, &Path::new(release_name).join(directory))?;
    }
    for relative in files {
        let source_path = release.join(&relative);
        let mut source = fs::File::open(&source_path)?;
        let metadata = source.metadata()?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(file_mode(&source_path)?);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(
            &mut header,
            Path::new(release_name).join(relative),
            &mut source,
        )?;
    }
    let encoder = builder.into_inner()?;
    encoder.finish()?.sync_all()?;
    Ok(())
}

fn append_directory<W: Write>(builder: &mut tar::Builder<W>, path: &Path) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, path, std::io::empty())?;
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)
        .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    Ok(())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> anyhow::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(path: &Path) -> anyhow::Result<u32> {
    let executable = path
        .components()
        .any(|component| component.as_os_str() == "bin")
        || path.extension().is_none();
    Ok(if executable { 0o755 } else { 0o644 })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn checksum_path(archive: &Path) -> PathBuf {
    let mut name: OsString = archive.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

fn slash_path(path: &Path) -> anyhow::Result<String> {
    let path = path.to_str().context("VPS package path must be UTF-8")?;
    Ok(path.replace('\\', "/"))
}

fn repository_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask repository root is unavailable")?
        .to_path_buf())
}

fn linux_target() -> anyhow::Result<String> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        architecture => anyhow::bail!("unsupported Linux VPS architecture: {architecture}"),
    };
    Ok(format!("{architecture}-unknown-linux-gnu"))
}

fn command_stdout(command: &mut Command, context: &str) -> anyhow::Result<String> {
    let output = command.output().with_context(|| context.to_owned())?;
    ensure!(output.status.success(), "{context} failed");
    Ok(String::from_utf8(output.stdout)
        .context("command output was not UTF-8")?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::{VpsPackageOptions, assemble, sha256_file, verify};
    use std::fs;

    #[test]
    fn vps_bundle_reproduces_and_verifies() {
        let repository_root = super::repository_root().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let binaries = temporary.path().join("binaries");
        fs::create_dir(&binaries).unwrap();
        for binary in [
            "mcp-agent-gateway",
            "tools-mcp-admin",
            "mcp-agent-vps-runner",
        ] {
            fs::write(binaries.join(binary), format!("fixture:{binary}\n")).unwrap();
        }
        let options = |output_root| VpsPackageOptions {
            repository_root: repository_root.clone(),
            gateway_path: binaries.join("mcp-agent-gateway"),
            admin_path: binaries.join("tools-mcp-admin"),
            runner_path: binaries.join("mcp-agent-vps-runner"),
            output_root,
            source_commit: "fixture-commit".to_owned(),
            source_tree_state: "clean".to_owned(),
            version: "0.0.0-test".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
        };
        let first = assemble(&options(temporary.path().join("first"))).unwrap();
        let second = assemble(&options(temporary.path().join("second"))).unwrap();
        assert_eq!(
            sha256_file(&first.archive).unwrap(),
            sha256_file(&second.archive).unwrap()
        );
        verify(&first.release_dir, "0.0.0-test", "x86_64-unknown-linux-gnu").unwrap();

        fs::write(
            first.release_dir.join("deploy/vps/Containerfile"),
            "tampered\n",
        )
        .unwrap();
        assert!(verify(&first.release_dir, "0.0.0-test", "x86_64-unknown-linux-gnu").is_err());
    }
}
