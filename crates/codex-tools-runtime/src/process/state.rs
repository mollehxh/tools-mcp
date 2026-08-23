use std::fmt;
use std::sync::Arc;

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct OwnerId(Arc<str>);

impl OwnerId {
    #[must_use]
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }
}

impl From<&str> for OwnerId {
    fn from(value: &str) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

impl From<String> for OwnerId {
    fn from(value: String) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

impl fmt::Debug for OwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnerId(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("unified exec capacity is exhausted (limit {limit})")]
    Capacity { limit: usize },
    #[error("unknown unified exec session {session_id}")]
    UnknownSession { session_id: i32 },
    #[error("stdin is closed for non-PTY session {session_id}")]
    StdinClosed { session_id: i32 },
    #[error("the process registry is shutting down")]
    ShuttingDown,
    #[error("unsupported workload shell {shell}; expected sh, bash, zsh, or fish")]
    UnsupportedShell { shell: String },
    #[error("command launch was rejected")]
    Spawn(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("process interaction failed")]
    Interaction(#[source] std::io::Error),
}

impl ProcessError {
    pub(crate) fn spawn(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Spawn(Box::new(error))
    }
}
