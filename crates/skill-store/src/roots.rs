use crate::contracts::{HostSkillMetadata, ListedSkill, SkillScope};
use crate::upstream::parse_skill_frontmatter_metadata;
use mcp_agent_authority::release::verify_system_skills;
use mcp_agent_authority::{
    ManagedEntryKind, ManagedFileReader, ServerOperations, WorkspaceAuthority,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

const MAX_SKILL_NAME_BYTES: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 1_024;
const TRUNCATED_SUFFIX: &str = "...";
const MAX_DISCOVERY_WARNINGS: usize = 20;

#[derive(Debug)]
pub(crate) struct SkillRoots {
    authority: WorkspaceAuthority,
}

#[derive(Debug)]
struct SkillPackage {
    name: String,
    operations: ServerOperations,
}

#[derive(Debug, Default)]
pub(crate) struct ScopeSnapshot {
    pub entries: Vec<ListedSkill>,
    pub warnings: Vec<String>,
    pub fingerprint: u64,
    packages: Vec<SkillPackage>,
}

impl ScopeSnapshot {
    pub fn contains_package(&self, package: &str) -> bool {
        self.packages.iter().any(|entry| entry.name == package)
    }

    pub fn open_resource(&self, package: &str, relative: &Path) -> Result<ManagedFileReader, ()> {
        self.packages
            .iter()
            .find(|entry| entry.name == package)
            .ok_or(())?
            .operations
            .open_file(relative)
            .map_err(|_| ())
    }
}

impl SkillRoots {
    pub fn new(authority: &WorkspaceAuthority) -> Self {
        Self {
            authority: authority.clone(),
        }
    }

    pub fn scan(&self, scope: SkillScope) -> Result<ScopeSnapshot, ()> {
        let Some(root) = self.operations(scope)? else {
            return Ok(if scope == SkillScope::System {
                ScopeSnapshot::default()
            } else {
                ScopeSnapshot {
                    warnings: vec!["The skill root could not be opened safely.".to_string()],
                    ..ScopeSnapshot::default()
                }
            });
        };
        let mut warnings = Vec::new();
        let mut entries = Vec::new();
        let mut packages = Vec::new();
        let Ok(mut root_entries) = root.read_root() else {
            return Ok(ScopeSnapshot {
                entries,
                warnings: vec!["The skill root could not be read.".to_string()],
                fingerprint: 0,
                packages,
            });
        };
        root_entries.sort_by(|left, right| left.name.cmp(&right.name));

        for root_entry in root_entries {
            if root_entry.kind != ManagedEntryKind::Directory {
                continue;
            }
            let Some(package) = root_entry.name.to_str() else {
                push_warning(
                    &mut warnings,
                    "A skill with an invalid package name was skipped.",
                );
                continue;
            };
            if !is_portable_segment(package) {
                push_warning(
                    &mut warnings,
                    &format!(
                        "Skill package `{}` was skipped because its name is not portable.",
                        safe_label(package)
                    ),
                );
                continue;
            }
            let Ok(package_operations) = root.open_directory(Path::new(package)) else {
                push_warning(
                    &mut warnings,
                    &format!(
                        "Skill package `{package}` was skipped because its directory could not be opened."
                    ),
                );
                continue;
            };
            let Ok(bytes) = package_operations.read_bytes(Path::new("SKILL.md")) else {
                push_warning(
                    &mut warnings,
                    &format!(
                        "Skill package `{package}` was skipped because SKILL.md could not be read."
                    ),
                );
                continue;
            };
            let Ok(contents) = String::from_utf8(bytes) else {
                push_warning(
                    &mut warnings,
                    &format!(
                        "Skill package `{package}` was skipped because SKILL.md is not valid UTF-8."
                    ),
                );
                continue;
            };
            let metadata = match parse_skill_frontmatter_metadata(&contents, || package.to_string())
            {
                Ok(metadata) => metadata,
                Err(error) => {
                    push_warning(
                        &mut warnings,
                        &format!("Skill package `{package}` was skipped: {error}."),
                    );
                    continue;
                }
            };
            let name = truncate_utf8_bytes(&metadata.name, MAX_SKILL_NAME_BYTES);
            let description = truncate_chars(&metadata.description, MAX_DESCRIPTION_CHARS);
            entries.push(ListedSkill::from_host(
                scope,
                HostSkillMetadata {
                    package: package.to_string(),
                    name,
                    description,
                },
            ));
            packages.push(SkillPackage {
                name: package.to_string(),
                operations: package_operations,
            });
        }

        Ok(finish_snapshot(entries, warnings, packages))
    }

    fn operations(&self, scope: SkillScope) -> Result<Option<ServerOperations>, ()> {
        match scope {
            SkillScope::System => {
                let operations = self.authority.open_system_skills().map_err(|_| ())?;
                if let Some(operations) = &operations {
                    verify_system_skills(operations).map_err(|_| ())?;
                }
                Ok(operations)
            }
            SkillScope::Project => Ok(self.authority.open_project_skills().ok()),
            SkillScope::Global => Ok(self.authority.open_global_skills().ok()),
        }
    }
}

fn finish_snapshot(
    mut entries: Vec<ListedSkill>,
    warnings: Vec<String>,
    packages: Vec<SkillPackage>,
) -> ScopeSnapshot {
    entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.package.cmp(&right.package))
    });
    let mut hasher = DefaultHasher::new();
    entries.hash(&mut hasher);
    warnings.hash(&mut hasher);
    ScopeSnapshot {
        entries,
        warnings,
        fingerprint: hasher.finish(),
        packages,
    }
}

pub(crate) fn is_portable_segment(segment: &str) -> bool {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || segment.ends_with(['.', ' '])
        || segment
            .chars()
            .any(|character| character.is_control() || "<>:\"/\\|?*".contains(character))
    {
        return false;
    }
    let stem = segment.split('.').next().unwrap_or(segment);
    !matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn push_warning(warnings: &mut Vec<String>, warning: &str) {
    if warnings.len() < MAX_DISCOVERY_WARNINGS {
        warnings.push(warning.to_string());
    }
}

fn safe_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}{TRUNCATED_SUFFIX}")
    } else {
        prefix
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let end = value.floor_char_boundary(max_bytes);
    value[..end].to_string()
}
