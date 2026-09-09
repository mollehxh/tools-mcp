//! Direct local implementation of the transport-neutral tool backend.

use codex_tools_runtime::process::{OwnerId, PendingResult, ProcessError, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, CallIdentity, SkillListInput, SkillListOutput,
    SkillReadInput, SkillReadOutput, SkillScope, ToolBackend, ToolOutput, ToolRequest,
};
use sha2::{Digest as _, Sha256};
use skill_store::{SkillCatalog, SkillStoreError};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_PROJECT_SESSIONS: usize = 256;
const MAX_PROJECT_CATALOGS: usize = 128;
const PROJECT_HANDLE_PREFIX: &str = "mcpctx-";

#[derive(Clone)]
pub struct LocalBackend {
    authority: WorkspaceAuthority,
    processes: Arc<ProcessManager>,
    projects: Arc<ProjectCatalogs>,
    owner: OwnerId,
}

#[derive(Clone, Debug)]
struct ActiveProject {
    root: PathBuf,
    token: String,
}

#[derive(Debug, Default)]
struct ProjectState {
    sessions: HashMap<CallIdentity, ActiveProject>,
    catalogs: HashMap<PathBuf, Arc<SkillCatalog>>,
    next_token: u64,
}

#[derive(Debug)]
struct ProjectCatalogs {
    authority: WorkspaceAuthority,
    default: Arc<SkillCatalog>,
    state: Mutex<ProjectState>,
}

impl LocalBackend {
    #[must_use]
    pub fn new(
        authority: WorkspaceAuthority,
        processes: Arc<ProcessManager>,
        catalog: Arc<SkillCatalog>,
        owner: OwnerId,
    ) -> Self {
        let projects = Arc::new(ProjectCatalogs {
            authority: authority.clone(),
            default: catalog,
            state: Mutex::new(ProjectState::default()),
        });
        Self {
            authority,
            processes,
            projects,
            owner,
        }
    }

    pub async fn shutdown(&self) {
        self.processes.shutdown().await;
    }

    async fn dispatch(
        &self,
        context: CallContext,
        request: ToolRequest,
    ) -> Result<ToolOutput, BackendError> {
        if context.cancellation.is_cancelled() {
            return Err(BackendError::new(
                "request_cancelled",
                "the request was cancelled before dispatch",
            ));
        }
        if context
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Err(BackendError::new(
                "deadline_exceeded",
                "the request deadline elapsed before dispatch",
            ));
        }
        match request {
            ToolRequest::ExecCommand(input) => {
                let identity = call_identity(&context);
                let project = self.prepare_command_project(&identity, &input)?;
                let pending = self
                    .processes
                    .exec_command(&self.owner, input)
                    .await
                    .map_err(|error| process_error(&error))?;
                let output = await_pending(&context, pending).await?;
                if let Some(project) = project {
                    self.projects.activate(identity, project);
                }
                Ok(ToolOutput::ExecCommand(output))
            }
            ToolRequest::WriteStdin(input) => {
                let pending = self
                    .processes
                    .write_stdin(&self.owner, input)
                    .await
                    .map_err(|error| process_error(&error))?;
                await_pending(&context, pending)
                    .await
                    .map(ToolOutput::WriteStdin)
            }
            ToolRequest::TerminateSession(input) => {
                self.processes
                    .terminate(&self.owner, input.session_id)
                    .await
                    .map_err(|error| process_error(&error))?;
                Ok(ToolOutput::TerminateSession(
                    mcp_agent_tool_contracts::TerminateSessionOutput { terminated: true },
                ))
            }
            ToolRequest::ApplyPatch(input) => {
                let authority = self.authority.clone();
                let task = tokio::task::spawn_blocking(move || {
                    codex_tools_runtime::patch::apply_patch(&authority, &input)
                });
                tokio::select! {
                    () = context.cancellation.cancelled() => Err(BackendError::new("request_cancelled", "the request was cancelled")),
                    result = task => match result {
                        Ok(Ok(output)) => Ok(ToolOutput::ApplyPatch(output)),
                        Ok(Err(error)) => Err(BackendError::new("apply_patch_failed", error.to_string())),
                        Err(_) => Err(BackendError::new("apply_patch_failed", "the patch worker stopped unexpectedly")),
                    }
                }
            }
            ToolRequest::SkillsList(input) => {
                let identity = call_identity(&context);
                let projects = Arc::clone(&self.projects);
                let task = tokio::task::spawn_blocking(move || projects.list(&identity, input));
                tokio::select! {
                    () = context.cancellation.cancelled() => Err(BackendError::new("request_cancelled", "the request was cancelled")),
                    result = task => match result {
                        Ok(Ok(output)) => Ok(ToolOutput::SkillsList(output)),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(BackendError::new("skill_store_failed", "the skill catalog worker stopped unexpectedly")),
                    }
                }
            }
            ToolRequest::SkillsRead(input) => {
                let identity = call_identity(&context);
                let projects = Arc::clone(&self.projects);
                let task = tokio::task::spawn_blocking(move || projects.read(&identity, input));
                tokio::select! {
                    () = context.cancellation.cancelled() => Err(BackendError::new("request_cancelled", "the request was cancelled")),
                    result = task => match result {
                        Ok(Ok(output)) => Ok(ToolOutput::SkillsRead(output)),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(BackendError::new("skill_store_failed", "the skill catalog worker stopped unexpectedly")),
                    }
                }
            }
        }
    }
}

