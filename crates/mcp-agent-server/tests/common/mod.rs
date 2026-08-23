#![allow(dead_code)]

use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
use mcp_agent_authority::{CapabilitySnapshot, WorkspaceAuthority};
use mcp_agent_server::{AgentHandler, ApplicationContext};
use skill_store::SkillCatalog;
use std::fs;
use std::path::Path;
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
        let release = root.path().join("release");
        let system_skills = release.join("system-skills");
        let home = root.path().join("home");
        let tmp = root.path().join("tmp");
        for directory in [&workspace, &system_skills, &home, &tmp] {
            fs::create_dir_all(directory).unwrap();
        }
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../third_party/openai-codex/skill-installer"),
            &system_skills.join("skill-installer"),
        );
        let codex_home = home.join(".codex");
        let capabilities = Arc::new(
            CapabilitySnapshot::resolve_configured(
                &workspace,
                &system_skills,
                |name| match name {
                    "HOME" => Some(home.clone().into_os_string()),
                    "CODEX_HOME" => Some(codex_home.clone().into_os_string()),
                    _ => None,
                },
                tmp.clone(),
                tmp,
            )
            .unwrap(),
        );
        let authority = WorkspaceAuthority::from_capabilities(capabilities).unwrap();
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

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
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
