#[path = "upstream/parser.rs"]
#[allow(clippy::missing_errors_doc)]
pub mod upstream;

mod catalog;
mod contracts;
mod cursor;
mod precedence;
mod resource;
mod roots;

pub use catalog::{SkillCatalog, SkillStoreError};
pub use contracts::{
    HostSkillMetadata, ListedSkill, SkillAuthority, SkillListInput, SkillListOutput,
    SkillReadInput, SkillReadOutput, SkillScope, SkillSource, SkillSourceKind,
};
