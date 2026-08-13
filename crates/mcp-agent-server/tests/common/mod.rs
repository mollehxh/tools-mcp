#![allow(dead_code)]

use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_authority::sandbox::{Sandbox, expected_manifest};
use mcp_agent_server::{AgentHandler, ApplicationContext};
use skill_store::{
    FetchedRepository, GitFetcher, InstallLimits, NormalizedGitSource, RepositoryEntry,
    RepositoryEntryKind, SkillCatalog, SkillInstallError, SkillInstaller,
};
use std::fs;
use std::sync::Arc;

#[derive(Debug)]
struct FixtureFetcher;

impl GitFetcher for FixtureFetcher {
    fn fetch(
        &self,
        source: &NormalizedGitSource,
        _limits: &InstallLimits,
    ) -> Result<FetchedRepository, SkillInstallError> {
        Ok(FetchedRepository {
            repository: source.repository.clone(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            entries: vec![RepositoryEntry {
                path: "SKILL.md".to_string(),
                kind: RepositoryEntryKind::RegularFile,
                bytes: b"---\nname: installed\ndescription: installed fixture\n---\nbody".to_vec(),
            }],
        })
    }
}

pub struct Fixture {
    pub _root: tempfile::TempDir,
    pub workspace: std::path::PathBuf,
    pub processes: Arc<ProcessManager>,
    pub context: Arc<ApplicationContext>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_fetcher(Arc::new(FixtureFetcher))
    }

    pub fn with_fetcher(fetcher: Arc<dyn GitFetcher>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let global = root.path().join("global-skills");
        let outside = root.path().join("outside");
        let release = root.path().join("release");
        for directory in [&workspace, &global, &outside, &release] {
            fs::create_dir_all(directory).unwrap();
        }
        let authority =
            WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap())
                .unwrap();
        expected_manifest()
            .unwrap()
            .write_release_relative(&release)
            .unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"host-readable").unwrap();
        let sandbox = Sandbox::load(authority.clone(), &release)
            .unwrap()
            .preflight(&sentinel)
            .unwrap()
            .0;
        let processes = Arc::new(ProcessManager::new(Arc::new(sandbox)));
        let catalog = Arc::new(SkillCatalog::new(&authority).unwrap());
        let installer = Arc::new(
            SkillInstaller::with_fetcher(&authority, Arc::clone(&catalog), fetcher).unwrap(),
        );
        let context = Arc::new(ApplicationContext::new(
            authority,
            Arc::clone(&processes),
            catalog,
            installer,
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
