mod commit;
mod controlled_http;
mod fetch;
mod limits;
mod source;
mod transport;
mod tree;

pub use fetch::GixGitFetcher;
pub use limits::InstallLimits;
pub use source::{NormalizedGitSource, normalize_git_source};
pub use transport::{TransportHop, TransportScript, evaluate_transport_script};
pub use tree::{
    RepositoryEntry, RepositoryEntryKind, validate_object_expansion, validate_pack_expansion,
    validate_repository_tree,
};

use crate::contracts::{
    SkillInstallInput, SkillInstallOutput, SkillListInput, SkillScope, SkillSource,
};
use crate::upstream::parse_skill_frontmatter_metadata;
use crate::{SkillCatalog, roots::is_portable_segment};
use mcp_agent_authority::{ServerOperations, WorkspaceAuthority};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct FetchedRepository {
    pub repository: String,
    pub commit: String,
    pub entries: Vec<RepositoryEntry>,
}

pub trait GitFetcher: std::fmt::Debug + Send + Sync {
    /// Resolves one immutable repository tree.
    ///
    /// # Errors
    ///
    /// Returns an error when source policy, network, revision, or fetch limits
    /// prevent obtaining an immutable tree.
    fn fetch(
        &self,
        source: &NormalizedGitSource,
        limits: &InstallLimits,
    ) -> Result<FetchedRepository, SkillInstallError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SkillInstallError {
    #[error("Git source must be a public HTTPS repository without credentials, query, or fragment")]
    InvalidSource,
    #[error("Git source resolves to a non-public network destination")]
    NonPublicSource,
    #[error("GitHub tree URL conflicts with explicit selector or revision")]
    AmbiguousSource,
    #[error("skill selector is not a portable repository-relative path")]
    InvalidSelector,
    #[error("Git revision is invalid")]
    InvalidRevision,
    #[error("Git tree could not be fetched through the controlled HTTPS transport")]
    FetchFailed,
    #[error("Git source contains no valid skill")]
    NoSkill,
    #[error("Git source contains multiple skills; retry with a candidate selector")]
    MultipleSkills { candidates: Vec<String> },
    #[error("Git tree contains an unsafe path")]
    UnsafePath,
    #[error("Git tree contains a non-regular entry")]
    UnsupportedEntry,
    #[error("Git tree contains a portable path collision")]
    PathCollision,
    #[error("Git source exceeds an installation limit")]
    LimitExceeded,
    #[error("SKILL.md metadata is invalid")]
    InvalidMetadata,
    #[error("skill package name is not portable")]
    InvalidPackageName,
    #[error("skill package already exists in the selected scope")]
    Collision,
    #[error("skill installation commit failed")]
    CommitFailed,
    #[error("skill installation authority setup failed")]
    AuthoritySetup,
}

impl SkillInstallError {
    #[must_use]
    pub fn candidate_selectors(&self) -> &[String] {
        match self {
            Self::MultipleSkills { candidates } => candidates,
            _ => &[],
        }
    }
}

#[derive(Debug)]
pub struct SkillInstaller {
    project: ServerOperations,
    project_staging: ServerOperations,
    global_staging: ServerOperations,
    authority: WorkspaceAuthority,
    catalog: Arc<SkillCatalog>,
    fetcher: Arc<dyn GitFetcher>,
    limits: InstallLimits,
}

impl SkillInstaller {
    /// Creates a production installer using the controlled `gix` fetcher.
    ///
    /// # Errors
    ///
    /// Returns an error when managed root capabilities cannot be opened.
    pub fn new(
        authority: &WorkspaceAuthority,
        catalog: Arc<SkillCatalog>,
    ) -> Result<Self, SkillInstallError> {
        Self::with_fetcher(authority, catalog, Arc::new(GixGitFetcher))
    }

    /// Creates an installer with an injected immutable-tree fetcher.
    ///
    /// # Errors
    ///
    /// Returns an error when managed root capabilities cannot be opened.
    pub fn with_fetcher(
        authority: &WorkspaceAuthority,
        catalog: Arc<SkillCatalog>,
        fetcher: Arc<dyn GitFetcher>,
    ) -> Result<Self, SkillInstallError> {
        let project_staging = ServerOperations::new(authority.staging())
            .map_err(|_| SkillInstallError::AuthoritySetup)?;
        let global_staging = ServerOperations::new(authority.global_staging())
            .map_err(|_| SkillInstallError::AuthoritySetup)?;
        commit::recover_staging(&project_staging).map_err(|_| SkillInstallError::AuthoritySetup)?;
        commit::recover_staging(&global_staging).map_err(|_| SkillInstallError::AuthoritySetup)?;
        Ok(Self {
            project: ServerOperations::new(authority.project_skills())
                .map_err(|_| SkillInstallError::AuthoritySetup)?,
            project_staging,
            global_staging,
            authority: authority.clone(),
            catalog,
            fetcher,
            limits: InstallLimits::default(),
        })
    }

