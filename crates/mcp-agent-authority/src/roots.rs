use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::AuthorityError;

/// A server-owned filesystem root. Possessing this value is the authority to
/// perform managed writes beneath the root; command children never receive it.
#[derive(Debug)]
pub struct ManagedRoot {
    root: PathBuf,
    scope: ManagedWriteScope,
    dir: Dir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWriteScope {
    ProjectSkills,
    GlobalSkills,
    ServerStaging,
}

/// Canonical startup paths and workload environment fixed for the server's
/// lifetime. The native sandbox consumes `writable_roots`; child environment
/// strings are only a usability bridge and never grant authority.
#[derive(Clone, Debug)]
pub struct CapabilitySnapshot {
    workspace: PathBuf,
    canonical_tmp: PathBuf,
    tmpdir: PathBuf,
    codex_home: PathBuf,
    global_skills: PathBuf,
    workspace_cache: PathBuf,
    cargo_home: PathBuf,
    gradle_user_home: PathBuf,
    system_skills: PathBuf,
    writable_roots: Vec<PathBuf>,
    environment: BTreeMap<String, OsString>,
}

impl CapabilitySnapshot {
    /// Resolves, creates, and canonicalizes every managed root before command
    /// service begins.
    pub fn resolve(
        workspace: impl AsRef<Path>,
        system_skills: impl AsRef<Path>,
    ) -> Result<Self, AuthorityError> {
        Self::resolve_with(
            workspace,
            system_skills,
            ResolveEnvironment::new(
                std::env::var_os,
                PathBuf::from("/tmp"),
                std::env::temp_dir(),
            ),
        )
    }

