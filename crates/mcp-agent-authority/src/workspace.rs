use crate::roots::{ManagedRoot, ManagedWriteScope};
use cap_primitives::fs::FollowSymlinks;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub(crate) const PROTECTED_TOP_LEVEL: [&str; 3] = [".git", ".codex", ".mcp-agent"];

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("no trustworthy per-user home directory is available for global skills")]
    HomeUnavailable,
    #[error("path is outside the fixed workspace")]
    OutsideWorkspace,
    #[error("path targets a protected authority root")]
    ProtectedRoot,
    #[error("path contains an unsupported component")]
    InvalidPath,
    #[error("authority setup failed: {0}")]
    Setup(#[source] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct WorkspaceAuthority {
    inner: Arc<AuthorityInner>,
}

#[derive(Debug)]
struct AuthorityInner {
    workspace: PathBuf,
    project_skills: ManagedRoot,
    global_skills: ManagedRoot,
    staging: ManagedRoot,
}

#[derive(Clone, Debug)]
pub struct CommandAuthority {
    inner: Arc<AuthorityInner>,
}

impl WorkspaceAuthority {
    /// Canonicalizes the launch directory exactly once and fixes it for the
    /// lifetime of this authority.
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, AuthorityError> {
        let global = default_global_skills()?;
        Self::with_global_skills(workspace, global)
    }

    pub fn with_global_skills(
        workspace: impl AsRef<Path>,
        global_skills: impl AsRef<Path>,
    ) -> Result<Self, AuthorityError> {
        let workspace = workspace
            .as_ref()
            .canonicalize()
            .map_err(AuthorityError::Setup)?;
        if !workspace.is_dir() {
            return Err(AuthorityError::InvalidPath);
        }

        let workspace_dir = Dir::open_ambient_dir(&workspace, ambient_authority())
            .map_err(AuthorityError::Setup)?;
        let project_skills_path = workspace.join(".agents/skills");
        let project_skills =
            open_or_create_relative_dir(&workspace_dir, Path::new(".agents/skills"))?;
        let staging_path = workspace.join(".mcp-agent/staging");
        let staging = open_or_create_relative_dir(&workspace_dir, Path::new(".mcp-agent/staging"))?;
        let global_skills = absolute_lexically(global_skills.as_ref())?;
        let global_dir = open_absolute_dir_no_follow(&global_skills)?;

        Ok(Self {
            inner: Arc::new(AuthorityInner {
                workspace,
                project_skills: ManagedRoot::new(
                    project_skills_path,
                    ManagedWriteScope::ProjectSkills,
                    project_skills,
                ),
                global_skills: ManagedRoot::new(
                    global_skills,
                    ManagedWriteScope::GlobalSkills,
                    global_dir,
                ),
                staging: ManagedRoot::new(staging_path, ManagedWriteScope::ServerStaging, staging),
            }),
        })
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.inner.workspace
    }

    #[must_use]
    pub fn command(&self) -> CommandAuthority {
        CommandAuthority {
            inner: Arc::clone(&self.inner),
        }
    }

    #[must_use]
    pub fn project_skills(&self) -> &ManagedRoot {
        &self.inner.project_skills
    }

    #[must_use]
    pub fn global_skills(&self) -> &ManagedRoot {
        &self.inner.global_skills
    }

    #[must_use]
    pub fn staging(&self) -> &ManagedRoot {
        &self.inner.staging
    }
}

fn default_global_skills() -> Result<PathBuf, AuthorityError> {
    configured_global_skills(|name| std::env::var_os(name))
}

fn configured_global_skills<F>(lookup: F) -> Result<PathBuf, AuthorityError>
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    #[cfg(unix)]
    let home = lookup("HOME");
    #[cfg(windows)]
    let home = lookup("USERPROFILE").or_else(|| {
        let drive = lookup("HOMEDRIVE")?;
        let path = lookup("HOMEPATH")?;
        Some(PathBuf::from(drive).join(path).into_os_string())
    });
    #[cfg(not(any(unix, windows)))]
    let home = None;

    let home = home
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(AuthorityError::HomeUnavailable)?;
    Ok(home.join(".agents/skills"))
}

