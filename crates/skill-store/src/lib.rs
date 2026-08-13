#[path = "upstream/parser.rs"]
#[allow(clippy::missing_errors_doc)]
pub mod upstream;

mod catalog;
mod contracts;
mod cursor;
mod install;
mod precedence;
mod resource;
mod roots;

pub use catalog::{SkillCatalog, SkillStoreError};
pub use contracts::{
    HostSkillMetadata, ListedSkill, SkillAuthority, SkillInstallInput, SkillInstallOutput,
    SkillListInput, SkillListOutput, SkillReadInput, SkillReadOutput, SkillScope, SkillSource,
    SkillSourceKind,
};
pub use install::{
    FetchedRepository, GitFetcher, GixGitFetcher, InstallLimits, NormalizedGitSource,
    RepositoryEntry, RepositoryEntryKind, SkillInstallError, SkillInstaller, TransportHop,
    TransportScript, evaluate_transport_script, normalize_git_source, validate_object_expansion,
    validate_pack_expansion, validate_repository_tree,
};
