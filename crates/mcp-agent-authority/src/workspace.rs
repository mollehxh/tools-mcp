use crate::operations::{OperationError, ServerOperations, same_directory};
use crate::roots::{CapabilitySnapshot, ManagedRoot, ManagedWriteScope};
use cap_primitives::fs::FollowSymlinks;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

// Retained for non-macOS backend compatibility. Workspace request authority
// and the macOS workload sandbox deliberately do not special-case these names.
pub(crate) const PROTECTED_TOP_LEVEL: [&str; 3] = [".git", ".codex", ".mcp-agent"];

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("no trustworthy per-user home directory is available for CODEX_HOME")]
    HomeUnavailable,
    #[error("path is outside the fixed workspace")]
    OutsideWorkspace,
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
    project_skill_anchor: SkillRootAnchor,
    global_skill_anchor: SkillRootAnchor,
    system_skill_anchor: Option<SkillRootAnchor>,
    capabilities: Option<Arc<CapabilitySnapshot>>,
}

#[derive(Debug)]
struct SkillRootAnchor {
    anchor: Dir,
    components: Vec<std::ffi::OsString>,
    identity_checks: Vec<(Vec<std::ffi::OsString>, Dir)>,
}

impl SkillRootAnchor {
    fn new(
        anchor: Dir,
        components: Vec<std::ffi::OsString>,
        identity_checks: Vec<(Vec<std::ffi::OsString>, Dir)>,
    ) -> Self {
        Self {
            anchor,
            components,
            identity_checks,
        }
    }

    fn reopen(&self) -> Result<ServerOperations, OperationError> {
        let mut current = self.anchor.try_clone()?;
        for component in &self.components {
            current = open_dir_component_no_follow(&current, component)?;
        }
        for (components, expected) in &self.identity_checks {
            let mut observed = current.try_clone()?;
            for component in components {
                observed = open_dir_component_no_follow(&observed, component)?;
            }
            if !same_directory(expected, &observed)? {
                return Err(OperationError::InvalidPath);
            }
        }
        Ok(ServerOperations::from_dir(current))
    }
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
        let global_is_nested_workspace = global_skills.starts_with(&workspace);
        if roots_overlap(&workspace, &global_skills)
            && !(capabilities.is_some() && global_is_nested_workspace)
        {
            return Err(AuthorityError::OverlappingRoots);
        }

        let workspace_dir = Dir::open_ambient_dir(&workspace, ambient_authority())
            .map_err(AuthorityError::Setup)?;
        let project_skills_path = workspace.join(".agents/skills");
        if roots_overlap(&project_skills_path, &global_skills) {
            return Err(AuthorityError::OverlappingRoots);
        }
        let project_skills =
            open_or_create_relative_dir(&workspace_dir, Path::new(".agents/skills"))?;
        let global_dir = open_absolute_dir_no_follow(&global_skills)?;
        let global_skills = global_skills
            .canonicalize()
            .map_err(AuthorityError::Setup)?;
        if roots_overlap(&workspace, &global_skills) && !global_is_nested_workspace {
            return Err(AuthorityError::OverlappingRoots);
        }
        let global_parent = global_skills.parent().ok_or(AuthorityError::InvalidPath)?;
        let global_parent_dir = open_absolute_dir_no_follow(global_parent)?;
        let project_skill_anchor = SkillRootAnchor::new(
            workspace_dir.try_clone().map_err(AuthorityError::Setup)?,
            vec![
                OsStr::new(".agents").to_os_string(),
                OsStr::new("skills").to_os_string(),
            ],
            Vec::new(),
        );
        let global_skill_anchor = if global_is_nested_workspace {
            SkillRootAnchor::new(
                workspace_dir.try_clone().map_err(AuthorityError::Setup)?,
                relative_components(&workspace, &global_skills)?,
                Vec::new(),
            )
        } else {
            SkillRootAnchor::new(
                global_parent_dir
                    .try_clone()
                    .map_err(AuthorityError::Setup)?,
                vec![
                    global_skills
                        .file_name()
                        .ok_or(AuthorityError::InvalidPath)?
                        .to_os_string(),
                ],
                Vec::new(),
            )
        };
        let system_skill_anchor = capabilities
            .as_ref()
            .map(|snapshot| system_skill_anchor(snapshot.system_skills()))
            .transpose()?;

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
                project_skill_anchor,
                global_skill_anchor,
                system_skill_anchor,
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
    pub fn capabilities(&self) -> Option<&CapabilitySnapshot> {
        self.inner.capabilities.as_deref()
    }

    pub fn open_project_skills(&self) -> Result<ServerOperations, OperationError> {
        self.inner.project_skill_anchor.reopen()
    }

    pub fn open_global_skills(&self) -> Result<ServerOperations, OperationError> {
        self.inner.global_skill_anchor.reopen()
    }

    pub fn open_system_skills(&self) -> Result<Option<ServerOperations>, OperationError> {
        self.inner
            .system_skill_anchor
            .as_ref()
            .map(SkillRootAnchor::reopen)
            .transpose()
    }

    pub(crate) fn try_clone_workspace_dir(&self) -> std::io::Result<Dir> {
        self.inner.workspace_dir.try_clone()
    }
}

fn relative_components(
    base: &Path,
    path: &Path,
) -> Result<Vec<std::ffi::OsString>, AuthorityError> {
    path.strip_prefix(base)
        .map_err(|_| AuthorityError::InvalidPath)?
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(AuthorityError::InvalidPath),
        })
        .collect()
}

fn system_skill_anchor(path: &Path) -> Result<SkillRootAnchor, AuthorityError> {
    let parent = path.parent().ok_or(AuthorityError::InvalidPath)?;
    let name = path.file_name().ok_or(AuthorityError::InvalidPath)?;
    let anchor = open_absolute_dir_no_follow(parent)?;
    let expected_root =
        open_dir_component_no_follow(&anchor, name).map_err(AuthorityError::Setup)?;
    let mut identity_checks = vec![(
        Vec::new(),
        expected_root.try_clone().map_err(AuthorityError::Setup)?,
    )];
    match open_dir_component_no_follow(&expected_root, OsStr::new("skill-installer")) {
        Ok(expected_package) => identity_checks.push((
            vec![OsStr::new("skill-installer").to_os_string()],
            expected_package,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(AuthorityError::Setup(error)),
    }
    Ok(SkillRootAnchor::new(
        anchor,
        vec![name.to_os_string()],
        identity_checks,
    ))
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

pub(crate) fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::{AuthorityError, configured_global_skills, roots_overlap};
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