impl LocalBackend {
    fn prepare_command_project(
        &self,
        identity: &CallIdentity,
        input: &mcp_agent_tool_contracts::ExecCommandInput,
    ) -> Result<Option<ActiveProject>, BackendError> {
        if input.workdir.as_deref().is_none_or(str::is_empty) {
            return Ok(None);
        }
        let relative = self
            .processes
            .workspace_relative_workdir(input)
            .map_err(|error| process_error(&error))?;
        self.projects.prepare(identity, &relative).map(Some)
    }
}

impl ProjectCatalogs {
    fn prepare(
        &self,
        identity: &CallIdentity,
        workdir: &Path,
    ) -> Result<ActiveProject, BackendError> {
        let root = self
            .authority
            .nearest_git_project(workdir)
            .map_err(|_| {
                BackendError::new("invalid_workdir", "the command workdir is unavailable")
            })?
            .unwrap_or_default();
        let mut state = self.lock();
        if let Some(active) = state.sessions.get(identity) {
            if active.root == root {
                return Ok(active.clone());
            }
        } else if state.sessions.len() >= MAX_PROJECT_SESSIONS {
            return Err(BackendError::new(
                "capacity_exhausted",
                "the active-project session budget is exhausted",
            ));
        }
        if !root.as_os_str().is_empty() && !state.catalogs.contains_key(&root) {
            if state.catalogs.len() >= MAX_PROJECT_CATALOGS {
                return Err(BackendError::new(
                    "capacity_exhausted",
                    "the project-skill catalog budget is exhausted",
                ));
            }
            let catalog = SkillCatalog::for_project(&self.authority, root.clone())
                .map_err(|error| store_error(&error))?;
            state.catalogs.insert(root.clone(), Arc::new(catalog));
        }
        state.next_token = state.next_token.checked_add(1).ok_or_else(|| {
            BackendError::new(
                "capacity_exhausted",
                "the active-project token space is exhausted",
            )
        })?;
        Ok(ActiveProject {
            root,
            token: project_token(state.next_token, identity),
        })
    }

    fn activate(&self, identity: CallIdentity, project: ActiveProject) {
        self.lock().sessions.insert(identity, project);
    }

    fn list(
        &self,
        identity: &CallIdentity,
        mut input: SkillListInput,
    ) -> Result<SkillListOutput, BackendError> {
        if input.scope != SkillScope::Project {
            return self
                .default
                .list(&input)
                .map_err(|error| store_error(&error));
        }
        let active = self.active(identity);
        if let Some(cursor) = input.cursor.take() {
            input.cursor = Some(decode_cursor(&active, &cursor)?);
        }
        let catalog = self.catalog(&active)?;
        let mut output = catalog.list(&input).map_err(|error| store_error(&error))?;
        if !active.root.as_os_str().is_empty() {
            for skill in &mut output.skills {
                encode_skill_handle(skill, &active.token)?;
            }
            if let Some(cursor) = output.next_cursor.take() {
                output.next_cursor = Some(encode_cursor(&active.token, &cursor));
            }
        }
        Ok(output)
    }

    fn read(
        &self,
        identity: &CallIdentity,
        mut input: SkillReadInput,
    ) -> Result<SkillReadOutput, BackendError> {
        if input.scope != SkillScope::Project {
            return self
                .default
                .read(&input)
                .map_err(|error| store_error(&error));
        }
        let active = self.active(identity);
        if !active.root.as_os_str().is_empty() {
            decode_skill_handle(&mut input, &active.token)?;
            if let Some(cursor) = input.cursor.take() {
                input.cursor = Some(decode_cursor(&active, &cursor)?);
            }
        }
        let catalog = self.catalog(&active)?;
        let mut output = catalog.read(&input).map_err(|error| store_error(&error))?;
        if !active.root.as_os_str().is_empty() {
            output.resource = encode_resource(&output.resource, &input.package, &active.token)?;
            if let Some(cursor) = output.next_cursor.take() {
                output.next_cursor = Some(encode_cursor(&active.token, &cursor));
            }
        }
        Ok(output)
    }

    fn active(&self, identity: &CallIdentity) -> ActiveProject {
        self.lock()
            .sessions
            .get(identity)
            .cloned()
            .unwrap_or_else(|| ActiveProject {
                root: PathBuf::new(),
                token: String::new(),
            })
    }

