use crate::roots::{CapabilitySnapshot, ManagedRoot, ManagedWriteScope};
use cap_primitives::fs::FollowSymlinks;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub(crate) const PROTECTED_TOP_LEVEL: [&str; 3] = [".git", ".codex", ".mcp-agent"];

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("no trustworthy per-user home directory is available for CODEX_HOME")]
    HomeUnavailable,
    #[error("path is outside the fixed workspace")]
    OutsideWorkspace,
    #[error("path targets a protected authority root")]
    ProtectedRoot,
    #[error("path contains an unsupported component")]
    InvalidPath,
    #[error("workspace and global skill roots must not overlap")]
    OverlappingRoots,
    #[error("environment variable {name} must contain a non-empty absolute path")]
    InvalidEnvironment { name: &'static str },
    #[error("release-owned system skills overlap a writable capability root")]
    ReleaseWritableOverlap,
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
    workspace_dir: Dir,
    project_skills: ManagedRoot,
    global_skills: ManagedRoot,
    staging: ManagedRoot,
    global_staging: ManagedRoot,
    capabilities: Option<Arc<CapabilitySnapshot>>,
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
        Self::build(workspace, global_skills, None)
    }

    pub fn from_capabilities(
        capabilities: Arc<CapabilitySnapshot>,
    ) -> Result<Self, AuthorityError> {
        let workspace = capabilities.workspace().to_path_buf();
        let global_skills = capabilities.global_skills().to_path_buf();
        Self::build(workspace, global_skills, Some(capabilities))
    }

    fn build(
        workspace: impl AsRef<Path>,
        global_skills: impl AsRef<Path>,
        capabilities: Option<Arc<CapabilitySnapshot>>,
    ) -> Result<Self, AuthorityError> {
        let workspace = workspace
            .as_ref()
            .canonicalize()
            .map_err(AuthorityError::Setup)?;
        if !workspace.is_dir() {
            return Err(AuthorityError::InvalidPath);
        }
        let global_skills = absolute_lexically(global_skills.as_ref())?;
        if roots_overlap(&workspace, &global_skills) {
            return Err(AuthorityError::OverlappingRoots);
        }

        let workspace_dir = Dir::open_ambient_dir(&workspace, ambient_authority())
            .map_err(AuthorityError::Setup)?;
        let project_skills_path = workspace.join(".agents/skills");
        let project_skills =
            open_or_create_relative_dir(&workspace_dir, Path::new(".agents/skills"))?;
        let staging_path = workspace.join(".mcp-agent/staging");
        let staging = open_or_create_relative_dir(&workspace_dir, Path::new(".mcp-agent/staging"))?;
        let global_dir = open_absolute_dir_no_follow(&global_skills)?;
        let global_skills = global_skills
            .canonicalize()
            .map_err(AuthorityError::Setup)?;
        if roots_overlap(&workspace, &global_skills) {
            return Err(AuthorityError::OverlappingRoots);
        }
        let global_parent = global_skills.parent().ok_or(AuthorityError::InvalidPath)?;
        let global_parent_dir = open_absolute_dir_no_follow(global_parent)?;
        let global_staging_name = global_staging_name(&global_skills);
        let global_staging =
            open_or_create_relative_dir(&global_parent_dir, Path::new(&global_staging_name))?;
        let global_staging_path = global_parent.join(global_staging_name);

        Ok(Self {
            inner: Arc::new(AuthorityInner {
                workspace,
                workspace_dir,
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
                global_staging: ManagedRoot::new(
                    global_staging_path,
                    ManagedWriteScope::ServerStaging,
                    global_staging,
                ),
                capabilities,
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

    #[must_use]
    pub fn global_staging(&self) -> &ManagedRoot {
        &self.inner.global_staging
    }

    #[must_use]
    pub fn capabilities(&self) -> Option<&Arc<CapabilitySnapshot>> {
        self.inner.capabilities.as_ref()
    }

    pub(crate) fn try_clone_workspace_dir(&self) -> std::io::Result<Dir> {
        self.inner.workspace_dir.try_clone()
    }
}

fn global_staging_name(global_skills: &Path) -> String {
    let digest = Sha256::digest(global_skills.to_string_lossy().as_bytes());
    let suffix = digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut suffix, byte| {
            write!(suffix, "{byte:02x}").expect("writing to String cannot fail");
            suffix
        });
    format!(".mcp-agent-skill-staging-{suffix}")
}

fn default_global_skills() -> Result<PathBuf, AuthorityError> {
    configured_global_skills(|name| std::env::var_os(name))
}

fn configured_global_skills<F>(lookup: F) -> Result<PathBuf, AuthorityError>
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    if let Some(codex_home) = lookup("CODEX_HOME") {
        let codex_home = PathBuf::from(codex_home);
        if codex_home.as_os_str().is_empty() || !codex_home.is_absolute() {
            return Err(AuthorityError::InvalidEnvironment { name: "CODEX_HOME" });
        }
        return Ok(codex_home.join("skills"));
    }
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
    Ok(home.join(".codex/skills"))
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
    open_dir_component_no_follow(parent, name).map_err(AuthorityError::Setup)
}

pub(crate) fn open_dir_component_no_follow(parent: &Dir, name: &OsStr) -> std::io::Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(FollowSymlinks::No)
        ._cap_fs_ext_maybe_dir(true);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "path component is not a directory",
        ));
    }
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
        .is_some_and(is_protected_top_level)
}