impl CommandAuthority {
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.inner.workspace
    }

    pub fn resolve_cwd(&self, requested: &Path) -> Result<PathBuf, AuthorityError> {
        let candidate = self.resolve_candidate(requested)?;
        let canonical = candidate.canonicalize().map_err(AuthorityError::Setup)?;
        if !canonical.starts_with(&self.inner.workspace) {
            return Err(AuthorityError::OutsideWorkspace);
        }
        if is_protected(&self.inner.workspace, &canonical) {
            return Err(AuthorityError::ProtectedRoot);
        }
        Ok(canonical)
    }

    /// Performs the request-level policy check. The native child sandbox is
    /// still authoritative for racing filesystem changes.
    pub fn authorize_write(&self, requested: &Path) -> Result<PathBuf, AuthorityError> {
        let candidate = self.resolve_candidate(requested)?;
        let normalized = normalize_absolute_lexically(&candidate)?;
        if !normalized.starts_with(&self.inner.workspace) {
            return Err(AuthorityError::OutsideWorkspace);
        }
        verify_existing_ancestor(&self.inner.workspace, &normalized)?;
        if is_protected(&self.inner.workspace, &normalized) {
            return Err(AuthorityError::ProtectedRoot);
        }
        Ok(normalized)
    }

    fn resolve_candidate(&self, requested: &Path) -> Result<PathBuf, AuthorityError> {
        if requested.is_absolute() {
            Ok(requested.to_path_buf())
        } else {
            validate_relative(requested)?;
            Ok(self.inner.workspace.join(requested))
        }
    }
}

fn absolute_lexically(path: &Path) -> Result<PathBuf, AuthorityError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(AuthorityError::Setup)?
            .join(path)
    };
    normalize_absolute_lexically(&absolute)
}

fn open_or_create_relative_dir(parent: &Dir, path: &Path) -> Result<Dir, AuthorityError> {
    let mut current = parent.try_clone().map_err(AuthorityError::Setup)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(AuthorityError::InvalidPath);
        };
        current = open_or_create_component(&current, name)?;
    }
    Ok(current)
}

fn open_or_create_component(parent: &Dir, name: &OsStr) -> Result<Dir, AuthorityError> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(AuthorityError::Setup(error)),
    }
    let mut options = OpenOptions::new();
    options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
    let file = parent
        .open_with(name, &options)
        .map_err(AuthorityError::Setup)?;
    Ok(Dir::from_std_file(file.into_std()))
}

fn open_absolute_dir_no_follow(path: &Path) -> Result<Dir, AuthorityError> {
    let mut anchor = PathBuf::new();
    let mut components = Vec::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => {
                anchor.push(component.as_os_str());
                rooted = true;
            }
            Component::Normal(name) if rooted => components.push(name.to_os_string()),
            _ => return Err(AuthorityError::InvalidPath),
        }
    }
    if !rooted {
        return Err(AuthorityError::InvalidPath);
    }
    let mut current =
        Dir::open_ambient_dir(anchor, ambient_authority()).map_err(AuthorityError::Setup)?;
    for component in components {
        current = open_or_create_component(&current, &component)?;
    }
    Ok(current)
}

fn validate_relative(path: &Path) -> Result<(), AuthorityError> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_) | Component::CurDir) {
            return Err(AuthorityError::OutsideWorkspace);
        }
    }
    Ok(())
}

fn normalize_absolute_lexically(path: &Path) -> Result<PathBuf, AuthorityError> {
    if !path.is_absolute() {
        return Err(AuthorityError::InvalidPath);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(AuthorityError::OutsideWorkspace),
        }
    }
    Ok(normalized)
}

fn verify_existing_ancestor(workspace: &Path, path: &Path) -> Result<(), AuthorityError> {
    let mut cursor = path;
    let canonical = loop {
        match cursor.canonicalize() {
            Ok(canonical) => break canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cursor = cursor.parent().ok_or(AuthorityError::OutsideWorkspace)?;
            }
            Err(error) => return Err(AuthorityError::Setup(error)),
        }
    };
    if canonical.starts_with(workspace) {
        Ok(())
    } else {
        Err(AuthorityError::OutsideWorkspace)
    }
}

fn is_protected(workspace: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(workspace) else {
        return false;
    };
    relative
        .components()
        .find_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .is_some_and(|part| {
            PROTECTED_TOP_LEVEL
                .iter()
                .any(|name| part == OsStr::new(name))
        })
}

#[cfg(test)]
mod tests {
    use super::{AuthorityError, configured_global_skills};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn missing_home_fails_closed_without_a_temp_fallback() {
        assert!(matches!(
            configured_global_skills(|_| None),
            Err(AuthorityError::HomeUnavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_home_selects_the_per_user_skill_root() {
        let global = configured_global_skills(|name| {
            (name == "HOME").then(|| OsString::from("/tmp/test-home"))
        })
        .unwrap();
        assert_eq!(global, PathBuf::from("/tmp/test-home/.agents/skills"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_userprofile_selects_the_per_user_skill_root() {
        let global = configured_global_skills(|name| {
            (name == "USERPROFILE").then(|| OsString::from(r"C:\Users\test"))
        })
        .unwrap();
        assert_eq!(global, PathBuf::from(r"C:\Users\test\.agents\skills"));
    }
}
