use crate::roots::ManagedRoot;
use crate::workspace::{WorkspaceAuthority, is_protected_top_level, open_dir_component_no_follow};
use cap_primitives::fs::FollowSymlinks;
use cap_std::fs::{Dir, OpenOptions, Permissions};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("workspace path must be relative and traversal-free")]
    InvalidPath,
    #[error("path targets a protected authority root")]
    ProtectedRoot,
    #[error("managed filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Handle-relative operations for server-managed roots. `cap_std::Dir` keeps
/// resolution beneath the opened directory even if an attacker races a
/// symlink replacement between validation and use.
#[derive(Debug)]
pub struct ServerOperations {
    dir: Dir,
}

/// Handle-relative operations rooted at the immutable workspace. Every path
/// component is opened without following links, so a validated path cannot be
/// redirected outside the workspace between validation and mutation.
#[derive(Debug)]
pub struct WorkspaceOperations {
    dir: Dir,
}

impl WorkspaceOperations {
    pub fn new(authority: &WorkspaceAuthority) -> Result<Self, OperationError> {
        Ok(Self {
            dir: authority.try_clone_workspace_dir()?,
        })
    }

    pub fn read_to_string(&self, path: &Path) -> Result<String, OperationError> {
        let (parent, file_name) = self.open_parent(path, false)?;
        let mut options = OpenOptions::new();
        options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
        let mut file = parent.open_with(&file_name, &options)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    }

    /// Atomically replaces the destination name instead of truncating an
    /// existing file. This prevents an existing external hardlink from being
    /// mutated and keeps failed writes from leaving partial target contents.
    pub fn atomic_write(&self, path: &Path, bytes: &[u8]) -> Result<(), OperationError> {
        let (parent, file_name) = self.open_parent(path, true)?;
        let permissions = existing_regular_file_permissions(&parent, Path::new(&file_name))?;
        let temporary = OsString::from(format!(
            ".{}.mcp-agent-{}.tmp",
            file_name.to_string_lossy(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = parent.open_with(&temporary, &options)?;
        if let Err(error) = (|| {
            file.write_all(bytes)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()?;
            parent.rename(&temporary, &parent, &file_name)?;
            Ok::<_, std::io::Error>(())
        })() {
            let _ = parent.remove_file(&temporary);
            return Err(OperationError::Io(error));
        }
        Ok(())
    }

    pub fn remove_file(&self, path: &Path) -> Result<(), OperationError> {
        let (parent, file_name) = self.open_parent(path, false)?;
        let metadata = parent.symlink_metadata(&file_name)?;
        if metadata.is_dir() || metadata.is_symlink() {
            return Err(OperationError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not a regular file",
            )));
        }
        parent.remove_file(file_name)?;
        Ok(())
    }

    fn open_parent(
        &self,
        path: &Path,
        create_missing: bool,
    ) -> Result<(Dir, OsString), OperationError> {
        let components = validate_workspace_relative(path)?;
        let (file_name, parents) = components.split_last().ok_or(OperationError::InvalidPath)?;
        let mut current = self.dir.try_clone()?;
        for component in parents {
            current = if create_missing {
                open_or_create_component(&current, component)?
            } else {
                open_component(&current, component)?
            };
        }
        Ok((current, file_name.clone()))
    }
}

impl ServerOperations {
    pub fn new(root: &ManagedRoot) -> Result<Self, OperationError> {
        Ok(Self {
            dir: root.try_clone_dir()?,
        })
    }

    pub fn atomic_write(&self, path: &Path, bytes: &[u8]) -> Result<(), OperationError> {
        validate_relative(path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        if !parent.as_os_str().is_empty() {
            self.dir.create_dir_all(parent)?;
        }
        let file_name = path
            .file_name()
            .ok_or(OperationError::InvalidPath)?
            .to_string_lossy();
        let temporary = parent.join(format!(
            ".{file_name}.mcp-agent-{}.tmp",
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let permissions = existing_regular_file_permissions(&self.dir, path)?;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self.dir.open_with(&temporary, &options)?;
        if let Err(error) = (|| {
            file.write_all(bytes)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()?;
            self.dir.rename(&temporary, &self.dir, path)?;
            Ok::<_, std::io::Error>(())
        })() {
            let _ = self.dir.remove_file(&temporary);
            return Err(OperationError::Io(error));
        }
        Ok(())
    }
}

fn validate_relative(path: &Path) -> Result<(), OperationError> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return Err(OperationError::InvalidPath);
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(OperationError::InvalidPath);
        }
    }
    Ok(())
}

fn validate_workspace_relative(path: &Path) -> Result<Vec<OsString>, OperationError> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return Err(OperationError::InvalidPath);
    }
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(OperationError::InvalidPath);
        };
        components.push(component.to_os_string());
    }
    if components
        .first()
        .is_some_and(|component| is_protected_top_level(component))
    {
        return Err(OperationError::ProtectedRoot);
    }
    Ok(components)
}

fn open_or_create_component(parent: &Dir, name: &OsStr) -> Result<Dir, OperationError> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(OperationError::Io(error)),
    }
    open_component(parent, name)
}

fn open_component(parent: &Dir, name: &OsStr) -> Result<Dir, OperationError> {
    open_dir_component_no_follow(parent, name).map_err(OperationError::Io)
}

fn existing_regular_file_permissions(
    parent: &Dir,
    path: &Path,
) -> Result<Option<Permissions>, OperationError> {
    match parent.symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.is_symlink() => {
            Ok(Some(metadata.permissions()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OperationError::Io(error)),
    }
}
