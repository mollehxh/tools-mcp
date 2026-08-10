#[path = "upstream/parser.rs"]
#[allow(clippy::missing_errors_doc)]
pub mod upstream;

mod contracts;

pub use contracts::{
    HostSkillMetadata, ListedSkill, SkillAuthority, SkillListInput, SkillListOutput,
    SkillReadInput, SkillReadOutput, SkillScope, SkillSource, SkillSourceKind,
};
