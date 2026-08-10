use anyhow::{Context, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

const PINNED_COMMIT: &str = "8cabf5a6cf103cebe338d46346e43e3201e64f41";

#[derive(Debug, Deserialize)]
struct SourceManifest {
    version: u32,
    repository: String,
    commit: String,
    license: String,
    license_path: String,
    license_sha256: String,
    notice_path: String,
    notice_sha256: String,
    audited_roots: Vec<String>,
    #[serde(default)]
    boundaries: Vec<Boundary>,
    files: Vec<SourceFile>,
}

#[derive(Debug, Deserialize)]
struct Boundary {
    symbol: String,
    local_path: String,
    status: String,
    requirements: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SourceFile {
    local_path: String,
    upstream_path: String,
    module: String,
    status: String,
    source_sha256: String,
    local_sha256: String,
    license: String,
    requirements: Vec<String>,
}

pub fn verify() -> anyhow::Result<()> {
    let root = std::env::current_dir().context("resolve repository root")?;
    verify_root(&root)?;
    println!("upstream snapshot: PASS ({PINNED_COMMIT})");
    println!("license, notice, source map, hashes, closure, and contract fixtures: PASS");
    Ok(())
}

pub fn verify_root(root: &Path) -> anyhow::Result<()> {
    let manifest_path = root.join("third_party/openai-codex/SOURCE.toml");
    let manifest_text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: SourceManifest = toml::from_str(&manifest_text).context("parse SOURCE.toml")?;
    ensure!(manifest.version == 1, "unsupported SOURCE.toml version");
    ensure!(
        manifest.commit == PINNED_COMMIT,
        "wrong pinned Codex commit"
    );
    ensure!(
        manifest.repository == "https://github.com/openai/codex",
        "wrong upstream repository"
    );
    ensure!(
        manifest.license == "Apache-2.0",
        "wrong upstream license classification"
    );
    verify_hash(root, &manifest.license_path, &manifest.license_sha256)
        .context("license verification")?;
    verify_hash(root, &manifest.notice_path, &manifest.notice_sha256)
        .context("notice verification")?;

    let mapped = manifest
        .files
        .iter()
        .map(|file| file.local_path.clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        mapped.len() == manifest.files.len(),
        "duplicate SOURCE.toml local_path"
    );

    let boundary_symbols = manifest
        .boundaries
        .iter()
        .map(|boundary| boundary.symbol.as_str())
        .collect::<Vec<_>>();
    for boundary in &manifest.boundaries {
        safe_relative(&boundary.local_path)?;
        ensure!(
            root.join(&boundary.local_path).is_file(),
            "missing boundary file {}",
            boundary.local_path
        );
        ensure!(
            boundary.status == "adapter",
            "boundary {} must be an adapter",
            boundary.symbol
        );
        verify_requirements(&boundary.requirements, &boundary.symbol)?;
    }

    for file in &manifest.files {
        safe_relative(&file.local_path)?;
        ensure!(
            !file.upstream_path.is_empty(),
            "missing upstream path for {}",
            file.local_path
        );
        ensure!(
            !file.module.is_empty(),
            "missing module classification for {}",
            file.local_path
        );
        ensure!(
            file.license == "Apache-2.0",
            "wrong license for {}",
            file.local_path
        );
        verify_requirements(&file.requirements, &file.local_path)?;
        let bytes = verify_hash(root, &file.local_path, &file.local_sha256)?;
        match file.status.as_str() {
            "unchanged" => {
                ensure!(
                    file.local_sha256 == file.source_sha256,
                    "unchanged file hash differs from source: {}",
                    file.local_path
                );
                if Path::new(&file.local_path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                {
                    let source = std::str::from_utf8(&bytes)
                        .with_context(|| format!("read {} as UTF-8", file.local_path))?;
                    verify_crate_imports(source, &boundary_symbols, &file.local_path)?;
                }
            }
            "adapted" => ensure!(
                file.local_sha256 != file.source_sha256,
                "adapted fixture must differ from upstream: {}",
                file.local_path
            ),
            status => anyhow::bail!(
                "invalid modification status {status} for {}",
                file.local_path
            ),
        }
    }

    let mut actual = BTreeSet::new();
    for audited_root in &manifest.audited_roots {
        safe_relative(audited_root)?;
        collect_files(root, &root.join(audited_root), &mut actual)?;
    }
    ensure!(
        actual == mapped,
        "untracked audited source or fixture: mapped={mapped:?}, actual={actual:?}"
    );
    Ok(())
}

fn verify_requirements(requirements: &[String], subject: &str) -> anyhow::Result<()> {
    ensure!(
        !requirements.is_empty(),
        "missing requirement mapping for {subject}"
    );
    ensure!(
        requirements.iter().all(|id| id.starts_with('R')),
        "invalid requirement mapping for {subject}"
    );
    Ok(())
}

fn verify_hash(root: &Path, relative: &str, expected: &str) -> anyhow::Result<Vec<u8>> {
    safe_relative(relative)?;
    let path = root.join(relative);
    let bytes = read_required_file(&path)?;
    verify_bytes_hash(&bytes, expected, relative)?;
    Ok(bytes)
}

pub fn read_required_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    fs::read(path)
        .with_context(|| format!("required provenance file is missing: {}", path.display()))
}

pub fn verify_bytes_hash(bytes: &[u8], expected: &str, subject: &str) -> anyhow::Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    ensure!(
        actual == expected,
        "SHA-256 mismatch for {subject}: expected {expected}, got {actual}"
    );
    Ok(())
}

fn safe_relative(path: &str) -> anyhow::Result<()> {
    let path = Path::new(path);
    ensure!(
        !path.is_absolute(),
        "absolute SOURCE.toml path is forbidden"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "non-normal SOURCE.toml path is forbidden: {}",
        path.display()
    );
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read audited root {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            files.insert(
                path.strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

pub fn verify_crate_imports(
    source: &str,
    allowed_boundaries: &[&str],
    subject: &str,
) -> anyhow::Result<()> {
    for (index, _) in source.match_indices("crate::") {
        let import = source[index..]
            .chars()
            .take_while(|character| {
                character.is_ascii_alphanumeric() || matches!(character, ':' | '_')
            })
            .collect::<String>();
        ensure!(
            allowed_boundaries
                .iter()
                .any(|boundary| import.starts_with(*boundary)),
            "unmapped local import {import} crosses into unchanged snapshot {subject}"
        );
    }
    Ok(())
}
