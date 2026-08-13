use super::{InstallLimits, SkillInstallError};
use crate::roots::is_portable_segment;
use std::collections::HashSet;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryEntryKind {
    RegularFile,
    Symlink,
    Submodule,
    Special,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryEntry {
    pub path: String,
    pub kind: RepositoryEntryKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct SelectedPackage {
    pub entries: Vec<RepositoryEntry>,
    pub candidate: String,
}

impl RepositoryEntry {
    #[must_use]
    pub fn regular(path: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            kind: RepositoryEntryKind::RegularFile,
            bytes,
        }
    }

    #[must_use]
    pub fn symlink(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: RepositoryEntryKind::Symlink,
            bytes: Vec::new(),
        }
    }

    #[must_use]
    pub fn submodule(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: RepositoryEntryKind::Submodule,
            bytes: Vec::new(),
        }
    }
}

/// Validates repository entries before any materialization.
///
/// # Errors
///
/// Returns an error for unsupported resource types, unsafe or colliding
/// portable paths, and exceeded file or byte limits.
pub fn validate_repository_tree(
    entries: &[RepositoryEntry],
    limits: &InstallLimits,
) -> Result<(), SkillInstallError> {
    if entries.len() > limits.max_files {
        return Err(SkillInstallError::LimitExceeded);
    }
    let mut total = 0usize;
    let mut portable = HashSet::new();
    for entry in entries {
        if entry.kind != RepositoryEntryKind::RegularFile {
            return Err(SkillInstallError::UnsupportedEntry);
        }
        if entry.bytes.len() > limits.max_file_bytes {
            return Err(SkillInstallError::LimitExceeded);
        }
        total = total
            .checked_add(entry.bytes.len())
            .ok_or(SkillInstallError::LimitExceeded)?;
        if total > limits.max_materialized_bytes {
            return Err(SkillInstallError::LimitExceeded);
        }
        let path = Path::new(&entry.path);
        if path.is_absolute() || entry.path.contains('\\') || entry.path.is_empty() {
            return Err(SkillInstallError::UnsafePath);
        }
        for component in entry.path.split('/') {
            if !is_portable_segment(component) {
                return Err(SkillInstallError::UnsafePath);
            }
        }
        let key = entry.path.nfkc().collect::<String>().to_lowercase();
        if !portable.insert(key) {
            return Err(SkillInstallError::PathCollision);
        }
    }
    Ok(())
}

/// Validates per-object and aggregate expanded-object budgets.
///
/// # Errors
///
/// Returns an error when one object or the aggregate expansion exceeds its
/// configured budget.
pub fn validate_object_expansion(
    expanded_sizes: &[usize],
    limits: &InstallLimits,
) -> Result<(), SkillInstallError> {
    let mut total = 0usize;
    for size in expanded_sizes {
        accumulate_expansion(&mut total, *size, 0, limits)?;
    }
    Ok(())
}

/// Validates the number and cumulative expansion cost of all fetched pack
/// objects, including delta instruction streams and unselected objects.
///
/// # Errors
///
/// Returns an error when the pack contains too many objects, one object or
/// delta allocation is too large, or their aggregate expansion exceeds the
/// configured budget.
pub fn validate_pack_expansion(
    object_and_delta_sizes: &[(usize, usize)],
    limits: &InstallLimits,
) -> Result<(), SkillInstallError> {
    if object_and_delta_sizes.len() > limits.max_objects {
        return Err(SkillInstallError::LimitExceeded);
    }
    let mut total = 0usize;
    for (object_size, delta_size) in object_and_delta_sizes {
        accumulate_expansion(&mut total, *object_size, *delta_size, limits)?;
    }
    Ok(())
}

pub(crate) fn accumulate_expansion(
    total: &mut usize,
    object_size: usize,
    delta_size: usize,
    limits: &InstallLimits,
) -> Result<(), SkillInstallError> {
    if object_size > limits.max_object_bytes || delta_size > limits.max_object_bytes {
        return Err(SkillInstallError::LimitExceeded);
    }
    *total = total
        .checked_add(object_size)
        .and_then(|total| total.checked_add(delta_size))
        .ok_or(SkillInstallError::LimitExceeded)?;
    if *total > limits.max_expanded_object_bytes {
        return Err(SkillInstallError::LimitExceeded);
    }
    Ok(())
}

pub(crate) fn candidate_selectors(entries: &[RepositoryEntry]) -> Vec<String> {
    let mut candidates = entries
        .iter()
        .filter_map(|entry| {
            entry
                .path
                .strip_suffix("/SKILL.md")
                .map(str::to_string)
                .or_else(|| (entry.path == "SKILL.md").then(String::new))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    candidates
}

pub(crate) fn select_package(
    entries: Vec<RepositoryEntry>,
    selector: Option<&str>,
) -> Result<SelectedPackage, SkillInstallError> {
    let candidates = candidate_selectors(&entries);
    let selected = if let Some(selector) = selector {
        if !candidates.iter().any(|candidate| candidate == selector) {
            return Err(SkillInstallError::NoSkill);
        }
        selector
    } else if candidates.len() == 1 {
        &candidates[0]
    } else if candidates.is_empty() {
        return Err(SkillInstallError::NoSkill);
    } else {
        return Err(SkillInstallError::MultipleSkills { candidates });
    };
    let selected_candidate = selected.to_string();
    let prefix = if selected.is_empty() {
        String::new()
    } else {
        format!("{selected}/")
    };
    let selected = entries
        .into_iter()
        .filter_map(|entry| {
            entry
                .path
                .strip_prefix(&prefix)
                .map(str::to_string)
                .map(|path| RepositoryEntry { path, ..entry })
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(SkillInstallError::NoSkill);
    }
    Ok(SelectedPackage {
        entries: selected,
        candidate: selected_candidate,
    })
}
