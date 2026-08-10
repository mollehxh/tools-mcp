use codex_tools_runtime::contracts::{ToolContract, frozen_tool_contracts};
use rmcp::ServerHandler;
use rmcp::model::{ListToolsResult, ServerCapabilities, ServerInfo, Tool, ToolAnnotations};
use serde_json::{Map, Value};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, Default)]
pub struct StubServer;

impl StubServer {
    #[must_use]
    pub fn tools() -> Vec<Tool> {
        cached_tools().clone()
    }
}

impl ServerHandler for StubServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("U1 contract-only stub; tool execution is intentionally unavailable")
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(Self::tools())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        cached_tools()
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }
}

fn cached_tools() -> &'static Vec<Tool> {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        frozen_tool_contracts()
            .iter()
            .map(contract_to_tool)
            .collect()
    })
}

fn contract_to_tool(contract: &ToolContract) -> Tool {
    let input_schema = object_schema(&contract.input_schema);
    let output_schema = contract.output_schema.as_ref().map(object_schema);
    let mut tool = Tool::new(
        contract.name.clone(),
        contract.description.clone(),
        Arc::new(input_schema),
    )
    .with_annotations(ToolAnnotations::from_raw(
        None,
        Some(contract.annotations.read_only_hint),
        Some(contract.annotations.destructive_hint),
        None,
        Some(contract.annotations.open_world_hint),
    ));
    if let Some(output_schema) = output_schema {
        tool = tool.with_raw_output_schema(Arc::new(output_schema));
    }
    tool
}

fn object_schema(value: &Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("frozen tool schemas must be JSON objects")
        .clone()
}
