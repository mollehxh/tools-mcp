//! Backward-compatible re-exports of the execution-free tool contract crate.

pub use mcp_agent_tool_contracts::{
    ApplyPatchInput, ApplyPatchOutput, ExecCommandInput, ExecCommandOutput, ToolAnnotations,
    ToolContract, UnifiedExecRequest, UnifiedExecResult, WriteStdinInput, frozen_tool_contracts,
};
