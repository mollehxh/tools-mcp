use crate::roots::ManagedRoot;
use crate::workspace::{WorkspaceAuthority, open_dir_component_no_follow};
use cap_primitives::fs::FollowSymlinks;
use cap_std::fs::{Dir, OpenOptions, Permissions};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("workspace path must be relative and traversal-free")]
    InvalidPath,
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

/// Read-only handle to a regular file opened beneath a server-managed
/// capability. The path is resolved once with no-follow semantics before this
/// handle is returned.
#[derive(Debug)]
pub struct ManagedFileReader {
    file: cap_std::fs::File,
}

/// An exclusive operating-system file lock released automatically on drop.
#[derive(Debug)]
pub struct ManagedFileLock {
    _file: std::fs::File,
}

/// A root-relative directory entry. It deliberately carries no ambient path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedDirEntry {
    pub name: OsString,
    pub kind: ManagedEntryKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedEntryKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
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
    pub(crate) fn from_dir(dir: Dir) -> Self {
        Self { dir }
    }

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

    /// Acquires a bounded, cross-process exclusive lock beneath this root.
    pub fn acquire_exclusive_lock(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<ManagedFileLock, OperationError> {
        validate_relative(path)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            ._cap_fs_ext_follow(FollowSymlinks::No);
        let file = self.dir.open_with(path, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(OperationError::InvalidPath);
        }
        let file = file.into_std();
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(ManagedFileLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(OperationError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "managed lock acquisition timed out",
                    )));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(OperationError::Io(error));
                }
            }
        }
    }

    /// Reopens the current project-skill root and rejects ambient replacement.
    pub fn revalidate_project_root(
        &self,
        authority: &WorkspaceAuthority,
    ) -> Result<Self, OperationError> {
        let workspace = authority.try_clone_workspace_dir()?;
        let agents = open_component(&workspace, OsStr::new(".agents"))?;
        let current = open_component(&agents, OsStr::new("skills"))?;
        if !same_directory(&self.dir, &current)? {
            return Err(OperationError::InvalidPath);
        }
        Ok(Self { dir: current })
    }

    /// Creates a directory and fails if the destination already exists.
    pub fn create_directory(&self, path: &Path) -> Result<Self, OperationError> {
        let components = validate_server_relative(path)?;
        let (name, parents) = components.split_last().ok_or(OperationError::InvalidPath)?;
        let mut current = self.dir.try_clone()?;
        for component in parents {
            current = open_component(&current, component)?;
        }
        current.create_dir(name)?;
        Ok(Self {
            dir: open_component(&current, name)?,
        })
    }

    /// Removes an empty child directory without following links.
    pub fn remove_directory(&self, path: &Path) -> Result<(), OperationError> {
        let components = validate_server_relative(path)?;
        let (name, parents) = components.split_last().ok_or(OperationError::InvalidPath)?;
        let mut current = self.dir.try_clone()?;
        for component in parents {
            current = open_component(&current, component)?;
        }
        current.remove_dir(name)?;
        Ok(())
    }

    /// Removes a regular child file without following links.
    pub fn remove_file(&self, path: &Path) -> Result<(), OperationError> {
        validate_relative(path)?;
        let metadata = self.dir.symlink_metadata(path)?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(OperationError::InvalidPath);
        }
        self.dir.remove_file(path)?;
        Ok(())
    }

    /// Atomically moves a directory between two opened roots on one filesystem.
    pub fn rename_directory_to(
        &self,
        source: &Path,
        destination_root: &Self,
        destination: &Path,
    ) -> Result<(), OperationError> {
        validate_relative(source)?;
        validate_relative(destination)?;
        self.open_directory(source)?;
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        rustix::fs::renameat_with(
            &self.dir,
            source,
            &destination_root.dir,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)?;
        #[cfg(windows)]
        self.dir
            .rename(source, &destination_root.dir, destination)?;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            windows
        )))]
        return Err(OperationError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace directory rename is unavailable",
        )));
        Ok(())
    }

    /// Lists the immediate children of this managed root without exposing its
    /// ambient host path. Entry type inspection does not follow symlinks.
    pub fn read_root(&self) -> Result<Vec<ManagedDirEntry>, OperationError> {
        let mut entries = Vec::new();
        for entry in self.dir.entries()? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                ManagedEntryKind::Symlink
            } else if file_type.is_dir() {
                ManagedEntryKind::Directory
            } else if file_type.is_file() {
                ManagedEntryKind::RegularFile
            } else {
                ManagedEntryKind::Other
            };
            entries.push(ManagedDirEntry {
                name: entry.file_name(),
                kind,
            });
        }
        Ok(entries)
    }

    /// Opens a child directory as a new capability. Every component is opened
    /// without following links, so subsequent operations stay bound to this
    /// exact directory even if its name is replaced in the parent.
    pub fn open_directory(&self, path: &Path) -> Result<Self, OperationError> {
        let components = validate_server_relative(path)?;
        let mut current = self.dir.try_clone()?;
        for component in &components {
            current = open_component(&current, component)?;
        }
        Ok(Self { dir: current })
    }

    /// Opens a regular file for bounded or streaming reads. Every directory
    /// component and the final file are opened without following links.
    pub fn open_file(&self, path: &Path) -> Result<ManagedFileReader, OperationError> {
        let components = validate_server_relative(path)?;
        let (file_name, parents) = components.split_last().ok_or(OperationError::InvalidPath)?;
        let mut current = self.dir.try_clone()?;
        for component in parents {
            current = open_component(&current, component)?;
        }
        let mut options = OpenOptions::new();
        options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
        let file = current.open_with(file_name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(OperationError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not a regular file",
            )));
        }
        Ok(ManagedFileReader { file })
    }

    /// Reads a regular file through the managed root. Every directory and the
    /// final file are opened with no-follow semantics.
    pub fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, OperationError> {
        let mut file = self.open_file(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

pub(crate) fn same_directory(left: &Dir, right: &Dir) -> Result<bool, OperationError> {
    let left = left.dir_metadata()?;
    let right = right.dir_metadata()?;
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        Ok(left.volume_serial_number().is_some()
            && left.volume_serial_number() == right.volume_serial_number()
            && left.file_index().is_some()
            && left.file_index() == right.file_index())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err(OperationError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory identity checks are unavailable",
        )))
    }
}

impl Read for ManagedFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for ManagedFileReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
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

fn validate_server_relative(path: &Path) -> Result<Vec<OsString>, OperationError> {
    validate_relative(path)?;
    Ok(path
        .components()
        .map(|component| match component {
            Component::Normal(component) => component.to_os_string(),
            _ => unreachable!("validate_relative accepted only normal components"),
        })
        .collect())
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
