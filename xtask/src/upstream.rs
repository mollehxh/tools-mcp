use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

const PINNED_COMMIT: &str = "8cabf5a6cf103cebe338d46346e43e3201e64f41";
const VALID_REQUIREMENTS: &[&str] = &[
    "R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10", "R11", "R12", "R13", "R14", "R15",
    "R16", "R17", "R18", "R19", "R20", "R21", "R22",
];

// This table is deliberately compiled into the verifier instead of being read from SOURCE.toml.
// It is the offline trust root that makes coordinated snapshot + manifest substitution detectable.
const PINNED_SOURCE_DIGESTS: &[(&str, &str)] = &[
    (
        "codex-rs/core/src/unified_exec/head_tail_buffer.rs",
        "24053729f07a437c87dd2277e9f0e993e4891077f11795ba52d8c39de7b56ca6",
    ),
    (
        "codex-rs/core/src/unified_exec/head_tail_buffer_tests.rs",
        "897003527a036f40970ef782f8369e8d5ce882abd6be508b3e7557ead1cfa48b",
    ),
    (
        "codex-rs/apply-patch/src/seek_sequence.rs",
        "5eced89191977d6b53b1b770ccaad6cafa2fb3ecbcd4839a88318f75e4a620e4",
    ),
    (
        "codex-rs/apply-patch/src/streaming_parser.rs",
        "5f4b8e60fd24ada7c1b6a696155de3da2b946f7b5436da90e377c9de04b1e578",
    ),
    (
        "codex-rs/apply-patch/src/parser.rs",
        "6b8086467d0500f4fc9aa9a35cd33a0bce53c01bcb74b915b9efc6fcf187f7ce",
    ),
    (
        "codex-rs/apply-patch/src/lib.rs",
        "4c07280c6c79ef0ad2e761777a9f2cc7dd120d5455c60294efefbaa90d8a8614",
    ),
    (
        "codex-rs/apply-patch/src/file_update.rs",
        "67acb3ac257e3dbaef324c048532d521042c3b857f98e7ba61d76d4eae769b05",
    ),
    (
        "codex-rs/core/src/tools/handlers/apply_patch_spec.rs",
        "91b4e8669c54e00f16470eb0677b0002a180c10ca6dfe0607b93240426fa9eef",
    ),
    (
        "codex-rs/skills/src/parser.rs",
        "f3532df1cc16f4da423b8e5c813269940c90317bcd211b6659cad449fd877e89",
    ),
    (
        "codex-rs/skills/src/parser_tests.rs",
        "89bf58eb2bd97c47bcefcdc605914bc5b3b023e30a8504fa93e2040ae1913b57",
    ),
    (
        "normalized-contracts/tool-contracts-v1",
        "cb2253251f7ac4ec02263f7050118d500b4c8603a07e9b871f64ca963df508bf",
    ),
];

