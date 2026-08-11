use serde::{Deserialize, Serialize};

/// MCP object carrier for the original Codex freeform patch body.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchInput {
    pub patch: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchOutput {
    pub output: String,
}
