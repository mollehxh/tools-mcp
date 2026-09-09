use crate::context::{ApplicationContext, current_identity};
use crate::result::{
    error_result, internal_serialization_error, invalid_arguments, success_result,
};
use mcp_agent_tool_contracts::{BackendError, CallContext, CallIdentity, ToolOutput, ToolRequest};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ListToolsResult, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const SERVER_INSTRUCTIONS: &str = "Autonomous coding tools with fixed managed-root authority. Execute user-requested shell, Git, package-manager, network, and repository operations directly with exec_command; do not invent restrictions that were not returned by a tool, and do not send the user to a separate terminal to run an available command. Interactive and browser-backed CLI authentication is supported. Start interactive authentication commands yourself with exec_command and tty=true. For GitHub, run `gh auth login --web`, show the returned URL and one-time code, retain its session_id, poll it with write_stdin after the user confirms, and verify with `gh auth status`. Failure to launch a browser does not mean a web or device flow is blocked: surface the provider URL/code and continue the existing terminal session. Human approval on the provider page is the only step to hand back to the user. Each VPS tenant has its own persistent HOME and credentials, so never reuse or request another tenant's token. Discover the reserved built-in installer with skills.list scope system, then read exactly scope system, package skill-installer, resource skill://host/system/skill-installer/SKILL.md.";

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
        self.call_with_context(
            name,
            arguments,
            CallContext::new(CancellationToken::new(), None),
        )
        .await
    }

    async fn call_with_context(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
        context: CallContext,
    ) -> CallToolResponse {
        if self.get_tool(name).is_none() {
            return error_result("unknown_tool", "the requested tool is not available", None)
                .into();
        }
        let request = match decode_request(name, arguments) {
            Ok(request) => request,
            Err(error) => return invalid_arguments(error).into(),
        };
        match self.context.backend.call(context, request).await {
            Ok(output) => render_output(output).into(),
            Err(error) => render_error(&error).into(),
        }
    }
}

impl ServerHandler for AgentHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown tool",
                None,
            ));
        }
        let identity = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<CallIdentity>())
            .cloned()
            .or_else(current_identity);
        let mut call_context = CallContext::new(context.ct, None);
        if let Some(identity) = identity {
            call_context = call_context.with_identity(identity);
        }
        Ok(self
            .call_with_context(&request.name, request.arguments, call_context)
            .await)
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

fn decode_request(
    name: &str,
    arguments: Option<Map<String, Value>>,
) -> Result<ToolRequest, String> {
    match name {
        "exec_command" => decode(arguments).map(ToolRequest::ExecCommand),
        "write_stdin" => decode(arguments).map(ToolRequest::WriteStdin),
        "apply_patch" => decode(arguments).map(ToolRequest::ApplyPatch),
        "skills.list" => decode(arguments).map(ToolRequest::SkillsList),
        "skills.read" => decode(arguments).map(ToolRequest::SkillsRead),
        _ => Err("the requested tool is not available".to_owned()),
    }
}

fn decode<T: DeserializeOwned>(arguments: Option<Map<String, Value>>) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("arguments do not match the tool schema: {error}"))
}

fn render_output(output: ToolOutput) -> rmcp::model::CallToolResult {
    let result = match output {
        ToolOutput::ExecCommand(value) | ToolOutput::WriteStdin(value) => success_result(&value),
        ToolOutput::ApplyPatch(value) => success_result(&value),
        ToolOutput::TerminateSession(value) => success_result(&value),
        ToolOutput::SkillsList(value) => success_result(&value),
        ToolOutput::SkillsRead(value) => success_result(&value),
    };
    result.unwrap_or_else(|_| internal_serialization_error())
}

fn render_error(error: &BackendError) -> rmcp::model::CallToolResult {
    error_result(error.code, &error.message, error.details.clone())
}

#[cfg(test)]
mod tests {
    use super::SERVER_INSTRUCTIONS;

    #[test]
    fn server_instructions_require_agents_to_drive_human_cli_auth_flows() {
        assert!(SERVER_INSTRUCTIONS.contains("gh auth login --web"));
        assert!(SERVER_INSTRUCTIONS.contains("write_stdin"));
        assert!(SERVER_INSTRUCTIONS.contains("do not invent restrictions"));
        assert!(SERVER_INSTRUCTIONS.contains("do not send the user to a separate terminal"));
        assert!(SERVER_INSTRUCTIONS.contains("does not mean a web or device flow is blocked"));
        assert!(SERVER_INSTRUCTIONS.contains("never reuse or request another tenant's token"));
    }
}
