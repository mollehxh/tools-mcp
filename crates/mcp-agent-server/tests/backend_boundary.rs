use mcp_agent_server::{AgentHandler, ApplicationContext};
use mcp_agent_tool_contracts::{
    ApplyPatchOutput, BackendError, BackendFuture, CallContext, ExecCommandOutput, SkillListOutput,
    SkillReadOutput, ToolBackend, ToolOutput, ToolRequest,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

struct FakeBackend;

impl ToolBackend for FakeBackend {
    fn call(&self, _context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            Ok(match request {
                ToolRequest::ExecCommand(_) => ToolOutput::ExecCommand(command_output("exec")),
                ToolRequest::WriteStdin(_) => ToolOutput::WriteStdin(command_output("stdin")),
                ToolRequest::ApplyPatch(_) => ToolOutput::ApplyPatch(ApplyPatchOutput {
                    output: "patch".to_owned(),
                }),
                ToolRequest::TerminateSession(_) => {
                    ToolOutput::TerminateSession(mcp_agent_tool_contracts::TerminateSessionOutput {
                        terminated: true,
                    })
                }
                ToolRequest::SkillsList(_) => ToolOutput::SkillsList(SkillListOutput {
                    skills: Vec::new(),
                    warnings: vec!["list".to_owned()],
                    next_cursor: None,
                }),
                ToolRequest::SkillsRead(input) => ToolOutput::SkillsRead(SkillReadOutput {
                    resource: input.resource,
                    contents: "read".to_owned(),
                    next_cursor: None,
                }),
            })
        })
    }
}

fn command_output(output: &str) -> ExecCommandOutput {
    ExecCommandOutput {
        chunk_id: None,
        wall_time_seconds: 0.0,
        exit_code: Some(0),
        session_id: None,
        original_token_count: None,
        output: output.to_owned(),
    }
}

fn arguments(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(arguments) => arguments,
        _ => panic!("test arguments must be an object"),
    }
}

fn structured(response: rmcp::model::CallToolResponse) -> Value {
    match response {
        rmcp::model::CallToolResponse::Complete(result) => result.structured_content.unwrap(),
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn execution_free_fake_backend_preserves_each_typed_output_shape() {
    let handler = AgentHandler::new(Arc::new(ApplicationContext::new(Arc::new(FakeBackend))));
    let cases = [
        (
            "exec_command",
            json!({"cmd":"true"}),
            json!({"wall_time_seconds":0.0,"exit_code":0,"output":"exec"}),
        ),
        (
            "write_stdin",
            json!({"session_id":1000,"chars":""}),
            json!({"wall_time_seconds":0.0,"exit_code":0,"output":"stdin"}),
        ),
        (
            "apply_patch",
            json!({"patch":"*** Begin Patch\n*** End Patch"}),
            json!({"output":"patch"}),
        ),
        (
            "skills.list",
            json!({"scope":"system"}),
            json!({"skills":[],"warnings":["list"],"next_cursor":null}),
        ),
        (
            "skills.read",
            json!({"scope":"system","package":"p","resource":"skill://host/system/p/SKILL.md"}),
            json!({"resource":"skill://host/system/p/SKILL.md","contents":"read","next_cursor":null}),
        ),
    ];
    for (name, input, expected) in cases {
        assert_eq!(
            structured(handler.call(name, Some(arguments(input))).await),
            expected
        );
    }
}

#[allow(dead_code)]
fn backend_error_is_transport_neutral(error: BackendError) -> (&'static str, String) {
    (error.code, error.message)
}
