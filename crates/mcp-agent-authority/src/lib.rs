//! Fixed workspace and native sandbox authority shared by all MCP tools.

#![allow(clippy::missing_errors_doc)]

mod operations;
mod roots;
pub mod sandbox;
mod workspace;

pub use operations::{OperationError, ServerOperations, WorkspaceOperations};
pub use roots::{ManagedRoot, ManagedWriteScope};
pub use workspace::{AuthorityError, CommandAuthority, WorkspaceAuthority};