    fn catalog(&self, active: &ActiveProject) -> Result<Arc<SkillCatalog>, BackendError> {
        if active.root.as_os_str().is_empty() {
            return Ok(Arc::clone(&self.default));
        }
        self.lock()
            .catalogs
            .get(&active.root)
            .cloned()
            .ok_or_else(|| BackendError::new("lost_context", "the active project is unavailable"))
    }

    fn lock(&self) -> MutexGuard<'_, ProjectState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn call_identity(context: &CallContext) -> CallIdentity {
    context.identity.clone().unwrap_or_else(|| CallIdentity {
        principal_fingerprint: "local-anonymous".to_owned(),
        session_fingerprint: "local-default".to_owned(),
    })
}

fn project_token(sequence: u64, identity: &CallIdentity) -> String {
    let mut digest = Sha256::new();
    digest.update(identity.principal_fingerprint.as_bytes());
    digest.update([0]);
    digest.update(identity.session_fingerprint.as_bytes());
    digest.update(sequence.to_be_bytes());
    digest
        .finalize()
        .iter()
        .take(12)
        .fold(String::with_capacity(24), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn encode_skill_handle(
    skill: &mut mcp_agent_tool_contracts::ListedSkill,
    token: &str,
) -> Result<(), BackendError> {
    let original_package = skill.package.clone();
    skill.package = format!("{PROJECT_HANDLE_PREFIX}{token}--{original_package}");
    skill.main_resource = encode_resource(&skill.main_resource, &original_package, token)?;
    Ok(())
}

fn encode_resource(resource: &str, package: &str, token: &str) -> Result<String, BackendError> {
    let prefix = format!("skill://host/project/{package}/");
    let relative = resource.strip_prefix(&prefix).ok_or_else(|| {
        BackendError::new("invalid_resource", "the project skill resource is invalid")
    })?;
    Ok(format!(
        "skill://host/project/{PROJECT_HANDLE_PREFIX}{token}--{package}/{relative}"
    ))
}

fn decode_skill_handle(input: &mut SkillReadInput, token: &str) -> Result<(), BackendError> {
    let encoded_prefix = format!("{PROJECT_HANDLE_PREFIX}{token}--");
    let package = input
        .package
        .strip_prefix(&encoded_prefix)
        .ok_or_else(|| BackendError::new("invalid_resource", "the project skill context is stale"))?
        .to_owned();
    let resource_prefix = format!("skill://host/project/{}/", input.package);
    let relative = input
        .resource
        .strip_prefix(&resource_prefix)
        .ok_or_else(|| {
            BackendError::new("invalid_resource", "the project skill resource is invalid")
        })?;
    let resource = format!("skill://host/project/{package}/{relative}");
    input.package = package;
    input.resource = resource;
    Ok(())
}

fn encode_cursor(token: &str, cursor: &str) -> String {
    format!("{PROJECT_HANDLE_PREFIX}{token}:{cursor}")
}

fn decode_cursor(active: &ActiveProject, cursor: &str) -> Result<String, BackendError> {
    if active.root.as_os_str().is_empty() {
        return Ok(cursor.to_owned());
    }
    let prefix = format!("{PROJECT_HANDLE_PREFIX}{}:", active.token);
    cursor
        .strip_prefix(&prefix)
        .map(str::to_owned)
        .ok_or_else(|| {
            BackendError::new("stale_cursor", "the project skill cursor context is stale")
        })
}

impl ToolBackend for LocalBackend {
    fn call(&self, context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(self.dispatch(context, request))
    }
}

async fn await_pending(
    context: &CallContext,
    pending: PendingResult,
) -> Result<mcp_agent_tool_contracts::ExecCommandOutput, BackendError> {
    tokio::select! {
        () = context.cancellation.cancelled() => Err(BackendError::new("request_cancelled", "the request was cancelled")),
        result = pending.handoff() => result.map_err(|error| process_error(&error)),
    }
}

fn process_error(error: &ProcessError) -> BackendError {
    let code = match error {
        ProcessError::Capacity { .. } => "capacity_exhausted",
        ProcessError::UnknownSession { .. } => "unknown_session",
        ProcessError::StdinClosed { .. } => "stdin_closed",
        ProcessError::ShuttingDown => "shutting_down",
        ProcessError::UnsupportedShell { .. } => "unsupported_shell",
        ProcessError::Spawn(_) => "command_launch_failed",
        ProcessError::Interaction(_) => "process_interaction_failed",
    };
    BackendError::new(code, error.to_string())
}

fn store_error(error: &SkillStoreError) -> BackendError {
    let code = match error {
        SkillStoreError::InvalidCursor { .. } => "invalid_cursor",
        SkillStoreError::StaleCursor { .. } => "stale_cursor",
        SkillStoreError::PackageUnavailable => "package_unavailable",
        SkillStoreError::InvalidResource => "invalid_resource",
        _ => "skill_store_failed",
    };
    BackendError::new(code, error.to_string())
}
