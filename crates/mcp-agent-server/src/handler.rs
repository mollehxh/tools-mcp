use crate::context::ApplicationContext;
use crate::result::{
    error_result, internal_serialization_error, invalid_arguments, success_result,
};
use codex_tools_runtime::contracts::ApplyPatchInput;
use codex_tools_runtime::process::{PendingResult, ProcessError};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ListToolsResult, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use skill_store::{SkillListInput, SkillReadInput, SkillStoreError};
use std::sync::Arc;

#[derive(Clone)]
pub struct AgentHandler {
    context: Arc<ApplicationContext>,
}

impl AgentHandler {
    #[must_use]
    pub fn new(context: Arc<ApplicationContext>) -> Self {
        Self { context }
    }

    #[must_use]
    pub fn tools() -> Vec<Tool> {
        crate::StubServer::tools()
    }

    pub async fn call(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
    ) -> CallToolResponse {
        let result = match name {
            "exec_command" => self.exec_command(arguments).await,
            "write_stdin" => self.write_stdin(arguments).await,
            "apply_patch" => self.apply_patch(arguments).await,
            "skills.list" => self.list_skills(arguments).await,
            "skills.read" => self.read_skill(arguments).await,
            _ => error_result("unknown_tool", "the requested tool is not available", None),
        };
        result.into()
    }

    async fn exec_command(
        &self,
        arguments: Option<Map<String, Value>>,
    ) -> rmcp::model::CallToolResult {
        let input: codex_tools_runtime::contracts::ExecCommandInput = match decode(arguments) {
            Ok(input) => input,
            Err(error) => return invalid_arguments(error),
        };
        pending_result(
            self.context
                .processes
                .exec_command(&self.context.owner, input)
                .await,
        )
        .await
    }

    async fn write_stdin(
        &self,
        arguments: Option<Map<String, Value>>,
    ) -> rmcp::model::CallToolResult {
        let input: codex_tools_runtime::contracts::WriteStdinInput = match decode(arguments) {
            Ok(input) => input,
            Err(error) => return invalid_arguments(error),
        };
        pending_result(
            self.context
                .processes
                .write_stdin(&self.context.owner, input)
                .await,
        )
        .await
    }

    async fn apply_patch(
        &self,
        arguments: Option<Map<String, Value>>,
    ) -> rmcp::model::CallToolResult {
        let input: ApplyPatchInput = match decode(arguments) {
            Ok(input) => input,
            Err(error) => return invalid_arguments(error),
        };
        let authority = self.context.authority.clone();
        match tokio::task::spawn_blocking(move || {
            codex_tools_runtime::patch::apply_patch(&authority, &input)
        })
        .await
        {
            Ok(Ok(output)) => {
                success_result(&output).unwrap_or_else(|_| internal_serialization_error())
            }
            Ok(Err(error)) => error_result("apply_patch_failed", &error.to_string(), None),
            Err(_) => error_result(
                "apply_patch_failed",
                "the patch worker stopped unexpectedly",
                None,
            ),
        }
    }

    async fn list_skills(
        &self,
        arguments: Option<Map<String, Value>>,
    ) -> rmcp::model::CallToolResult {
        let input: SkillListInput = match decode(arguments) {
            Ok(input) => input,
            Err(error) => return invalid_arguments(error),
        };
        let catalog = Arc::clone(&self.context.catalog);
        match tokio::task::spawn_blocking(move || catalog.list(&input)).await {
            Ok(Ok(output)) => {
                success_result(&output).unwrap_or_else(|_| internal_serialization_error())
            }
            Ok(Err(error)) => store_error(&error),
            Err(_) => error_result(
                "skill_store_failed",
                "the skill catalog worker stopped unexpectedly",
                None,
            ),
        }
    }

    async fn read_skill(
        &self,
        arguments: Option<Map<String, Value>>,
    ) -> rmcp::model::CallToolResult {
        let input: SkillReadInput = match decode(arguments) {
            Ok(input) => input,
            Err(error) => return invalid_arguments(error),
        };
        let catalog = Arc::clone(&self.context.catalog);
        match tokio::task::spawn_blocking(move || catalog.read(&input)).await {
            Ok(Ok(output)) => {
                success_result(&output).unwrap_or_else(|_| internal_serialization_error())
            }
            Ok(Err(error)) => store_error(&error),
            Err(_) => error_result(
                "skill_store_failed",
                "the skill catalog worker stopped unexpectedly",
                None,
            ),
        }
    }
}

impl ServerHandler for AgentHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Five local coding and skill tools with fixed workspace-write authority. Discover the reserved built-in installer with skills.list scope system, then read exactly scope system, package skill-installer, resource skill://host/system/skill-installer/SKILL.md.",
        )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown tool",
                None,
            ));
        }
        Ok(self.call(&request.name, request.arguments).await)
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        crate::StubServer::tools()
            .into_iter()
            .find(|tool| tool.name == name)
    }
}

fn decode<T: DeserializeOwned>(arguments: Option<Map<String, Value>>) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("arguments do not match the tool schema: {error}"))
}

async fn pending_result(
    result: Result<PendingResult, ProcessError>,
) -> rmcp::model::CallToolResult {
    match result {
        Ok(pending) => match pending.handoff().await {
            Ok(output) => {
                success_result(&output).unwrap_or_else(|_| internal_serialization_error())
            }
            Err(error) => process_error(&error),
        },
        Err(error) => process_error(&error),
    }
}

fn process_error(error: &ProcessError) -> rmcp::model::CallToolResult {
    let code = match error {
        ProcessError::Capacity { .. } => "capacity_exhausted",
        ProcessError::UnknownSession { .. } => "unknown_session",
        ProcessError::StdinClosed { .. } => "stdin_closed",
        ProcessError::ShuttingDown => "shutting_down",
        ProcessError::UnsupportedShell { .. } => "unsupported_shell",
        ProcessError::Spawn(_) => "command_launch_failed",
        ProcessError::Interaction(_) => "process_interaction_failed",
    };
    error_result(code, &error.to_string(), None)
}

fn store_error(error: &SkillStoreError) -> rmcp::model::CallToolResult {
    let code = match error {
        SkillStoreError::InvalidCursor { .. } => "invalid_cursor",
        SkillStoreError::StaleCursor { .. } => "stale_cursor",
        SkillStoreError::PackageUnavailable => "package_unavailable",
        SkillStoreError::InvalidResource => "invalid_resource",
        _ => "skill_store_failed",
    };
    error_result(code, &error.to_string(), None)
}
