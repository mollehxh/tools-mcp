use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecCommandInput {
    pub cmd: String,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default = "default_exec_yield_time_ms")]
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub login: Option<bool>,
}

const fn default_exec_yield_time_ms() -> u64 {
    10_000
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnifiedExecRequest {
    pub cmd: String,
    pub workdir: Option<String>,
    pub tty: bool,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub shell: Option<String>,
    pub login: Option<bool>,
}

impl ExecCommandInput {
    #[must_use]
    pub fn into_unified_exec_request(self) -> UnifiedExecRequest {
        UnifiedExecRequest {
            cmd: self.cmd,
            workdir: self.workdir,
            tty: self.tty,
            yield_time_ms: self.yield_time_ms,
            max_output_tokens: self.max_output_tokens,
            shell: self.shell,
            login: self.login,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedExecResult {
    pub chunk_id: Option<String>,
    pub wall_time: Duration,
    pub exit_code: Option<i32>,
    pub process_id: Option<i32>,
    pub original_token_count: Option<usize>,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecCommandOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    pub wall_time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_token_count: Option<usize>,
    pub output: String,
}

impl From<UnifiedExecResult> for ExecCommandOutput {
    fn from(result: UnifiedExecResult) -> Self {
        Self {
            chunk_id: result.chunk_id,
            wall_time_seconds: result.wall_time.as_secs_f64(),
            exit_code: result.exit_code,
            session_id: result.process_id,
            original_token_count: result.original_token_count,
            output: result.output,
        }
    }
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
