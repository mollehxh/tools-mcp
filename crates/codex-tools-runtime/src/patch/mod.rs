mod adapter;

pub use crate::parser::ParseError as ApplyPatchParseError;
pub use adapter::apply_patch;

use mcp_agent_authority::{AuthorityError, OperationError};
use std::fmt;

#[derive(Debug)]
pub enum ApplyPatchError {
    Parse(ApplyPatchParseError),
    Policy(AuthorityError),
    ComputeReplacements(String),
    Filesystem {
        context: String,
        source: OperationError,
    },
    NoFilesModified,
}

impl ApplyPatchError {
    pub(crate) fn filesystem(context: String, source: OperationError) -> Self {
        Self::Filesystem { context, source }
    }
}

impl fmt::Display for ApplyPatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(ApplyPatchParseError::InvalidPatchError(message)) => {
                write!(formatter, "Invalid patch: {message}")
            }
            Self::Parse(ApplyPatchParseError::InvalidHunkError {
                message,
                line_number,
            }) => write!(
                formatter,
                "Invalid patch hunk on line {line_number}: {message}"
            ),
            Self::Policy(error) => error.fmt(formatter),
            Self::ComputeReplacements(message) => formatter.write_str(message),
            Self::Filesystem { context, source } => write!(formatter, "{context}: {source}"),
            Self::NoFilesModified => formatter.write_str("No files were modified."),
        }
    }
}

impl std::error::Error for ApplyPatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Policy(error) => Some(error),
            Self::Filesystem { source, .. } => Some(source),
            Self::ComputeReplacements(_) | Self::NoFilesModified => None,
        }
    }
}