pub(crate) fn is_protected_top_level(component: &OsStr) -> bool {
    let Some(component) = component.to_str() else {
        return false;
    };
    #[cfg(windows)]
    let component = component.trim_end_matches(['.', ' ']);

    PROTECTED_TOP_LEVEL
        .iter()
        .any(|name| component.eq_ignore_ascii_case(name))
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::{AuthorityError, configured_global_skills, is_protected_top_level, roots_overlap};
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
    fn unix_home_selects_the_codex_skill_root() {
        let global = configured_global_skills(|name| {
            (name == "HOME").then(|| OsString::from("/tmp/test-home"))
        })
        .unwrap();
        assert_eq!(global, PathBuf::from("/tmp/test-home/.codex/skills"));
    }

    #[test]
    fn configured_codex_home_selects_the_skill_root() {
        let global = configured_global_skills(|name| {
            (name == "CODEX_HOME").then(|| OsString::from("/tmp/configured-codex"))
        })
        .unwrap();
        assert_eq!(global, PathBuf::from("/tmp/configured-codex/skills"));
    }

    #[test]
    fn protected_roots_are_ascii_case_insensitive() {
        assert!(is_protected_top_level(OsString::from(".GIT").as_os_str()));
        assert!(is_protected_top_level(OsString::from(".CoDeX").as_os_str()));
        assert!(is_protected_top_level(
            OsString::from(".MCP-AGENT").as_os_str()
        ));
        assert!(!is_protected_top_level(
            OsString::from(".git-data").as_os_str()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn protected_roots_reject_windows_trailing_dot_and_space_aliases() {
        assert!(is_protected_top_level(OsString::from(".git. ").as_os_str()));
        assert!(is_protected_top_level(
            OsString::from(".CODEX...").as_os_str()
        ));
    }

    #[test]
    fn overlapping_roots_include_equal_and_ancestor_paths() {
        let workspace = PathBuf::from("/workspace");
        assert!(roots_overlap(&workspace, &workspace));
        assert!(roots_overlap(&workspace, &workspace.join("global")));
        assert!(roots_overlap(PathBuf::from("/").as_path(), &workspace));
        assert!(!roots_overlap(
            &workspace,
            PathBuf::from("/outside/global").as_path()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_userprofile_selects_the_per_user_skill_root() {
        let global = configured_global_skills(|name| {
            (name == "USERPROFILE").then(|| OsString::from(r"C:\Users\test"))
        })
        .unwrap();
        assert_eq!(global, PathBuf::from(r"C:\Users\test\.codex\skills"));
    }
}
