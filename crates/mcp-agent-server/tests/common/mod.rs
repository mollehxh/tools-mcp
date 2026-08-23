#![allow(dead_code)]

use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
use mcp_agent_server::{AgentHandler, ApplicationContext};
use skill_store::SkillCatalog;
use std::fs;
use std::sync::Arc;

pub struct Fixture {
    pub _root: tempfile::TempDir,
    pub workspace: std::path::PathBuf,
    pub processes: Arc<ProcessManager>,
    pub context: Arc<ApplicationContext>,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global-skills");
        let release = root.path().join("release");
        for directory in [&workspace, &global, &release] {
            fs::create_dir_all(directory).unwrap();
        }
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        expected_manifest()
            .unwrap()
            .write_release_relative(&release)
            .unwrap();
        let sandbox = Sandbox::load(authority.clone(), &release)
            .unwrap()
            .preflight()
            .unwrap()
            .0;
        let processes = Arc::new(ProcessManager::new(Arc::new(sandbox)));
        let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
        let context = Arc::new(ApplicationContext::new(
            authority,
            Arc::clone(&processes),
            catalog,
            OwnerId::from("local-anonymous"),
        ));
        Self {
            _root: root,
            workspace,
            processes,
            context,
        }
    }

    pub fn handler(&self) -> AgentHandler {
        AgentHandler::new(Arc::clone(&self.context))
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn arguments(value: serde_json::Value) -> rmcp::model::JsonObject {
    value.as_object().unwrap().clone()
}

pub fn complete(response: rmcp::model::CallToolResponse) -> rmcp::model::CallToolResult {
    match response {
        rmcp::model::CallToolResponse::Complete(result) => result,
        other => panic!("unexpected response: {other:?}"),
    }
}

#[cfg(unix)]
pub fn yielded_command() -> &'static str {
    "printf first; sleep 0.5; printf second"
}

#[cfg(windows)]
pub fn yielded_command() -> &'static str {
    "Write-Output -NoNewline first; Start-Sleep -Milliseconds 500; Write-Output -NoNewline second"
}