    /// Deterministic resolution seam for embedders that already captured an
    /// environment snapshot. Ordinary server startup should use [`Self::resolve`].
    #[doc(hidden)]
    pub fn resolve_configured<F>(
        workspace: impl AsRef<Path>,
        system_skills: impl AsRef<Path>,
        lookup: F,
        canonical_tmp: PathBuf,
        fallback_tmp: PathBuf,
    ) -> Result<Self, AuthorityError>
    where
        F: Fn(&'static str) -> Option<OsString>,
    {
        Self::resolve_with(
            workspace,
            system_skills,
            ResolveEnvironment::new(lookup, canonical_tmp, fallback_tmp),
        )
    }

    fn resolve_with<F>(
        workspace: impl AsRef<Path>,
        system_skills: impl AsRef<Path>,
        resolver: ResolveEnvironment<F>,
    ) -> Result<Self, AuthorityError>
    where
        F: Fn(&'static str) -> Option<OsString>,
    {
        let workspace = canonical_directory(workspace.as_ref(), false)?;
        let home = optional_absolute(&resolver.lookup, "HOME")?;
        let codex_home = match (resolver.lookup)("CODEX_HOME") {
            Some(value) => validate_absolute_value(value, "CODEX_HOME")?,
            None => home
                .as_deref()
                .ok_or(AuthorityError::HomeUnavailable)?
                .join(".codex"),
        };
        let effective_tmp = match (resolver.lookup)("TMPDIR") {
            Some(value) if !value.is_empty() => validate_absolute_value(value, "TMPDIR")?,
            _ => resolver.fallback_tmp,
        };
        let cargo_home = effective_tool_home(
            &resolver.lookup,
            "CARGO_HOME",
            home.as_deref().map(|home| home.join(".cargo")),
            &workspace,
        )?;
        let gradle_user_home = effective_tool_home(
            &resolver.lookup,
            "GRADLE_USER_HOME",
            home.as_deref().map(|home| home.join(".gradle")),
            &workspace,
        )?;

        let canonical_tmp = canonical_directory(&resolver.canonical_tmp, false)?;
        let tmpdir = canonical_directory(&effective_tmp, true)?;
        let codex_home = canonical_directory(&codex_home, true)?;
        let global_skills = canonical_directory(&codex_home.join("skills"), true)?;
        let workspace_cache = canonical_directory(
            &codex_home
                .join("cache/tools-mcp/workspaces")
                .join(workspace_digest(&workspace)),
            true,
        )?;
        let cargo_home = canonical_directory(&cargo_home, true)?;
        let gradle_user_home = canonical_directory(&gradle_user_home, true)?;
        let system_skills = canonical_directory(system_skills.as_ref(), false)?;

        let writable_roots = dedupe_roots([
            workspace.clone(),
            canonical_tmp.clone(),
            tmpdir.clone(),
            workspace_cache.clone(),
            global_skills.clone(),
            cargo_home.clone(),
            gradle_user_home.clone(),
        ]);
        if writable_roots
            .iter()
            .any(|root| roots_overlap(root, &system_skills))
        {
            return Err(AuthorityError::ReleaseWritableOverlap);
        }

        let mut environment = BTreeMap::new();
        insert_path(&mut environment, "CODEX_HOME", &codex_home)?;
        insert_path(&mut environment, "TMPDIR", &tmpdir)?;
        insert_path(
            &mut environment,
            "MCP_AGENT_SYSTEM_SKILLS_ROOT",
            &system_skills,
        )?;
        insert_path(&mut environment, "CARGO_HOME", &cargo_home)?;
        insert_path(&mut environment, "GRADLE_USER_HOME", &gradle_user_home)?;
        for (name, suffix) in [
            ("MCP_AGENT_CACHE_HOME", ""),
            ("XDG_CACHE_HOME", "xdg"),
            ("CARGO_TARGET_DIR", "cargo-target"),
            ("npm_config_cache", "npm"),
            ("YARN_CACHE_FOLDER", "yarn"),
            ("npm_config_store_dir", "pnpm-store"),
            ("PIP_CACHE_DIR", "pip"),
            ("UV_CACHE_DIR", "uv"),
            ("GOCACHE", "go-build"),
            ("GOMODCACHE", "go-mod"),
        ] {
            let value = if suffix.is_empty() {
                workspace_cache.clone()
            } else {
                workspace_cache.join(suffix)
            };
            insert_path(&mut environment, name, &value)?;
        }

        Ok(Self {
            workspace,
            canonical_tmp,
            tmpdir,
            codex_home,
            global_skills,
            workspace_cache,
            cargo_home,
            gradle_user_home,
            system_skills,
            writable_roots,
            environment,
        })
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    #[must_use]
    pub fn canonical_tmp(&self) -> &Path {
        &self.canonical_tmp
    }
    #[must_use]
    pub fn tmpdir(&self) -> &Path {
        &self.tmpdir
    }
    #[must_use]
    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }
    #[must_use]
    pub fn global_skills(&self) -> &Path {
        &self.global_skills
    }
    #[must_use]
    pub fn workspace_cache(&self) -> &Path {
        &self.workspace_cache
    }
    #[must_use]
    pub fn cargo_home(&self) -> &Path {
        &self.cargo_home
    }
    #[must_use]
    pub fn gradle_user_home(&self) -> &Path {
        &self.gradle_user_home
    }
    #[must_use]
    pub fn system_skills(&self) -> &Path {
        &self.system_skills
    }
    #[must_use]
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, OsString> {
        &self.environment
    }
}

struct ResolveEnvironment<F> {
    lookup: F,
    canonical_tmp: PathBuf,
    fallback_tmp: PathBuf,
}

impl<F> ResolveEnvironment<F> {
    fn new(lookup: F, canonical_tmp: PathBuf, fallback_tmp: PathBuf) -> Self {
        Self {
            lookup,
            canonical_tmp,
            fallback_tmp,
        }
    }
}

fn optional_absolute<F>(lookup: &F, name: &'static str) -> Result<Option<PathBuf>, AuthorityError>
where
    F: Fn(&'static str) -> Option<OsString>,
{
    lookup(name)
        .map(|value| validate_absolute_value(value, name))
        .transpose()
}

fn validate_absolute_value(value: OsString, name: &'static str) -> Result<PathBuf, AuthorityError> {
    let value = PathBuf::from(value);
    if value.as_os_str().is_empty() || !value.is_absolute() {
        return Err(AuthorityError::InvalidEnvironment { name });
    }
    Ok(value)
}

fn effective_tool_home<F>(
    lookup: &F,
    name: &'static str,
    default: Option<PathBuf>,
    workspace: &Path,
) -> Result<PathBuf, AuthorityError>
where
    F: Fn(&'static str) -> Option<OsString>,
{
    let Some(value) = lookup(name).filter(|value| !value.is_empty()) else {
        return default.ok_or(AuthorityError::HomeUnavailable);
    };
    let value = PathBuf::from(value);
    Ok(if value.is_absolute() {
        value
    } else {
        workspace.join(value)
    })
}

fn canonical_directory(path: &Path, create: bool) -> Result<PathBuf, AuthorityError> {
    if create {
        fs::create_dir_all(path).map_err(AuthorityError::Setup)?;
    }
    let canonical = path.canonicalize().map_err(AuthorityError::Setup)?;
    if !canonical.is_dir() {
        return Err(AuthorityError::InvalidPath);
    }
    Ok(canonical)
}

fn dedupe_roots(roots: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort_by_key(|root| root.components().count());
    roots.dedup();
    roots.into_iter().fold(Vec::new(), |mut deduped, root| {
        if !deduped.iter().any(|existing| root.starts_with(existing)) {
            deduped.push(root);
        }
        deduped
    })
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn insert_path(
    environment: &mut BTreeMap<String, OsString>,
    name: &'static str,
    path: &Path,
) -> Result<(), AuthorityError> {
    if path.to_str().is_none() {
        return Err(AuthorityError::InvalidEnvironment { name });
    }
    environment.insert(name.to_owned(), path.as_os_str().to_owned());
    Ok(())
}

fn workspace_digest(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(workspace.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(workspace.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

impl ManagedRoot {
    pub(crate) fn new(root: PathBuf, scope: ManagedWriteScope, dir: Dir) -> Self {
        Self { root, scope, dir }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn scope(&self) -> ManagedWriteScope {
        self.scope
    }

    pub(crate) fn try_clone_dir(&self) -> std::io::Result<Dir> {
        self.dir.try_clone()
    }
}

#[cfg(test)]
mod capability_tests {
    use super::{CapabilitySnapshot, ResolveEnvironment};
    use crate::AuthorityError;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct Fixture {
        root: tempfile::TempDir,
        workspace: PathBuf,
        system_skills: PathBuf,
        home: PathBuf,
        tmp: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let release = root.path().join("release");
            let system_skills = release.join("system-skills");
            let home = root.path().join("home");
            let tmp = root.path().join("tmp");
            for path in [&workspace, &system_skills, &home, &tmp] {
                fs::create_dir_all(path).unwrap();
            }
            Self {
                root,
                workspace,
                system_skills,
                home,
                tmp,
            }
        }

        fn resolve(
            &self,
            values: &[(&str, PathBuf)],
        ) -> Result<CapabilitySnapshot, AuthorityError> {
            let mut environment = BTreeMap::<String, OsString>::new();
            environment.insert("HOME".to_owned(), self.home.clone().into_os_string());
            for (name, value) in values {
                environment.insert((*name).to_owned(), value.clone().into_os_string());
            }
            CapabilitySnapshot::resolve_with(
                &self.workspace,
                &self.system_skills,
                ResolveEnvironment::new(
                    |name| environment.get(name).cloned(),
                    self.tmp.clone(),
                    self.tmp.clone(),
                ),
            )
        }
    }

    #[test]
    fn codex_home_defaults_to_dot_codex_and_never_dot_agents() {
        let fixture = Fixture::new();
        let snapshot = fixture.resolve(&[]).unwrap();

        assert_eq!(
            snapshot.codex_home(),
            fixture.home.join(".codex").canonicalize().unwrap()
        );
        assert_eq!(
            snapshot.global_skills(),
            fixture.home.join(".codex/skills").canonicalize().unwrap()
        );
        assert!(
            !snapshot
                .global_skills()
                .ends_with(Path::new(".agents/skills"))
        );
        assert!(snapshot.global_skills().is_dir());
    }

    #[test]
    fn workspace_cache_is_stable_partitioned_and_steers_only_cache_state() {
        let fixture = Fixture::new();
        let cargo = fixture.home.join("configured-cargo");
        let gradle = fixture.home.join("configured-gradle");
        fs::create_dir_all(&cargo).unwrap();
        fs::write(cargo.join("config.toml"), "[net]\noffline = true\n").unwrap();
        fs::create_dir_all(&gradle).unwrap();
        fs::write(gradle.join("init.gradle"), "// retained\n").unwrap();

        let first = fixture
            .resolve(&[
                ("CARGO_HOME", cargo.clone()),
                ("GRADLE_USER_HOME", gradle.clone()),
            ])
            .unwrap();
        let second = fixture
            .resolve(&[
                ("CARGO_HOME", cargo.clone()),
                ("GRADLE_USER_HOME", gradle.clone()),
            ])
            .unwrap();

        assert_eq!(first.workspace_cache(), second.workspace_cache());
        assert!(
            first.workspace_cache().starts_with(
                fixture
                    .home
                    .join(".codex")
                    .canonicalize()
                    .unwrap()
                    .join("cache/tools-mcp/workspaces")
            )
        );
        assert_eq!(first.cargo_home(), cargo.canonicalize().unwrap());
        assert_eq!(first.gradle_user_home(), gradle.canonicalize().unwrap());
        assert_eq!(
            fs::read_to_string(first.cargo_home().join("config.toml")).unwrap(),
            "[net]\noffline = true\n"
        );
        assert_eq!(
            fs::read_to_string(first.gradle_user_home().join("init.gradle")).unwrap(),
            "// retained\n"
        );
        assert_eq!(
            first
                .environment()
                .get("CARGO_HOME")
                .map(OsString::as_os_str),
            Some(first.cargo_home().as_os_str())
        );
        assert_eq!(
            first
                .environment()
                .get("GRADLE_USER_HOME")
                .map(OsString::as_os_str),
            Some(first.gradle_user_home().as_os_str())
        );
        assert!(!first.environment().contains_key("HOME"));
        assert!(!first.environment().contains_key("PATH"));
        assert_eq!(
            first
                .environment()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "CARGO_HOME",
                "CARGO_TARGET_DIR",
                "CODEX_HOME",
                "GOCACHE",
                "GOMODCACHE",
                "GRADLE_USER_HOME",
                "MCP_AGENT_CACHE_HOME",
                "MCP_AGENT_SYSTEM_SKILLS_ROOT",
                "PIP_CACHE_DIR",
                "TMPDIR",
                "UV_CACHE_DIR",
                "XDG_CACHE_HOME",
                "YARN_CACHE_FOLDER",
                "npm_config_cache",
                "npm_config_store_dir",
            ]
        );
    }

    #[test]
    fn aliases_and_nested_writable_roots_are_canonicalized_and_deduplicated() {
        let fixture = Fixture::new();
        let nested_tmp = fixture.workspace.join("nested-tmp");
        fs::create_dir_all(&nested_tmp).unwrap();
        let snapshot = fixture
            .resolve(&[("TMPDIR", nested_tmp.join("."))])
            .unwrap();

        assert_eq!(snapshot.tmpdir(), nested_tmp.canonicalize().unwrap());
        let workspace = fixture.workspace.canonicalize().unwrap();
        assert_eq!(
            snapshot
                .writable_roots()
                .iter()
                .filter(|root| root.starts_with(&workspace))
                .count(),
            1
        );
        for (index, root) in snapshot.writable_roots().iter().enumerate() {
            assert!(
                snapshot.writable_roots()[..index]
                    .iter()
                    .all(|earlier| !root.starts_with(earlier) && !earlier.starts_with(root))
            );
        }
    }

    #[test]
    fn invalid_codex_home_and_release_overlap_fail_closed() {
        let fixture = Fixture::new();
        assert!(matches!(
            fixture.resolve(&[("CODEX_HOME", PathBuf::new())]),
            Err(AuthorityError::InvalidEnvironment { name: "CODEX_HOME" })
        ));
        assert!(matches!(
            fixture.resolve(&[("CODEX_HOME", PathBuf::from("relative"))]),
            Err(AuthorityError::InvalidEnvironment { name: "CODEX_HOME" })
        ));
        assert!(matches!(
            fixture.resolve(&[("TMPDIR", PathBuf::from("relative"))]),
            Err(AuthorityError::InvalidEnvironment { name: "TMPDIR" })
        ));

        let no_home = BTreeMap::<String, OsString>::from([
            (
                "CODEX_HOME".to_owned(),
                fixture.home.join("without-home").into_os_string(),
            ),
            (
                "CARGO_HOME".to_owned(),
                fixture.home.join("cargo-without-home").into_os_string(),
            ),
            (
                "GRADLE_USER_HOME".to_owned(),
                fixture.home.join("gradle-without-home").into_os_string(),
            ),
        ]);
        CapabilitySnapshot::resolve_with(
            &fixture.workspace,
            &fixture.system_skills,
            ResolveEnvironment::new(
                |name| no_home.get(name).cloned(),
                fixture.tmp.clone(),
                fixture.tmp.clone(),
            ),
        )
        .unwrap();

        let mut environment = BTreeMap::<String, OsString>::new();
        environment.insert("HOME".to_owned(), fixture.home.clone().into_os_string());
        let overlapping_system_skills = fixture.workspace.join("system-skills");
        fs::create_dir(&overlapping_system_skills).unwrap();
        let result = CapabilitySnapshot::resolve_with(
            &fixture.workspace,
            &overlapping_system_skills,
            ResolveEnvironment::new(
                |name| environment.get(name).cloned(),
                fixture.tmp.clone(),
                fixture.tmp.clone(),
            ),
        );
        assert!(matches!(
            result,
            Err(AuthorityError::ReleaseWritableOverlap)
        ));

        let equal = Fixture::new();
        let codex_home = equal.home.join("system-codex");
        let initial = equal
            .resolve(&[("CODEX_HOME", codex_home.clone())])
            .unwrap();
        let equal_system_skills = initial.global_skills().to_path_buf();
        let environment = BTreeMap::<String, OsString>::from([
            ("HOME".to_owned(), equal.home.clone().into_os_string()),
            ("CODEX_HOME".to_owned(), codex_home.into_os_string()),
        ]);
        let result = CapabilitySnapshot::resolve_with(
            &equal.workspace,
            equal_system_skills,
            ResolveEnvironment::new(
                |name| environment.get(name).cloned(),
                equal.tmp.clone(),
                equal.tmp.clone(),
            ),
        );
        assert!(matches!(
            result,
            Err(AuthorityError::ReleaseWritableOverlap)
        ));

        let containing = Fixture::new();
        let environment = BTreeMap::<String, OsString>::from([(
            "HOME".to_owned(),
            containing.home.clone().into_os_string(),
        )]);
        let result = CapabilitySnapshot::resolve_with(
            &containing.workspace,
            containing.root.path(),
            ResolveEnvironment::new(
                |name| environment.get(name).cloned(),
                containing.tmp.clone(),
                containing.tmp.clone(),
            ),
        );
        assert!(matches!(
            result,
            Err(AuthorityError::ReleaseWritableOverlap)
        ));
    }

    #[test]
    fn different_workspaces_receive_distinct_cache_partitions() {
        let first = Fixture::new();
        let second_workspace = first.root.path().join("second-workspace");
        fs::create_dir(&second_workspace).unwrap();
        let first_snapshot = first.resolve(&[]).unwrap();
        let environment = ResolveEnvironment::new(
            |name| (name == "HOME").then(|| first.home.clone().into_os_string()),
            first.tmp.clone(),
            first.tmp.clone(),
        );
        let second_snapshot =
            CapabilitySnapshot::resolve_with(second_workspace, &first.system_skills, environment)
                .unwrap();
        assert_ne!(
            first_snapshot.workspace_cache(),
            second_snapshot.workspace_cache()
        );
    }
}
