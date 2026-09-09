use mcp_agent_gateway::{BackendKind, GatewayBackend, RouteContext};
use mcp_agent_tool_contracts::{
    ApplyPatchOutput, BackendFuture, CallContext, CallIdentity, ExecCommandInput,
    ExecCommandOutput, ListedSkill, SkillAuthority, SkillListInput, SkillListOutput,
    SkillReadInput, SkillReadOutput, SkillScope, SkillSource, SkillSourceKind, ToolBackend,
    ToolOutput, ToolRequest, WriteStdinInput,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct MatrixBackend {
    name: &'static str,
    observed: Mutex<Vec<&'static str>>,
}

impl MatrixBackend {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            observed: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, tool: &'static str) {
        self.observed.lock().unwrap().push(tool);
    }
}

impl ToolBackend for MatrixBackend {
    fn call(&self, _context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            match request {
                ToolRequest::ExecCommand(_) => {
                    self.record("exec_command");
                    Ok(ToolOutput::ExecCommand(output(self.name, Some(41))))
                }
                ToolRequest::WriteStdin(input) => {
                    assert_eq!(input.session_id, 41);
                    self.record("write_stdin");
                    Ok(ToolOutput::WriteStdin(output(self.name, Some(41))))
                }
                ToolRequest::ApplyPatch(_) => {
                    self.record("apply_patch");
                    Ok(ToolOutput::ApplyPatch(ApplyPatchOutput {
                        output: self.name.to_owned(),
                    }))
                }
                ToolRequest::SkillsList(_) => {
                    self.record("skills.list");
                    Ok(ToolOutput::SkillsList(SkillListOutput {
                        skills: vec![ListedSkill {
                            authority: SkillAuthority::Host,
                            scope: SkillScope::Project,
                            package: "native-package".to_owned(),
                            name: "matrix".to_owned(),
                            description: "matrix fixture".to_owned(),
                            main_resource: "skill://host/project/native-package/SKILL.md"
                                .to_owned(),
                            source: SkillSource {
                                kind: SkillSourceKind::Host,
                                repository: None,
                                commit: None,
                                selector: None,
                            },
                        }],
                        warnings: Vec::new(),
                        next_cursor: None,
                    }))
                }
                ToolRequest::SkillsRead(input) => {
                    assert_eq!(input.package, "native-package");
                    assert_eq!(
                        input.resource,
                        "skill://host/project/native-package/SKILL.md"
                    );
                    self.record("skills.read");
                    Ok(ToolOutput::SkillsRead(SkillReadOutput {
                        resource: input.resource,
                        contents: self.name.to_owned(),
                        next_cursor: None,
                    }))
                }
                ToolRequest::TerminateSession(_) => unreachable!("not model-visible"),
            }
        })
    }
}

fn route(kind: BackendKind, os: &str, generation: u64) -> RouteContext {
    RouteContext {
        kind,
        workspace_id: format!("opaque-{os}-workspace"),
        generation,
        operating_system: os.to_owned(),
        privilege_posture: if kind == BackendKind::Vps {
            "rootless-podman-verified"
        } else if os == "windows" {
            "windows-restricted-token-job-verified"
        } else {
            "macos-seatbelt-verified"
        }
        .to_owned(),
    }
}

fn context(session: &str) -> CallContext {
    CallContext::new(CancellationToken::new(), None).with_identity(CallIdentity {
        principal_fingerprint: "owner-fingerprint".to_owned(),
        session_fingerprint: session.to_owned(),
    })
}

async fn call(gateway: &GatewayBackend, session: &str, request: ToolRequest) -> ToolOutput {
    gateway.call(context(session), request).await.unwrap()
}

async fn confirm(gateway: &GatewayBackend, session: &str, request: ToolRequest) -> ToolOutput {
    assert_eq!(
        gateway
            .call(context(session), request.clone())
            .await
            .unwrap_err()
            .code,
        "backend_changed"
    );
    call(gateway, session, request).await
}

async fn exercise_five_tools(gateway: &GatewayBackend, session: &str, needs_fence: bool) -> i32 {
    let exec = ToolRequest::ExecCommand(ExecCommandInput {
        cmd: "printf matrix".to_owned(),
        workdir: None,
        tty: true,
        yield_time_ms: 10,
        max_output_tokens: None,
        shell: None,
        login: None,
    });
    let ToolOutput::ExecCommand(exec) = (if needs_fence {
        confirm(gateway, session, exec).await
    } else {
        call(gateway, session, exec).await
    }) else {
        panic!("wrong exec output")
    };
    let public_session = exec.session_id.unwrap();
    call(
        gateway,
        session,
        ToolRequest::WriteStdin(WriteStdinInput {
            session_id: public_session,
            chars: "input".to_owned(),
            yield_time_ms: 10,
            max_output_tokens: None,
        }),
    )
    .await;
    call(
        gateway,
        session,
        ToolRequest::ApplyPatch(mcp_agent_tool_contracts::ApplyPatchInput {
            patch: "*** Begin Patch\n*** End Patch".to_owned(),
        }),
    )
    .await;
    let ToolOutput::SkillsList(listed) = call(
        gateway,
        session,
        ToolRequest::SkillsList(SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        }),
    )
    .await
    else {
        panic!("wrong list output")
    };
    let skill = &listed.skills[0];
    call(
        gateway,
        session,
        ToolRequest::SkillsRead(SkillReadInput {
            scope: SkillScope::Project,
            package: skill.package.clone(),
            resource: skill.main_resource.clone(),
            cursor: None,
        }),
    )
    .await;
    public_session
}

fn output(name: &str, session_id: Option<i32>) -> ExecCommandOutput {
    ExecCommandOutput {
        chunk_id: None,
        wall_time_seconds: 0.0,
        exit_code: Some(0),
        session_id,
        original_token_count: Some(1),
        output: name.to_owned(),
    }
}

#[tokio::test]
async fn five_tool_matrix_matches_for_vps_macos_and_windows_routes() {
    for local_os in ["macos", "windows"] {
        let vps = Arc::new(MatrixBackend::new("vps"));
        let local = Arc::new(MatrixBackend::new("local"));
        let gateway = GatewayBackend::new(
            vps.clone(),
            route(BackendKind::Vps, "linux", 1),
            Duration::from_secs(4),
        );

        let vps_session = format!("matrix-vps-{local_os}");
        let vps_handle = exercise_five_tools(&gateway, &vps_session, true).await;
        let lease = gateway
            .register_local_preallocated(
                &format!("launch-{local_os}"),
                1,
                local.clone(),
                route(BackendKind::Local, local_os, 2),
            )
            .unwrap();
        let local_session = format!("matrix-local-{local_os}");
        exercise_five_tools(&gateway, &local_session, true).await;

        call(
            &gateway,
            &vps_session,
            ToolRequest::WriteStdin(WriteStdinInput::poll(vps_handle)),
        )
        .await;
        assert!(gateway.expire_local_lease(&lease));
        confirm(
            &gateway,
            &local_session,
            ToolRequest::SkillsList(SkillListInput {
                scope: SkillScope::System,
                cursor: None,
            }),
        )
        .await;

        assert_eq!(
            *local.observed.lock().unwrap(),
            [
                "exec_command",
                "write_stdin",
                "apply_patch",
                "skills.list",
                "skills.read"
            ]
        );
        assert_eq!(vps.observed.lock().unwrap().len(), 7);
    }
}