    /// Validates, stages, and atomically installs exactly one skill package.
    ///
    /// # Errors
    ///
    /// Returns a safe install error for invalid sources or trees, ambiguous
    /// candidates, collisions, limits, fetch failures, and commit failures.
    pub fn install(
        &self,
        input: &SkillInstallInput,
    ) -> Result<SkillInstallOutput, SkillInstallError> {
        let source = normalize_git_source(
            &input.repository,
            input.selector.as_deref(),
            input.revision.as_deref(),
        )?;
        let fetched = self.fetcher.fetch(&source, &self.limits)?;
        if fetched.repository != source.repository || !source::is_commit_id(&fetched.commit) {
            return Err(SkillInstallError::FetchFailed);
        }
        validate_repository_tree(&fetched.entries, &self.limits)?;
        let selected = tree::select_package(fetched.entries, source.selector.as_deref())?;
        let entries = selected.entries;
        validate_repository_tree(&entries, &self.limits)?;
        let main = entries
            .iter()
            .find(|entry| entry.path == "SKILL.md")
            .ok_or(SkillInstallError::NoSkill)?;
        let text =
            std::str::from_utf8(&main.bytes).map_err(|_| SkillInstallError::InvalidMetadata)?;
        let default_name = package_default_name(&selected.candidate, &source.repository);
        let metadata = parse_skill_frontmatter_metadata(text, || default_name)
            .map_err(|_| SkillInstallError::InvalidMetadata)?;
        let package = metadata.name.trim().to_string();
        if !is_portable_segment(&package)
            || package.len() > 128
            || package.starts_with(".mcp-agent-")
        {
            return Err(SkillInstallError::InvalidPackageName);
        }
        let provenance_source =
            SkillSource::git(fetched.repository, fetched.commit, source.selector);
        let expected_hash = package_hash(&entries);
        let staging_name = format!(
            ".mcp-agent-install-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let staging = self.staging(input.scope);
        let _scope_lock = commit::acquire_scope_lock(staging)?;
        self.revalidate_destination(input.scope)?;
        self.reject_canonical_collision(input.scope, &metadata.name)?;
        let staged = commit::materialize_package(staging, &staging_name, &entries)?;
        let staged_hash = match staged_package_hash(&staged, &entries) {
            Ok(hash) => hash,
            Err(error) => {
                let _ = commit::discard_package(staging, &staging_name);
                return Err(error);
            }
        };
        if staged_hash != expected_hash {
            let _ = commit::discard_package(staging, &staging_name);
            return Err(SkillInstallError::CommitFailed);
        }
        let destination = match self.revalidated_destination(input.scope) {
            Ok(destination) => destination,
            Err(error) => {
                let _ = commit::discard_package(staging, &staging_name);
                return Err(error);
            }
        };
        if let Err(error) = self.reject_canonical_collision(input.scope, &metadata.name) {
            let _ = commit::discard_package(staging, &staging_name);
            return Err(error);
        }
        if let Err(error) = commit::commit_package(staging, &staging_name, &destination, &package) {
            let _ = commit::discard_package(staging, &staging_name);
            return Err(error);
        }
        self.catalog.invalidate_after_install();
        Ok(SkillInstallOutput::new(
            input.scope,
            package,
            metadata.name,
            provenance_source,
        ))
    }

    fn staging(&self, scope: SkillScope) -> &ServerOperations {
        match scope {
            SkillScope::Project => &self.project_staging,
            SkillScope::Global => &self.global_staging,
        }
    }

    fn revalidate_destination(&self, scope: SkillScope) -> Result<(), SkillInstallError> {
        self.revalidated_destination(scope).map(|_| ())
    }

    fn revalidated_destination(
        &self,
        scope: SkillScope,
    ) -> Result<ServerOperations, SkillInstallError> {
        match scope {
            SkillScope::Project => self
                .project
                .revalidate_project_root(&self.authority)
                .map_err(|_| SkillInstallError::CommitFailed),
            SkillScope::Global => ServerOperations::new(self.authority.global_skills())
                .map_err(|_| SkillInstallError::CommitFailed),
        }
    }

    fn reject_canonical_collision(
        &self,
        scope: SkillScope,
        name: &str,
    ) -> Result<(), SkillInstallError> {
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list(&SkillListInput {
                    scope,
                    cursor: cursor.take(),
                })
                .map_err(|_| SkillInstallError::CommitFailed)?;
            if page.skills.iter().any(|entry| entry.name == name) {
                return Err(SkillInstallError::Collision);
            }
            let Some(next_cursor) = page.next_cursor else {
                return Ok(());
            };
            cursor = Some(next_cursor);
        }
    }
}

fn package_default_name(candidate: &str, repository: &str) -> String {
    let candidate_name = candidate
        .rsplit('/')
        .find(|part| !part.is_empty())
        .map(str::to_string);
    let repository_name = || {
        let url = url::Url::parse(repository).ok()?;
        url.path_segments()?.next_back().map(str::to_string)
    };
    candidate_name
        .or_else(repository_name)
        .unwrap_or_default()
        .trim_end_matches(".git")
        .to_string()
}

fn staged_package_hash(
    staged: &ServerOperations,
    entries: &[RepositoryEntry],
) -> Result<[u8; 32], SkillInstallError> {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hash = Sha256::new();
    for entry in entries {
        let bytes = staged
            .read_bytes(std::path::Path::new(&entry.path))
            .map_err(|_| SkillInstallError::CommitFailed)?;
        update_package_hash(&mut hash, &entry.path, &bytes);
    }
    Ok(hash.finalize().into())
}

fn package_hash(entries: &[RepositoryEntry]) -> [u8; 32] {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hash = Sha256::new();
    for entry in entries {
        update_package_hash(&mut hash, &entry.path, &entry.bytes);
    }
    hash.finalize().into()
}

fn update_package_hash(hash: &mut Sha256, path: &str, bytes: &[u8]) {
    hash.update(path.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    hash.update([0]);
}
