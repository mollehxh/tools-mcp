use super::{RepositoryEntry, SkillInstallError};
use mcp_agent_authority::ServerOperations;
use std::path::Path;
use std::time::Duration;

const INSTALL_LOCK: &str = ".mcp-agent-install.lock";
const INSTALL_PREFIX: &str = ".mcp-agent-install-";

pub(crate) fn acquire_scope_lock(
    staging: &ServerOperations,
) -> Result<mcp_agent_authority::ManagedFileLock, SkillInstallError> {
    staging
        .acquire_exclusive_lock(Path::new(INSTALL_LOCK), Duration::from_secs(5))
        .map_err(|_| SkillInstallError::CommitFailed)
}

pub(crate) fn recover_staging(staging: &ServerOperations) -> Result<(), SkillInstallError> {
    let _lock = acquire_scope_lock(staging)?;
    let entries = staging
        .read_root()
        .map_err(|_| SkillInstallError::CommitFailed)?;
    for entry in entries {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if !name.starts_with(INSTALL_PREFIX) {
            continue;
        }
        if entry.kind != mcp_agent_authority::ManagedEntryKind::Directory {
            return Err(SkillInstallError::CommitFailed);
        }
        discard_package(staging, name)?;
    }
    Ok(())
}

pub(crate) fn materialize_package(
    staging: &ServerOperations,
    staging_name: &str,
    entries: &[RepositoryEntry],
) -> Result<ServerOperations, SkillInstallError> {
    let staging_path = Path::new(staging_name);
    let package_root = staging
        .create_directory(staging_path)
        .map_err(|_| SkillInstallError::CommitFailed)?;
    let result = entries.iter().try_for_each(|entry| {
        package_root
            .atomic_write(Path::new(&entry.path), &entry.bytes)
            .map_err(|_| SkillInstallError::CommitFailed)
    });
    if result.is_err() {
        let _ = remove_tree(&package_root);
        let _ = staging.remove_directory(staging_path);
    }
    result.map(|()| package_root)
}

pub(crate) fn commit_package(
    staging: &ServerOperations,
    staging_name: &str,
    root: &ServerOperations,
    package: &str,
) -> Result<(), SkillInstallError> {
    staging
        .rename_directory_to(Path::new(staging_name), root, Path::new(package))
        .map_err(|error| match error {
            mcp_agent_authority::OperationError::Io(ref source)
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                SkillInstallError::Collision
            }
            _ => SkillInstallError::CommitFailed,
        })
}

pub(crate) fn discard_package(
    staging: &ServerOperations,
    staging_name: &str,
) -> Result<(), SkillInstallError> {
    let path = Path::new(staging_name);
    if let Ok(package) = staging.open_directory(path) {
        remove_tree(&package)?;
        staging
            .remove_directory(path)
            .map_err(|_| SkillInstallError::CommitFailed)?;
    }
    Ok(())
}

fn remove_tree(root: &ServerOperations) -> Result<(), SkillInstallError> {
    let mut entries = root
        .read_root()
        .map_err(|_| SkillInstallError::CommitFailed)?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        let path = Path::new(&entry.name);
        match entry.kind {
            mcp_agent_authority::ManagedEntryKind::Directory => {
                let child = root
                    .open_directory(path)
                    .map_err(|_| SkillInstallError::CommitFailed)?;
                remove_tree(&child)?;
                root.remove_directory(path)
                    .map_err(|_| SkillInstallError::CommitFailed)?;
            }
            mcp_agent_authority::ManagedEntryKind::RegularFile => root
                .remove_file(path)
                .map_err(|_| SkillInstallError::CommitFailed)?,
            _ => return Err(SkillInstallError::CommitFailed),
        }
    }
    Ok(())
}
