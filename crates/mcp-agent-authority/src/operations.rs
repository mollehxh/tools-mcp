use crate::roots::ManagedRoot;
use cap_std::fs::{Dir, OpenOptions};
use std::io::Write;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("managed path must be relative and traversal-free")]
    InvalidPath,
    #[error("managed filesystem operation failed")]
    Io(#[from] std::io::Error),
}

/// Handle-relative operations for server-managed roots. `cap_std::Dir` keeps
/// resolution beneath the opened directory even if an attacker races a
/// symlink replacement between validation and use.
#[derive(Debug)]
pub struct ServerOperations {
    dir: Dir,
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

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self.dir.open_with(&temporary, &options)?;
        if let Err(error) = (|| {
            file.write_all(bytes)?;
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
