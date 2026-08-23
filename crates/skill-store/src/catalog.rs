use crate::contracts::{
    ListedSkill, SkillListInput, SkillListOutput, SkillReadInput, SkillReadOutput, SkillScope,
};
use crate::cursor::{pagination_cursor, parse_pagination_cursor};
use crate::precedence;
use crate::resource::{
    MAX_RESPONSE_BYTES, page_response_from_reader, relative_resource_path, serialized_len,
};
use crate::roots::{ScopeSnapshot, SkillRoots};
use mcp_agent_authority::WorkspaceAuthority;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SKILLS_PER_PAGE: usize = 20;
const MAX_WARNINGS_PER_PAGE: usize = 4;
const MAX_WARNING_BYTES: usize = 256;
const OVERSIZED_ENTRY_WARNING: &str =
    "Some skills were omitted because their metadata is too large.";

#[derive(Debug, thiserror::Error)]
pub enum SkillStoreError {
    #[error("skill catalog authority setup failed")]
    AuthoritySetup,
    #[error("{tool} cursor is invalid")]
    InvalidCursor { tool: &'static str },
    #[error("{tool} cursor is stale; restart from the first page")]
    StaleCursor { tool: &'static str },
    #[error("{field} must be non-empty, contain no control characters, and be at most 2048 bytes")]
    InvalidHandle { field: &'static str },
    #[error("skill package is not available from the requested authority")]
    PackageUnavailable,
    #[error("skill resource handle is invalid or outside its package")]
    InvalidResource,
    #[error("failed to read skill resource")]
    ReadFailed,
    #[error("skill resource is not valid UTF-8")]
    InvalidUtf8,
    #[error("skill metadata is too large to list")]
    MetadataTooLarge,
    #[error("skill resource handle leaves no room for contents")]
    ResponseTooLarge,
    #[error("failed to serialize skill result: {0}")]
    Serialization(#[source] serde_json::Error),
    #[error("skill catalog state is unavailable")]
    StateUnavailable,
    #[error("packaged system skills failed identity or digest verification")]
    SystemVerification,
}

#[derive(Debug, Default)]
struct CatalogState {
    system: ScopeSnapshot,
    project: ScopeSnapshot,
    global: ScopeSnapshot,
}

#[derive(Debug)]
pub struct SkillCatalog {
    roots: SkillRoots,
    state: Mutex<CatalogState>,
    generation: AtomicU64,
}

impl SkillCatalog {
    /// Builds the startup catalog from the fixed authority roots.
    ///
    /// # Errors
    ///
    /// Returns an error when the authority capabilities cannot be cloned or
    /// the catalog state cannot be initialized.
    pub fn new(authority: &WorkspaceAuthority) -> Result<Self, SkillStoreError> {
        let roots = SkillRoots::new(authority);
        let catalog = Self {
            roots,
            state: Mutex::new(CatalogState::default()),
            generation: AtomicU64::new(0),
        };
        catalog.reconcile()?;
        Ok(catalog)
    }

    /// Lists one page from the requested host scope.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or stale cursors, unavailable catalog
    /// state, or a response that cannot fit the pinned limit.
    pub fn list(&self, input: &SkillListInput) -> Result<SkillListOutput, SkillStoreError> {
        self.reconcile_scope(input.scope)?;
        let state = self
            .state
            .lock()
            .map_err(|_| SkillStoreError::StateUnavailable)?;
        let snapshot = snapshot(&state, input.scope);
        let mut omitted_oversized_entry = false;
        let skills = snapshot
            .entries
            .iter()
            .filter_map(|entry| {
                let bounded = single_entry_response_is_bounded(entry);
                omitted_oversized_entry |= !bounded;
                bounded.then(|| entry.clone())
            })
            .collect::<Vec<_>>();
        let start = parse_pagination_cursor(input.cursor.as_deref(), &skills, "skills.list")?;
        if start > skills.len() {
            return Err(SkillStoreError::InvalidCursor {
                tool: "skills.list",
            });
        }
        let mut warnings = if start == 0 {
            bounded_warnings(&snapshot.warnings, omitted_oversized_entry)
        } else {
            Vec::new()
        };
        let mut end = (start + MAX_SKILLS_PER_PAGE).min(skills.len());
        loop {
            let response = SkillListOutput {
                skills: skills[start..end].to_vec(),
                warnings: warnings.clone(),
                next_cursor: (end < skills.len()).then(|| pagination_cursor(&skills, end)),
            };
            if serialized_len(&response)? <= MAX_RESPONSE_BYTES {
                return Ok(response);
            }
            if end.saturating_sub(start) > 1 {
                end -= 1;
            } else if !warnings.is_empty() {
                warnings.clear();
            } else {
                return Err(SkillStoreError::MetadataTooLarge);
            }
        }
    }

    /// Reads one page from an exact package-relative logical resource.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid handles or cursors, missing packages,
    /// unsafe resources, failed no-follow reads, or non-UTF-8 contents.
    pub fn read(&self, input: &SkillReadInput) -> Result<SkillReadOutput, SkillStoreError> {
        crate::resource::validate_handle("package", &input.package)?;
        crate::resource::validate_handle("resource", &input.resource)?;
        self.reconcile_scope(input.scope)?;
        self.read_reconciled(input)
    }

    fn read_reconciled(&self, input: &SkillReadInput) -> Result<SkillReadOutput, SkillStoreError> {
        let relative = relative_resource_path(input.scope, &input.package, &input.resource)?;
        let state = self
            .state
            .lock()
            .map_err(|_| SkillStoreError::StateUnavailable)?;
        let scope = snapshot(&state, input.scope);
        if !scope.contains_package(&input.package) {
            return Err(SkillStoreError::PackageUnavailable);
        }
        let mut file = scope
            .open_resource(&input.package, &relative)
            .map_err(|()| SkillStoreError::ReadFailed)?;
        drop(state);
        page_response_from_reader(&input.resource, &mut file, input.cursor.as_deref())
    }

    #[cfg(test)]
    fn read_after_reconcile_for_test(
        &self,
        input: &SkillReadInput,
        after_reconcile: impl FnOnce(),
    ) -> Result<SkillReadOutput, SkillStoreError> {
        crate::resource::validate_handle("package", &input.package)?;
        crate::resource::validate_handle("resource", &input.resource)?;
        self.reconcile_scope(input.scope)?;
        after_reconcile();
        self.read_reconciled(input)
    }

    /// Resolves a canonical skill name with project-before-global precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when catalog reconciliation or state access fails.
    pub fn resolve_name(&self, name: &str) -> Result<Option<ListedSkill>, SkillStoreError> {
        self.reconcile()?;
        let state = self
            .state
            .lock()
            .map_err(|_| SkillStoreError::StateUnavailable)?;
        Ok(precedence::resolve_name(
            &state.system.entries,
            &state.project.entries,
            &state.global.entries,
            name,
        )
        .cloned())
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn reconcile(&self) -> Result<(), SkillStoreError> {
        for scope in [SkillScope::System, SkillScope::Project, SkillScope::Global] {
            self.reconcile_scope(scope)?;
        }
        Ok(())
    }

    fn reconcile_scope(&self, scope: SkillScope) -> Result<(), SkillStoreError> {
        let refreshed = self.roots.scan(scope).map_err(|()| match scope {
            SkillScope::System => SkillStoreError::SystemVerification,
            SkillScope::Project | SkillScope::Global => SkillStoreError::AuthoritySetup,
        })?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SkillStoreError::StateUnavailable)?;
        let current = snapshot_mut(&mut state, scope);
        let contents_changed = current.fingerprint != refreshed.fingerprint;
        *current = refreshed;
        if contents_changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
}

fn snapshot_mut(state: &mut CatalogState, scope: SkillScope) -> &mut ScopeSnapshot {
    match scope {
        SkillScope::System => &mut state.system,
        SkillScope::Project => &mut state.project,
        SkillScope::Global => &mut state.global,
    }
}

fn snapshot(state: &CatalogState, scope: SkillScope) -> &ScopeSnapshot {
    match scope {
        SkillScope::System => &state.system,
        SkillScope::Project => &state.project,
        SkillScope::Global => &state.global,
    }
}

fn single_entry_response_is_bounded(skill: &ListedSkill) -> bool {
    serialized_len(&SkillListOutput {
        skills: vec![skill.clone()],
        warnings: Vec::new(),
        next_cursor: Some(pagination_cursor(skill, usize::MAX)),
    })
    .is_ok_and(|size| size <= MAX_RESPONSE_BYTES)
}

fn bounded_warnings(warnings: &[String], omitted_oversized_entry: bool) -> Vec<String> {
    warnings
        .iter()
        .map(String::as_str)
        .chain(omitted_oversized_entry.then_some(OVERSIZED_ENTRY_WARNING))
        .take(MAX_WARNINGS_PER_PAGE)
        .map(|warning| truncate_utf8_bytes(warning, MAX_WARNING_BYTES))
        .collect()
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let end = value.floor_char_boundary(max_bytes);
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn final_warning_bound_includes_the_oversized_entry_warning() {
        let warnings = vec![
            "я".repeat(200),
            "second warning".to_string(),
            "third warning".to_string(),
        ];

        let bounded = bounded_warnings(&warnings, true);

        assert_eq!(bounded.len(), MAX_WARNINGS_PER_PAGE);
        assert!(
            bounded
                .iter()
                .all(|warning| warning.len() <= MAX_WARNING_BYTES)
        );
        assert_eq!(bounded.last().unwrap(), OVERSIZED_ENTRY_WARNING);
    }

    #[test]
    fn read_keeps_the_validated_package_capability_across_a_name_swap() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let global = fixture.path().join("global");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&global).unwrap();
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        let package = authority.project_skills().root().join("swap");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("SKILL.md"),
            "---\nname: swap\ndescription: valid package\n---\nbody",
        )
        .unwrap();
        fs::write(package.join("data.md"), "validated bytes").unwrap();
        let catalog = SkillCatalog::new(&authority).unwrap();
        let input = SkillReadInput {
            scope: SkillScope::Project,
            package: "swap".to_string(),
            resource: "skill://host/project/swap/data.md".to_string(),
            cursor: None,
        };

        let page = catalog
            .read_after_reconcile_for_test(&input, || {
                fs::rename(
                    &package,
                    authority.project_skills().root().join("validated"),
                )
                .unwrap();
                fs::create_dir_all(&package).unwrap();
                fs::write(package.join("SKILL.md"), "not frontmatter").unwrap();
                fs::write(package.join("data.md"), "replacement bytes").unwrap();
            })
            .unwrap();

        assert_eq!(page.contents, "validated bytes");
        assert!(matches!(
            catalog.read(&input),
            Err(SkillStoreError::PackageUnavailable)
        ));
    }
}
