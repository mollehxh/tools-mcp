use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolContract {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: ToolAnnotations,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAnnotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub open_world_hint: bool,
}

#[must_use]
/// Returns the pinned, checked-in six-tool contract fixture.
///
/// # Panics
///
/// Panics only when the repository's audited JSON fixture is malformed.
pub fn frozen_tool_contracts() -> &'static [ToolContract] {
    static CONTRACTS: OnceLock<Vec<ToolContract>> = OnceLock::new();
    CONTRACTS
        .get_or_init(|| {
            serde_json::from_str(include_str!(
                "../../../../tests/conformance/fixtures/tool-contracts.json"
            ))
            .expect("checked-in tool contract fixture must be valid")
        })
        .as_slice()
}
