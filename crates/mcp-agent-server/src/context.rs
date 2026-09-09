use mcp_agent_tool_contracts::{CallIdentity, ToolBackend};
use std::sync::Arc;

/// Transport adapter context containing only an execution-free backend trait.
#[derive(Clone)]
pub struct ApplicationContext {
    pub(crate) backend: Arc<dyn ToolBackend>,
}

impl ApplicationContext {
    #[must_use]
    pub fn new<B>(backend: Arc<B>) -> Self
    where
        B: ToolBackend + 'static,
    {
        Self { backend }
    }
}

tokio::task_local! {
    pub(crate) static TRUSTED_CALL_IDENTITY: CallIdentity;
}

pub(crate) fn current_identity() -> Option<CallIdentity> {
    TRUSTED_CALL_IDENTITY.try_with(Clone::clone).ok()
}
