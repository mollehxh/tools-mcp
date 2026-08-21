use anyhow::{Context, ensure};
use flate2::{Compression, GzBuilder};
use mcp_agent_authority::release::{
    RELEASE_MANIFEST_FILE, REQUIRED_RELEASE_ARTIFACTS, ReleaseArtifact, ReleaseArtifactKind,
    ReleaseManifest, current_release_target,
};
use mcp_agent_authority::sandbox::{CAPABILITY_PROTOCOL, PINNED_CODEX_COMMIT, expected_manifest};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::Command;

const PACKAGE_NAME: &str = "mcp-agent";
#[derive(Clone, Debug)]
pub struct PackageOptions {
    pub repository_root: PathBuf,
    pub binary_path: PathBuf,
    pub output_root: PathBuf,
    pub source_commit: String,
    pub source_tree_state: String,
    pub version: String,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct PackageResult {
    pub release_dir: PathBuf,
    pub archive: PathBuf,
    pub archive_checksum: PathBuf,
}

pub fn ensure_supported_os(os: &str) -> anyhow::Result<()> {
    ensure!(
        os == "macos",
        "mcp-agent packaging is macOS-only; Linux and Windows release support is deferred"
    );
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let result = build()?;
    println!("release directory: {}", result.release_dir.display());
    println!("release archive: {}", result.archive.display());
    println!("archive checksum: {}", result.archive_checksum.display());
    Ok(())
}

pub fn build() -> anyhow::Result<PackageResult> {
    ensure_supported_os(std::env::consts::OS)?;
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask must be inside the repository")?
        .to_path_buf();
    let target = current_release_target().context("unsupported macOS architecture")?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&repository_root)
        .args(["build", "--locked", "--release", "-p", PACKAGE_NAME])
        .status()
        .context("build native mcp-agent release binary")?;
    ensure!(status.success(), "native release build failed");

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
    assemble(&PackageOptions {
        repository_root: repository_root.clone(),
        binary_path: repository_root.join("target/release/mcp-agent"),
        output_root: repository_root.join("target/release-artifacts"),
        source_commit,
        source_tree_state: if source_status.is_empty() {
            "clean".to_owned()
        } else {
            "dirty".to_owned()
        },
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: target.to_owned(),
    })
}

pub fn assemble(options: &PackageOptions) -> anyhow::Result<PackageResult> {
    ensure_supported_os(std::env::consts::OS)?;
    ensure!(
        Some(options.target.as_str()) == current_release_target(),
        "package target must match the native macOS host"
    );
    ensure!(
        !options.version.trim().is_empty()
            && !options.source_commit.trim().is_empty()
            && matches!(options.source_tree_state.as_str(), "clean" | "dirty"),
        "release version, source commit, and tree state are required"
    );
    fs::create_dir_all(&options.output_root).context("create release output root")?;
    let release_name = format!("{PACKAGE_NAME}-{}-{}", options.version, options.target);
    let staging = tempfile::Builder::new()
        .prefix(".mcp-agent-package-")
        .tempdir_in(&options.output_root)
        .context("create same-filesystem package staging")?;
    let staged_release = staging.path().join(&release_name);
    fs::create_dir(&staged_release).context("create staged release directory")?;

    copy_file(&options.binary_path, &staged_release.join(PACKAGE_NAME))?;
    set_executable(&staged_release.join(PACKAGE_NAME))?;
    expected_manifest()
        .context("construct native sandbox compatibility manifest")?
        .write_release_relative(&staged_release)
        .context("materialize native sandbox assets")?;
    for (source, destination) in [
        ("third_party/openai-codex/LICENSE", "LICENSE"),
        ("third_party/openai-codex/NOTICE", "NOTICE"),
        ("THIRD_PARTY_NOTICES.md", "THIRD_PARTY_NOTICES.md"),
    ] {
        copy_file(
            &options.repository_root.join(source),
            &staged_release.join(destination),
        )?;
    }

    let artifacts = REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .map(|(path, kind)| release_artifact(&staged_release, path, *kind))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let manifest = ReleaseManifest {
        schema_version: 1,
        package: PACKAGE_NAME.to_owned(),
        version: options.version.clone(),
        target: options.target.clone(),
        supported_os: vec!["macos".to_owned()],
        capability_protocol: CAPABILITY_PROTOCOL.to_owned(),
        upstream_commit: PINNED_CODEX_COMMIT.to_owned(),
        source_commit: options.source_commit.clone(),
        source_tree_state: options.source_tree_state.clone(),
        artifacts,
    };
    fs::write(
        staged_release.join(RELEASE_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).context("serialize release manifest")?,
    )
    .context("write release manifest")?;
    write_checksums(&staged_release, &manifest.artifacts)?;

    let staged_archive = staging.path().join(format!("{release_name}.tar.gz"));
    create_reproducible_archive(&staged_release, &staged_archive, &release_name)?;

    let release_dir = options.output_root.join(&release_name);
    let archive = options.output_root.join(format!("{release_name}.tar.gz"));
    let archive_checksum = checksum_path(&archive);
    if release_dir.exists() {
        fs::remove_dir_all(&release_dir).context("replace prior release directory")?;
    }
    for path in [&archive, &archive_checksum] {
        if path.exists() {
            fs::remove_file(path).context("replace prior release artifact")?;
        }
    }
    fs::rename(&staged_release, &release_dir).context("install assembled release directory")?;
    fs::rename(&staged_archive, &archive).context("install assembled release archive")?;
    let archive_digest = sha256_file(&archive)?;
    fs::write(
        &archive_checksum,
        format!(
            "{archive_digest}  {}\n",
            archive.file_name().unwrap_or_default().to_string_lossy()
        ),
    )
    .context("write release archive checksum")?;
    Ok(PackageResult {
        release_dir,
        archive,
        archive_checksum,
    })
}

