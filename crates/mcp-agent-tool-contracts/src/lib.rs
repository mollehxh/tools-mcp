//! Execution-free DTOs and backend boundary shared by MCP adapters and workers.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod skill;

pub use skill::{
    HostSkillMetadata, ListedSkill, SkillAuthority, SkillListInput, SkillListOutput,
    SkillReadInput, SkillReadOutput, SkillScope, SkillSource, SkillSourceKind,
};

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WriteStdinInput {
    pub session_id: i32,
    #[serde(default)]
    pub chars: String,
    #[serde(default = "default_write_stdin_yield_time_ms")]
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl<'de> Deserialize<'de> for WriteStdinInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            session_id: i32,
            #[serde(default)]
            chars: String,
            #[serde(default)]
            yield_time_ms: Option<u64>,
            #[serde(default)]
            max_output_tokens: Option<usize>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let yield_time_ms = raw.yield_time_ms.unwrap_or_else(|| {
            if raw.chars.is_empty() {
                default_empty_poll_yield_time_ms()
            } else {
                default_write_stdin_yield_time_ms()
            }
        });
        Ok(Self {
            session_id: raw.session_id,
            chars: raw.chars,
            yield_time_ms,
            max_output_tokens: raw.max_output_tokens,
        })
    }
}

impl WriteStdinInput {
    #[must_use]
    pub fn poll(session_id: i32) -> Self {
        Self {
            session_id,
            chars: String::new(),
            yield_time_ms: default_empty_poll_yield_time_ms(),
            max_output_tokens: None,
        }
    }
}

const fn default_write_stdin_yield_time_ms() -> u64 {
    250
}
const fn default_empty_poll_yield_time_ms() -> u64 {
    5_000
}

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

/// Internal-only administrative request. It is intentionally absent from the MCP tool surface.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminateSessionInput {
    pub session_id: i32,
}

/// Confirms that an internal session termination reached its owning process manager.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TerminateSessionOutput {
    pub terminated: bool,
}

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
/// Returns the pinned five-tool contract fixture.
///
/// # Panics
///
/// Panics only when the audited checked-in JSON fixture is malformed.
pub fn frozen_tool_contracts() -> &'static [ToolContract] {
    static CONTRACTS: OnceLock<Vec<ToolContract>> = OnceLock::new();
    CONTRACTS
        .get_or_init(|| {
            serde_json::from_str(include_str!(
                "../../../tests/conformance/fixtures/tool-contracts.json"
            ))
            .expect("checked-in tool contract fixture must be valid")
        })
        .as_slice()
}

#[derive(Clone, Debug)]
pub struct CallContext {
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
    pub identity: Option<CallIdentity>,
}

impl CallContext {
    #[must_use]
    pub fn new(cancellation: CancellationToken, deadline: Option<Instant>) -> Self {
        Self {
            cancellation,
            deadline,
            identity: None,
        }
    }

    #[must_use]
    pub fn with_identity(mut self, identity: CallIdentity) -> Self {
        self.identity = Some(identity);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallIdentity {
    pub principal_fingerprint: String,
    pub session_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "tool", content = "input", rename_all = "snake_case")]
pub enum ToolRequest {
    ExecCommand(ExecCommandInput),
    WriteStdin(WriteStdinInput),
    ApplyPatch(ApplyPatchInput),
    TerminateSession(TerminateSessionInput),
    SkillsList(SkillListInput),
    SkillsRead(SkillReadInput),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "tool", content = "output", rename_all = "snake_case")]
pub enum ToolOutput {
    ExecCommand(ExecCommandOutput),
    WriteStdin(ExecCommandOutput),
    ApplyPatch(ApplyPatchOutput),
    TerminateSession(TerminateSessionOutput),
    SkillsList(SkillListOutput),
    SkillsRead(SkillReadOutput),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError {
    pub code: &'static str,
    pub message: String,
    pub details: Option<Value>,
}

impl BackendError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BackendError {}

pub type BackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolOutput, BackendError>> + Send + 'a>>;

pub trait ToolBackend: Send + Sync {
    fn call(&self, context: CallContext, request: ToolRequest) -> BackendFuture<'_>;
}