// These exemptions are deliberately compiled into the verifier. A manifest edit cannot turn a
// newly adapted boundary into an unverified legacy boundary by inventing an exemption rationale.
const PINNED_BOUNDARY_PROVENANCE_EXEMPTIONS: &[(&str, &str, &str)] = &[
    (
        "crate::unified_exec",
        "crates/codex-tools-runtime/src/lib.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::ApplyPatchFileUpdateMode",
        "crates/codex-tools-runtime/src/lib.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::contracts::ExecCommandInput",
        "crates/codex-tools-runtime/src/contracts/exec_command.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::contracts::WriteStdinInput",
        "crates/codex-tools-runtime/src/contracts/write_stdin.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::process::ProcessManager",
        "crates/codex-tools-runtime/src/process/manager.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::process::PendingResult",
        "crates/codex-tools-runtime/src/process/manager.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
    (
        "crate::contracts",
        "crates/skill-store/src/contracts.rs",
        "legacy boundary predates source-level adapter provenance",
    ),
];

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
    #[serde(default)]
    local_sha256: Option<String>,
    #[serde(default)]
    sources: Vec<BoundarySource>,
    #[serde(default)]
    provenance_exemption: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BoundarySource {
    upstream_path: String,
    source_sha256: String,
    license: String,
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
    #[serde(default)]
    baseline_path: Option<String>,
    #[serde(default)]
    delta_registry_path: Option<String>,
}

#[derive(Deserialize)]
struct CargoLock {
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
}

pub fn verify() -> anyhow::Result<()> {
    let root = std::env::current_dir().context("resolve repository root")?;
    verify_root(&root)?;
    println!("upstream snapshot: PASS ({PINNED_COMMIT})");
    println!("license, notice, source map, hashes, closure, contracts, and rmcp pin: PASS");
    Ok(())
}

pub fn verify_root(root: &Path) -> anyhow::Result<()> {
    let lock_text = fs::read_to_string(root.join("Cargo.lock")).context("read Cargo.lock")?;
    let lock: CargoLock = toml::from_str(&lock_text).context("parse Cargo.lock")?;
    let rmcp_versions = lock
        .package
        .iter()
        .filter(|package| package.name == "rmcp")
        .map(|package| package.version.clone())
        .collect::<Vec<_>>();
    verify_rmcp_versions(&rmcp_versions)?;

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
        verify_requirement_ids(&boundary.requirements, &boundary.symbol)?;
        if boundary.sources.is_empty() {
            ensure!(
                boundary.local_sha256.is_none(),
                "adapted boundary {} has local_sha256 but no sources",
                boundary.symbol
            );
            let exemption = boundary.provenance_exemption.as_deref().with_context(|| {
                format!(
                    "adapted boundary {} lacks sources and a pinned provenance exemption",
                    boundary.symbol
                )
            })?;
            ensure!(
                !exemption.trim().is_empty(),
                "empty provenance exemption for boundary {}",
                boundary.symbol
            );
            let expected = PINNED_BOUNDARY_PROVENANCE_EXEMPTIONS
                .iter()
                .find_map(|(symbol, local_path, rationale)| {
                    (*symbol == boundary.symbol).then_some((*local_path, *rationale))
                })
                .with_context(|| {
                    format!(
                        "boundary {} is not eligible for a provenance exemption",
                        boundary.symbol
                    )
                })?;
            ensure!(
                boundary.local_path == expected.0,
                "provenance exemption for boundary {} disagrees with the pinned local path",
                boundary.symbol
            );
            ensure!(
                exemption == expected.1,
                "provenance exemption for boundary {} disagrees with the pinned rationale",
                boundary.symbol
            );
        } else {
            ensure!(
                boundary.provenance_exemption.is_none(),
                "adapted boundary {} cannot combine sources with a provenance exemption",
                boundary.symbol
            );
            let local_sha256 = boundary.local_sha256.as_deref().with_context(|| {
                format!("adapted boundary {} lacks local_sha256", boundary.symbol)
            })?;
            verify_hash(root, &boundary.local_path, local_sha256)?;
            for source in &boundary.sources {
                ensure!(
                    source.license == "Apache-2.0",
                    "wrong source license for boundary {}",
                    boundary.symbol
                );
                verify_trusted_source_hash(&source.upstream_path, &source.source_sha256)?;
            }
        }
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
        verify_requirement_ids(&file.requirements, &file.local_path)?;
        let bytes = verify_hash(root, &file.local_path, &file.local_sha256)?;
        match file.status.as_str() {
            "unchanged" => {
                verify_trusted_source_hash(&file.upstream_path, &file.source_sha256)?;
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
            "adapted" => {
                verify_trusted_source_hash(&file.upstream_path, &file.source_sha256)?;
                let baseline_path = file.baseline_path.as_deref().with_context(|| {
                    format!("adapted fixture {} lacks baseline_path", file.local_path)
                })?;
                let registry_path = file.delta_registry_path.as_deref().with_context(|| {
                    format!(
                        "adapted fixture {} lacks delta_registry_path",
                        file.local_path
                    )
                })?;
                safe_relative(registry_path)?;
                let baseline: Value =
                    serde_json::from_slice(&verify_hash(root, baseline_path, &file.source_sha256)?)
                        .with_context(|| {
                            format!("parse normalized contract baseline {baseline_path}")
                        })?;
                let local: Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse adapted fixture {}", file.local_path))?;
                let registry: Value = serde_json::from_slice(&fs::read(root.join(registry_path))?)
                    .with_context(|| {
                        format!("parse compatibility delta registry {registry_path}")
                    })?;
                ensure!(
                    registry["baseline"].as_str() == Some(baseline_path),
                    "delta registry baseline does not match adapted fixture baseline_path"
                );
                verify_contract_delta_coverage(&baseline, &local, &registry)?;
            }
            "baseline" => {
                verify_trusted_source_hash(&file.upstream_path, &file.source_sha256)?;
                ensure!(
                    file.local_sha256 == file.source_sha256,
                    "baseline file hash differs from independent source: {}",
                    file.local_path
                );
            }
            "registry" => {}
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

pub fn verify_rmcp_versions(versions: &[String]) -> anyhow::Result<()> {
    ensure!(
        versions == ["3.0.1"],
        "Cargo.lock must contain exactly rmcp 3.0.1; found {versions:?}"
    );
    Ok(())
}

pub fn verify_requirement_ids(requirements: &[String], subject: &str) -> anyhow::Result<()> {
    ensure!(
        !requirements.is_empty(),
        "missing requirement mapping for {subject}"
    );
    let mut unique = BTreeSet::new();
    for id in requirements {
        ensure!(
            VALID_REQUIREMENTS.contains(&id.as_str()),
            "invalid requirement mapping {id} for {subject}; expected R1..R22"
        );
        ensure!(
            unique.insert(id),
            "duplicate requirement mapping {id} for {subject}"
        );
    }
    Ok(())
}

pub fn verify_trusted_source_hash(upstream_path: &str, manifest_hash: &str) -> anyhow::Result<()> {
    let expected = PINNED_SOURCE_DIGESTS
        .iter()
        .find_map(|(path, digest)| (*path == upstream_path).then_some(*digest))
        .with_context(|| format!("no independent pinned digest for {upstream_path}"))?;
    ensure!(
        manifest_hash == expected,
        "SOURCE.toml hash for {upstream_path} disagrees with independent pinned digest"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DeltaRegistry {
    version: u32,
    baseline: String,
    deltas: Vec<ContractDelta>,
}

#[derive(Debug, Deserialize)]
struct ContractDelta {
    tool: String,
    json_path: String,
    kind: String,
    requirements: Vec<String>,
    upstream_source: String,
    reason: String,
    expected_upstream_sha256: Option<String>,
    expected_local_sha256: Option<String>,
}

pub fn verify_contract_delta_coverage(
    baseline: &Value,
    local: &Value,
    registry: &Value,
) -> anyhow::Result<()> {
    let registry: DeltaRegistry =
        serde_json::from_value(registry.clone()).context("decode compatibility delta registry")?;
    ensure!(registry.version == 1, "unsupported delta registry version");
    ensure!(
        !registry.baseline.trim().is_empty(),
        "delta registry lacks baseline path"
    );

    let baseline = contracts_by_name(baseline, "baseline")?;
    let local = contracts_by_name(local, "local")?;
    let mut differences = BTreeMap::new();
    for tool in baseline.keys().chain(local.keys()).collect::<BTreeSet<_>>() {
        match (baseline.get(tool), local.get(tool)) {
            (Some(upstream), Some(actual)) => {
                collect_json_differences(upstream, actual, "", &mut differences)?;
            }
            (None, Some(_)) => {
                differences.insert(((*tool).clone(), String::new()), "added".to_string());
            }
            (Some(_), None) => {
                differences.insert(((*tool).clone(), String::new()), "omitted".to_string());
            }
            (None, None) => unreachable!(),
        }
    }

    let mut declared = BTreeMap::new();
    for delta in &registry.deltas {
        verify_requirement_ids(
            &delta.requirements,
            &format!("{}{}", delta.tool, delta.json_path),
        )?;
        ensure!(
            !delta.upstream_source.trim().is_empty(),
            "missing upstream_source for {}{}",
            delta.tool,
            delta.json_path
        );
        ensure!(
            !delta.reason.trim().is_empty(),
            "missing reason for {}{}",
            delta.tool,
            delta.json_path
        );
        let upstream_value = contract_value_at(&baseline, &delta.tool, &delta.json_path);
        let local_value = contract_value_at(&local, &delta.tool, &delta.json_path);
        verify_expected_json_hash(
            upstream_value,
            delta.expected_upstream_sha256.as_deref(),
            &format!("upstream {}{}", delta.tool, delta.json_path),
        )?;
        verify_expected_json_hash(
            local_value,
            delta.expected_local_sha256.as_deref(),
            &format!("local {}{}", delta.tool, delta.json_path),
        )?;
        let key = (delta.tool.clone(), delta.json_path.clone());
        ensure!(
            declared.insert(key.clone(), delta.kind.clone()).is_none(),
            "duplicate contract delta {}{}",
            key.0,
            key.1
        );
    }
    ensure!(
        differences == declared,
        "unregistered contract difference or stale delta: actual={differences:?}, declared={declared:?}"
    );
    Ok(())
}

fn contract_value_at<'a>(
    contracts: &'a BTreeMap<String, Value>,
    tool: &str,
    json_path: &str,
) -> Option<&'a Value> {
    let contract = contracts.get(tool)?;
    if json_path.is_empty() {
        Some(contract)
    } else {
        contract.pointer(json_path)
    }
}

fn verify_expected_json_hash(
    value: Option<&Value>,
    expected: Option<&str>,
    subject: &str,
) -> anyhow::Result<()> {
    match (value, expected) {
        (None, None) => Ok(()),
        (Some(value), Some(expected)) => {
            let canonical = serde_json::to_vec(value).context("serialize contract delta value")?;
            verify_bytes_hash(&canonical, expected, subject)
        }
        (None, Some(_)) => anyhow::bail!("unexpected expected hash for absent {subject}"),
        (Some(_), None) => anyhow::bail!("missing expected hash for present {subject}"),
    }
}

fn contracts_by_name(value: &Value, subject: &str) -> anyhow::Result<BTreeMap<String, Value>> {
    let contracts = value
        .as_array()
        .with_context(|| format!("{subject} contracts must be an array"))?;
    let mut by_name = BTreeMap::new();
    for contract in contracts {
        let name = contract["name"]
            .as_str()
            .with_context(|| format!("{subject} contract lacks name"))?;
        ensure!(
            by_name.insert(name.to_string(), contract.clone()).is_none(),
            "duplicate {subject} contract {name}"
        );
    }
    Ok(by_name)
}

fn collect_json_differences(
    baseline: &Value,
    local: &Value,
    path: &str,
    differences: &mut BTreeMap<(String, String), String>,
) -> anyhow::Result<()> {
    let tool = baseline["name"]
        .as_str()
        .or_else(|| local["name"].as_str())
        .unwrap_or_default()
        .to_string();
    collect_json_differences_for_tool(baseline, local, path, &tool, differences)
}

fn collect_json_differences_for_tool(
    baseline: &Value,
    local: &Value,
    path: &str,
    tool: &str,
    differences: &mut BTreeMap<(String, String), String>,
) -> anyhow::Result<()> {
    if baseline == local {
        return Ok(());
    }
    if let (Some(upstream), Some(actual)) = (baseline.as_object(), local.as_object()) {
        for key in upstream
            .keys()
            .chain(actual.keys())
            .collect::<BTreeSet<_>>()
        {
            let next = format!("{path}/{}", escape_json_pointer(key));
            match (upstream.get(key), actual.get(key)) {
                (Some(left), Some(right)) => {
                    collect_json_differences_for_tool(left, right, &next, tool, differences)?;
                }
                (Some(_), None) => {
                    differences.insert((tool.to_string(), next), "omitted".to_string());
                }
                (None, Some(_)) => {
                    differences.insert((tool.to_string(), next), "added".to_string());
                }
                (None, None) => unreachable!(),
            }
        }
    } else {
        differences.insert((tool.to_string(), path.to_string()), "changed".to_string());
    }
    Ok(())
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
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
            allowed_boundaries.iter().any(|boundary| {
                import == *boundary
                    || import
                        .strip_prefix(*boundary)
                        .is_some_and(|suffix| suffix.starts_with("::"))
            }),
            "unmapped local import {import} crosses into unchanged snapshot {subject}"
        );
    }
    Ok(())
}
