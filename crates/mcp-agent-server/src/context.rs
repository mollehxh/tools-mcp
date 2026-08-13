use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{SkillCatalog, SkillInstaller};
use std::sync::Arc;

/// Long-lived, owner-scoped application capabilities shared by fresh MCP handlers.
#[derive(Clone)]
pub struct ApplicationContext {
    pub(crate) authority: WorkspaceAuthority,
    pub(crate) processes: Arc<ProcessManager>,
    pub(crate) catalog: Arc<SkillCatalog>,
    pub(crate) installer: Arc<SkillInstaller>,
    pub(crate) owner: OwnerId,
}

impl ApplicationContext {
    #[must_use]
    pub fn new(
        authority: WorkspaceAuthority,
        processes: Arc<ProcessManager>,
        catalog: Arc<SkillCatalog>,
        installer: Arc<SkillInstaller>,
        owner: OwnerId,
    ) -> Self {
        Self {
            authority,
            processes,
            catalog,
            installer,
            owner,
        }
    }
}