fn release_artifact(
    release: &Path,
    path: &str,
    kind: ReleaseArtifactKind,
) -> anyhow::Result<ReleaseArtifact> {
    let absolute = release.join(path);
    let metadata =
        fs::metadata(&absolute).with_context(|| format!("read package metadata for {path}"))?;
    ensure!(metadata.is_file(), "package artifact is not a file: {path}");
    Ok(ReleaseArtifact {
        path: path.to_owned(),
        sha256: sha256_file(&absolute)?,
        bytes: metadata.len(),
        kind,
    })
}

fn write_checksums(release: &Path, artifacts: &[ReleaseArtifact]) -> anyhow::Result<()> {
    let mut checksums = artifacts
        .iter()
        .map(|artifact| (artifact.path.as_str(), artifact.sha256.clone()))
        .collect::<Vec<_>>();
    checksums.push((
        RELEASE_MANIFEST_FILE,
        sha256_file(&release.join(RELEASE_MANIFEST_FILE))?,
    ));
    checksums.sort_unstable_by_key(|(path, _)| *path);
    let mut sums = String::new();
    for (path, checksum) in checksums {
        writeln!(sums, "{checksum}  {path}")?;
    }
    fs::write(release.join("SHA256SUMS"), sums).context("write package checksums")
}

fn create_reproducible_archive(
    release: &Path,
    archive: &Path,
    release_name: &str,
) -> anyhow::Result<()> {
    let file = fs::File::create(archive).context("create release archive")?;
    let encoder = GzBuilder::new().mtime(0).write(file, Compression::best());
    let mut builder = tar::Builder::new(encoder);
    builder.mode(tar::HeaderMode::Deterministic);
    append_directory(&mut builder, Path::new(release_name))?;
    append_directory(&mut builder, &Path::new(release_name).join("sandbox"))?;
    let mut files = REQUIRED_RELEASE_ARTIFACTS
        .iter()
        .map(|(path, _)| *path)
        .chain([RELEASE_MANIFEST_FILE, "SHA256SUMS"])
        .collect::<Vec<_>>();
    files.sort_unstable();
    for relative in files {
        let mut source = fs::File::open(release.join(relative))?;
        let metadata = source.metadata()?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(if relative == PACKAGE_NAME {
            0o755
        } else {
            0o644
        });
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

fn append_directory<W: IoWrite>(builder: &mut tar::Builder<W>, path: &Path) -> anyhow::Result<()> {
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
    fs::copy(source, destination).with_context(|| {
        format!(
            "copy package file from {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_executable(_path: &Path) -> anyhow::Result<()> {
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

fn command_stdout(command: &mut Command, context: &str) -> anyhow::Result<String> {
    let output = command.output().with_context(|| context.to_owned())?;
    ensure!(output.status.success(), "{context} failed");
    let stdout = String::from_utf8(output.stdout).context("command output was not UTF-8")?;
    Ok(stdout.trim().to_owned())
}
