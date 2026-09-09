//! Lease-aware routing backend with route fences and generation-affine terminal handles.

mod auth;
pub mod oauth_http;
mod observability;
mod state;
pub use auth::{
    AccessAuthority, AuthConfig, AuthError, AuthStore, AuthorizationGrant, DeviceAuthority,
    PendingDeviceEnrollment, TokenPair,
};
pub use observability::{CallDisposition, CallObservation, MetricsError, Observability};
pub use state::{AllocationKind, StateStore, StateStoreError};

use mcp_agent_tool_contracts::{
    BackendError, BackendFuture, CallContext, CallIdentity, ExecCommandOutput, SkillScope,
    TerminateSessionInput, ToolBackend, ToolOutput, ToolRequest,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BackendKind {
    Local,
    Vps,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteContext {
    pub kind: BackendKind,
    pub workspace_id: String,
    pub generation: u64,
    pub operating_system: String,
    pub privilege_posture: String,
}

#[derive(Clone)]
struct Route {
    context: RouteContext,
    backend: Arc<dyn ToolBackend>,
    launch_instance_id: Option<String>,
    connection_epoch: Option<u64>,
    expires_at: Option<Instant>,
    fence: CancellationToken,
}

struct RoutingState {
    local: Option<Route>,
    superseded_launches: HashSet<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    principal: String,
    session: String,
}

#[derive(Default)]
struct SessionState {
    confirmed: Option<RouteContext>,
    pending: Option<PendingRoute>,
}

struct PendingRoute {
    context: RouteContext,
    blocked_through: u64,
}

#[derive(Default)]
struct SessionSlot {
    next_admission: AtomicU64,
    state: tokio::sync::Mutex<SessionState>,
}

#[derive(Clone)]
struct TerminalHandle {
    principal: String,
    route: RouteContext,
    native_id: i32,
}

#[derive(Clone)]
struct CursorHandle {
    principal: String,
    route: RouteContext,
    native: String,
}

#[derive(Clone)]
struct SkillHandle {
    principal: String,
    route: RouteContext,
    scope: SkillScope,
    native_package: String,
    native_resource: String,
}

pub struct GatewayBackend {
    vps: Mutex<Route>,
    routing: Mutex<RoutingState>,
    sessions: Mutex<HashMap<SessionKey, Arc<SessionSlot>>>,
    terminals: Mutex<HashMap<i32, TerminalHandle>>,
    cursors: Mutex<HashMap<String, CursorHandle>>,
    skills: Mutex<HashMap<String, SkillHandle>>,
    authority_fences: Mutex<HashMap<String, CancellationToken>>,
    next_generation: AtomicU64,
    next_terminal: AtomicI32,
    next_opaque: AtomicU64,
    lease_ttl: Duration,
    state_store: Option<Arc<StateStore>>,
    observability: Option<Arc<Observability>>,
    state_failed: AtomicBool,
}

impl GatewayBackend {
    #[must_use]
    pub fn new(
        vps_backend: Arc<dyn ToolBackend>,
        vps_context: RouteContext,
        lease_ttl: Duration,
    ) -> Self {
        Self {
            vps: Mutex::new(Route {
                context: vps_context,
                backend: vps_backend,
                launch_instance_id: None,
                connection_epoch: None,
                expires_at: None,
                fence: CancellationToken::new(),
            }),
            routing: Mutex::new(RoutingState {
                local: None,
                superseded_launches: HashSet::new(),
            }),
            sessions: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            skills: Mutex::new(HashMap::new()),
            authority_fences: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(2),
            next_terminal: AtomicI32::new(1_000),
            next_opaque: AtomicU64::new(1),
            lease_ttl,
            state_store: None,
            observability: None,
            state_failed: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn with_state_store(mut self, state_store: Arc<StateStore>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    #[must_use]
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        self.observability = Some(observability);
        self
    }

    /// Publishes a freshly connected, containment-verified VPS runner generation.
    ///
    /// # Errors
    ///
    /// Returns an error when durable generation allocation is unavailable.
    pub fn replace_vps(
        &self,
        backend: Arc<dyn ToolBackend>,
        mut context: RouteContext,
    ) -> Result<RouteContext, BackendError> {
        context.generation = self.allocate(AllocationKind::Generation)?;
        self.replace_vps_preallocated(backend, context)
    }

    /// Publishes a relay backend using a generation already reserved durably for its handshake.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied generation is zero.
    pub fn replace_vps_preallocated(
        &self,
        backend: Arc<dyn ToolBackend>,
        mut context: RouteContext,
    ) -> Result<RouteContext, BackendError> {
        self.ensure_state_available()?;
        if context.generation == 0 {
            return Err(BackendError::new(
                "invalid_generation",
                "a preallocated generation must be nonzero",
            ));
        }
        context.kind = BackendKind::Vps;
        let mut vps = self.lock_vps();
        vps.fence.cancel();
        *vps = Route {
            context: context.clone(),
            backend,
            launch_instance_id: None,
            connection_epoch: None,
            expires_at: None,
            fence: CancellationToken::new(),
        };
        if let Some(observability) = &self.observability {
            observability.backend_connected(&context, None);
        }
        Ok(context)
    }

    #[must_use]
    pub fn current_vps_generation(&self) -> u64 {
        self.lock_vps().context.generation
    }

    /// Fences the exact VPS generation after its relay connection is lost.
    ///
    /// Returns `true` only when the supplied generation is still current.
    pub fn fence_vps_generation(&self, generation: u64) -> bool {
        let vps = self.lock_vps();
        if vps.context.generation != generation || vps.fence.is_cancelled() {
            return false;
        }
        vps.fence.cancel();
        if let Some(observability) = &self.observability {
            observability.backend_disconnected(&vps.context, "relay_disconnected");
        }
        true
    }

    /// Commits a launch as the sole local route and permanently supersedes its predecessor.
    ///
    /// # Errors
    ///
    /// Returns an error when a superseded launch identity attempts to register again.
    pub fn register_local(
        &self,
        launch_instance_id: &str,
        connection_epoch: u64,
        backend: Arc<dyn ToolBackend>,
        mut context: RouteContext,
    ) -> Result<LocalLease, BackendError> {
        context.generation = self.allocate(AllocationKind::Generation)?;
        self.register_local_preallocated(launch_instance_id, connection_epoch, backend, context)
    }

    /// Commits a local relay whose generation was reserved before the registration reply.
    ///
    /// # Errors
    ///
    /// Returns an error for zero generations or superseded launch identities.
    pub fn register_local_preallocated(
        &self,
        launch_instance_id: &str,
        connection_epoch: u64,
        backend: Arc<dyn ToolBackend>,
        mut context: RouteContext,
    ) -> Result<LocalLease, BackendError> {
        self.ensure_state_available()?;
        if context.generation == 0 {
            return Err(BackendError::new(
                "invalid_generation",
                "a preallocated generation must be nonzero",
            ));
        }
        if connection_epoch == 0 {
            return Err(BackendError::new(
                "invalid_connection_epoch",
                "a local connection epoch must be nonzero",
            ));
        }
        let mut routing = self.lock_routing();
        self.expire_local(&mut routing, Instant::now());
        if self.is_launch_superseded(&routing, launch_instance_id)? {
            return Err(BackendError::new(
                "launch_superseded",
                "the local launch identity was permanently superseded",
            ));
        }
        if let Some(current) = routing.local.as_ref()
            && current.launch_instance_id.as_deref() == Some(launch_instance_id)
        {
            let current_epoch = current.connection_epoch.ok_or_else(state_unavailable)?;
            if connection_epoch <= current_epoch {
                return Err(BackendError::new(
                    "stale_connection_epoch",
                    "the local connection epoch is not newer than the current connection",
                ));
            }

            let current = routing.local.take().ok_or_else(state_unavailable)?;
            current.fence.cancel();
            if let Some(observability) = &self.observability {
                observability.backend_disconnected(&current.context, "relay_disconnected");
            }
            context.kind = BackendKind::Local;
            let fence = CancellationToken::new();
            routing.local = Some(Route {
                context: context.clone(),
                backend,
                launch_instance_id: Some(launch_instance_id.to_owned()),
                connection_epoch: Some(connection_epoch),
                expires_at: Some(Instant::now() + self.lease_ttl),
                fence: fence.clone(),
            });
            if let Some(observability) = &self.observability {
                observability.backend_connected(&context, Some(launch_instance_id));
            }
            return Ok(LocalLease {
                launch_instance_id: launch_instance_id.to_owned(),
                connection_epoch,
                generation: context.generation,
                fence,
            });
        }
        if let Some(previous_id) = routing
            .local
            .as_ref()
            .and_then(|route| route.launch_instance_id.as_deref())
            .map(str::to_owned)
        {
            self.record_superseded_launch(&mut routing, &previous_id)?;
        }
        if let Some(previous) = routing.local.take() {
            previous.fence.cancel();
            if let Some(observability) = &self.observability {
                observability.backend_disconnected(&previous.context, "lease_lost");
            }
        }
        context.kind = BackendKind::Local;
        let fence = CancellationToken::new();
        routing.local = Some(Route {
            context: context.clone(),
            backend,
            launch_instance_id: Some(launch_instance_id.to_owned()),
            connection_epoch: Some(connection_epoch),
            expires_at: Some(Instant::now() + self.lease_ttl),
            fence: fence.clone(),
        });
        if let Some(observability) = &self.observability {
            observability.backend_connected(&context, Some(launch_instance_id));
        }
        Ok(LocalLease {
            launch_instance_id: launch_instance_id.to_owned(),
            connection_epoch,
            generation: context.generation,
            fence,
        })
    }

    /// Extends only the currently committed local lease.
    ///
    /// # Errors
    ///
    /// Returns an error for stale, superseded, or fenced leases.
    pub fn heartbeat(&self, lease: &LocalLease) -> Result<(), BackendError> {
        self.ensure_state_available()?;
        let mut routing = self.lock_routing();
        self.expire_local(&mut routing, Instant::now());
        let Some(local) = routing.local.as_mut() else {
            return Err(BackendError::new(
                "lease_lost",
                "the local lease is no longer current",
            ));
        };
        if local.context.generation != lease.generation
            || local.launch_instance_id.as_deref() != Some(&lease.launch_instance_id)
            || local.connection_epoch != Some(lease.connection_epoch)
            || lease.fence.is_cancelled()
        {
            return Err(BackendError::new(
                "lease_lost",
                "the local lease is no longer current",
            ));
        }
        local.expires_at = Some(Instant::now() + self.lease_ttl);
        if let Some(observability) = &self.observability {
            observability.local_heartbeat(lease.generation);
        }
        Ok(())
    }

    /// Fences the exact current lease after its external monotonic heartbeat deadline elapses.
    ///
    /// Returns `true` only when this lease was still current and was fenced by this call.
    pub fn expire_local_lease(&self, lease: &LocalLease) -> bool {
        let mut routing = self.lock_routing();
        let is_current = routing.local.as_ref().is_some_and(|local| {
            local.context.generation == lease.generation
                && local.launch_instance_id.as_deref() == Some(&lease.launch_instance_id)
                && local.connection_epoch == Some(lease.connection_epoch)
                && !lease.fence.is_cancelled()
        });
        if !is_current {
            return false;
        }
        self.fence_current_local(&mut routing);
        true
    }

    #[must_use]
    pub fn current_route(&self) -> RouteContext {
        self.default_route().context
    }

    /// Revokes a security principal inside the live routing process.
    ///
    /// Already-running calls receive cancellation and every virtual handle owned by the
    /// principal becomes unusable. The cancelled fence is retained so a request that raced
    /// bearer revocation cannot recreate authority before the HTTP layer observes the database.
    ///
    /// # Errors
    ///
    /// Returns an error unless every still-reachable terminal process acknowledges termination.
    pub async fn revoke_principal(&self, principal: &str) -> Result<(), BackendError> {
        if let Some(observability) = &self.observability {
            observability.principal_revoked(principal);
        }
        let fence = self.authority_fence(principal);
        fence.cancel();
        let terminals = {
            let mut handles = self.lock_terminals();
            let revoked = handles
                .values()
                .filter(|handle| handle.principal == principal)
                .cloned()
                .collect::<Vec<_>>();
            handles.retain(|_, handle| handle.principal != principal);
            revoked
        };
        self.lock_cursors()
            .retain(|_, handle| handle.principal != principal);
        self.lock_skills()
            .retain(|_, handle| handle.principal != principal);
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, _| key.principal != principal);
        let mut terminations = tokio::task::JoinSet::new();
        for terminal in terminals {
            let Some(route) = self.route_for_context(&terminal.route) else {
                continue;
            };
            terminations.spawn(async move {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    route.backend.call(
                        CallContext::new(CancellationToken::new(), None),
                        ToolRequest::TerminateSession(TerminateSessionInput {
                            session_id: terminal.native_id,
                        }),
                    ),
                )
                .await
            });
        }
        while let Some(result) = terminations.join_next().await {
            match result {
                Ok(Ok(Ok(ToolOutput::TerminateSession(output)))) if output.terminated => {}
                _ => {
                    return Err(BackendError::new(
                        "authority_cleanup_failed",
                        "a revoked grant terminal process could not be terminated",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Cancels every observed principal that is absent from durable active grant state.
    ///
    /// # Errors
    ///
    /// Returns an error when terminal cleanup for any revoked principal is unproven.
    pub async fn reconcile_principals(&self, active: &HashSet<String>) -> Result<(), BackendError> {
        let revoked = self
            .authority_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(principal, fence)| !fence.is_cancelled() && !active.contains(*principal))
            .map(|(principal, _)| principal.clone())
            .collect::<Vec<_>>();
        for principal in revoked {
            self.revoke_principal(&principal).await?;
        }
        Ok(())
    }

    fn authority_fence(&self, principal: &str) -> CancellationToken {
        self.authority_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(principal.to_owned())
            .or_default()
            .clone()
    }

    async fn dispatch(
        &self,
        context: CallContext,
        request: ToolRequest,
    ) -> Result<ToolOutput, BackendError> {
        let owner_id = context.identity.as_ref().map_or_else(
            || "local-anonymous".to_owned(),
            |value| value.principal_fingerprint.clone(),
        );
        let observation = self
            .observability
            .as_ref()
            .map(|observability| observability.begin_call(&request, &owner_id));
        let started = Instant::now();
        let mut observed_route = None;
        let result = self
            .dispatch_inner(context, request, &mut observed_route)
            .await;
        if let Some(observation) = observation {
            observation.finish(
                observed_route.as_ref(),
                CallDisposition::from_error(result.as_ref().err()),
                result.as_ref().err(),
                started.elapsed(),
            );
        }
        result
    }

    async fn dispatch_inner(
        &self,
        mut context: CallContext,
        mut request: ToolRequest,
        observed_route: &mut Option<RouteContext>,
    ) -> Result<ToolOutput, BackendError> {
        self.ensure_state_available()?;
        let identity = context.identity.clone().unwrap_or_else(default_identity);
        let authority_fence = self.authority_fence(&identity.principal_fingerprint);
        if authority_fence.is_cancelled() {
            return Err(BackendError::new(
                "authority_revoked",
                "the authenticated grant was revoked",
            ));
        }
        let (route, public_resource) = self.select_route(&identity, &mut request)?;
        *observed_route = Some(route.context.clone());
        self.ensure_state_available()?;
        let key = SessionKey {
            principal: identity.principal_fingerprint.clone(),
            session: identity.session_fingerprint,
        };
        let slot = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                sessions
                    .entry(key)
                    .or_insert_with(|| Arc::new(SessionSlot::default())),
            )
        };
        let admission = slot.next_admission.fetch_add(1, Ordering::SeqCst) + 1;
        let mut session = slot.state.lock().await;
        if session.confirmed.as_ref() != Some(&route.context) {
            if session
                .pending
                .as_ref()
                .is_none_or(|pending| pending.context != route.context)
            {
                // Block every already-admitted ticket so concurrency cannot act as the explicit
                // retry that confirms a changed route.
                tokio::task::yield_now().await;
                session.pending = Some(PendingRoute {
                    context: route.context.clone(),
                    blocked_through: slot.next_admission.load(Ordering::SeqCst),
                });
                return Err(backend_changed(&route.context));
            }
            let pending = session
                .pending
                .as_ref()
                .expect("the matching pending route was just observed");
            if admission <= pending.blocked_through {
                return Err(backend_changed(&route.context));
            }
            session.pending = None;
            session.confirmed = Some(route.context.clone());
        }
        if route.fence.is_cancelled() {
            return Err(BackendError::new(
                "lost_context",
                "the selected backend generation was fenced",
            ));
        }
        let request_cancellation = context.cancellation.clone();
        let call_cancellation = authority_fence.child_token();
        context.cancellation = call_cancellation.clone();
        let cancellation_forwarder = tokio::spawn(async move {
            request_cancellation.cancelled().await;
            call_cancellation.cancel();
        });
        let result = route.backend.call(context, request).await;
        cancellation_forwarder.abort();
        let mut output = result?;
        if route.fence.is_cancelled() {
            return Err(BackendError {
                code: "backend_lost",
                message: "the backend was lost after dispatch; the outcome is unknown".to_owned(),
                details: Some(json!({ "outcome": "unknown", "dispatched": true })),
            });
        }
        match &mut output {
            ToolOutput::ExecCommand(value) => {
                self.virtualize_terminal(&identity.principal_fingerprint, &route.context, value)?;
            }
            ToolOutput::SkillsList(value) => {
                self.virtualize_skill_list(&identity.principal_fingerprint, &route.context, value)?;
            }
            ToolOutput::SkillsRead(value) => {
                if let Some(resource) = public_resource {
                    value.resource = resource;
                }
                self.virtualize_cursor(
                    &identity.principal_fingerprint,
                    &route.context,
                    &mut value.next_cursor,
                )?;
            }
            ToolOutput::WriteStdin(_)
            | ToolOutput::ApplyPatch(_)
            | ToolOutput::TerminateSession(_) => {}
        }
        Ok(output)
    }

    fn select_route(
        &self,
        identity: &CallIdentity,
        request: &mut ToolRequest,
    ) -> Result<(Route, Option<String>), BackendError> {
        let mut public_resource = None;
        let route = match request {
            ToolRequest::WriteStdin(input) => {
                let handle = self
                    .lock_terminals()
                    .get(&input.session_id)
                    .cloned()
                    .ok_or_else(|| {
                        BackendError::new(
                            "unknown_session",
                            "the public terminal session is unknown",
                        )
                    })?;
                if handle.principal != identity.principal_fingerprint {
                    return Err(BackendError::new(
                        "unknown_session",
                        "the public terminal session is unknown",
                    ));
                }
                input.session_id = handle.native_id;
                self.route_for_context(&handle.route).ok_or_else(|| {
                    BackendError::new(
                        "lost_context",
                        "the terminal backend generation is no longer available",
                    )
                })?
            }
            ToolRequest::SkillsList(input) if input.cursor.is_some() => {
                let public = input.cursor.take().expect("cursor presence checked");
                let handle = self.lock_cursors().get(&public).cloned().ok_or_else(|| {
                    BackendError::new("invalid_cursor", "the gateway skill cursor is invalid")
                })?;
                if handle.principal != identity.principal_fingerprint {
                    return Err(BackendError::new(
                        "invalid_cursor",
                        "the gateway skill cursor is invalid",
                    ));
                }
                input.cursor = Some(handle.native);
                self.route_for_context(&handle.route).ok_or_else(|| {
                    BackendError::new(
                        "lost_context",
                        "the skill cursor backend generation is no longer available",
                    )
                })?
            }
            ToolRequest::SkillsRead(input) if input.scope != SkillScope::System => {
                let public = input.resource.clone();
                let handle = self.lock_skills().get(&public).cloned().ok_or_else(|| {
                    BackendError::new("invalid_resource", "the gateway skill resource is invalid")
                })?;
                if handle.principal != identity.principal_fingerprint
                    || handle.scope != input.scope
                    || input.package != public_package(&public)
                {
                    return Err(BackendError::new(
                        "invalid_resource",
                        "the gateway skill resource is invalid",
                    ));
                }
                input.package = handle.native_package;
                input.resource = handle.native_resource;
                if let Some(cursor) = input.cursor.take() {
                    let cursor = self.lock_cursors().get(&cursor).cloned().ok_or_else(|| {
                        BackendError::new("invalid_cursor", "the gateway skill cursor is invalid")
                    })?;
                    if cursor.principal != identity.principal_fingerprint
                        || cursor.route != handle.route
                    {
                        return Err(BackendError::new(
                            "invalid_cursor",
                            "the gateway skill cursor is invalid",
                        ));
                    }
                    input.cursor = Some(cursor.native);
                }
                public_resource = Some(public);
                self.route_for_context(&handle.route).ok_or_else(|| {
                    BackendError::new(
                        "lost_context",
                        "the skill resource backend generation is no longer available",
                    )
                })?
            }
            _ => self.available_default_route()?,
        };
        Ok((route, public_resource))
    }

    fn virtualize_terminal(
        &self,
        principal: &str,
        route: &RouteContext,
        output: &mut ExecCommandOutput,
    ) -> Result<(), BackendError> {
        let Some(native_id) = output.session_id else {
            return Ok(());
        };
        let public_id = i32::try_from(self.allocate(AllocationKind::Terminal)?).map_err(|_| {
            BackendError::new(
                "allocator_exhausted",
                "the public terminal ID space is exhausted",
            )
        })?;
        self.lock_terminals().insert(
            public_id,
            TerminalHandle {
                principal: principal.to_owned(),
                route: route.clone(),
                native_id,
            },
        );
        output.session_id = Some(public_id);
        Ok(())
    }

    fn virtualize_skill_list(
        &self,
        principal: &str,
        route: &RouteContext,
        output: &mut mcp_agent_tool_contracts::SkillListOutput,
    ) -> Result<(), BackendError> {
        for skill in &mut output.skills {
            if skill.scope == SkillScope::System {
                continue;
            }
            let id = self.allocate(AllocationKind::Opaque)?;
            let public_resource = format!("gateway-skill://{id}");
            let public_package = public_package(&public_resource);
            self.lock_skills().insert(
                public_resource.clone(),
                SkillHandle {
                    principal: principal.to_owned(),
                    route: route.clone(),
                    scope: skill.scope,
                    native_package: skill.package.clone(),
                    native_resource: skill.main_resource.clone(),
                },
            );
            skill.package = public_package;
            skill.main_resource = public_resource;
        }
        self.virtualize_cursor(principal, route, &mut output.next_cursor)
    }

    fn virtualize_cursor(
        &self,
        principal: &str,
        route: &RouteContext,
        cursor: &mut Option<String>,
    ) -> Result<(), BackendError> {
        let Some(native) = cursor.take() else {
            return Ok(());
        };
        let public = format!("gateway-cursor-{}", self.allocate(AllocationKind::Opaque)?);
        self.lock_cursors().insert(
            public.clone(),
            CursorHandle {
                principal: principal.to_owned(),
                route: route.clone(),
                native,
            },
        );
        *cursor = Some(public);
        Ok(())
    }

    fn allocate(&self, kind: AllocationKind) -> Result<u64, BackendError> {
        if let Some(store) = &self.state_store {
            return store.allocate(kind).map_err(|_| {
                BackendError::new("state_unavailable", "durable gateway state is unavailable")
            });
        }
        Ok(match kind {
            AllocationKind::Generation => self.next_generation.fetch_add(1, Ordering::SeqCst),
            AllocationKind::Terminal => {
                u64::try_from(self.next_terminal.fetch_add(1, Ordering::SeqCst)).map_err(|_| {
                    BackendError::new(
                        "allocator_exhausted",
                        "the public terminal ID space is exhausted",
                    )
                })?
            }
            AllocationKind::Opaque => self.next_opaque.fetch_add(1, Ordering::SeqCst),
        })
    }

    fn default_route(&self) -> Route {
        let mut routing = self.lock_routing();
        self.expire_local(&mut routing, Instant::now());
        routing
            .local
            .as_ref()
            .filter(|route| !route.fence.is_cancelled())
            .cloned()
            .unwrap_or_else(|| self.lock_vps().clone())
    }

    fn available_default_route(&self) -> Result<Route, BackendError> {
        let route = self.default_route();
        if route.fence.is_cancelled() {
            return Err(BackendError::new(
                "no_backend",
                "no eligible execution backend is available",
            ));
        }
        Ok(route)
    }

    fn route_for_context(&self, context: &RouteContext) -> Option<Route> {
        let vps = self.lock_vps().clone();
        if &vps.context == context {
            return (!vps.fence.is_cancelled()).then_some(vps);
        }
        let mut routing = self.lock_routing();
        self.expire_local(&mut routing, Instant::now());
        routing
            .local
            .as_ref()
            .filter(|route| &route.context == context && !route.fence.is_cancelled())
            .cloned()
    }

    fn expire_local(&self, routing: &mut RoutingState, now: Instant) {
        let expired = routing
            .local
            .as_ref()
            .and_then(|route| route.expires_at)
            .is_some_and(|expiry| expiry <= now);
        if !expired {
            return;
        }
        self.fence_current_local(routing);
    }

    fn fence_current_local(&self, routing: &mut RoutingState) {
        if let Some(launch_instance_id) = routing
            .local
            .as_ref()
            .and_then(|route| route.launch_instance_id.as_deref())
            .map(str::to_owned)
            && self
                .record_superseded_launch(routing, &launch_instance_id)
                .is_err()
        {
            self.state_failed.store(true, Ordering::SeqCst);
        }
        if let Some(route) = routing.local.take() {
            if let Some(observability) = &self.observability {
                observability.backend_disconnected(&route.context, "lease_expired");
            }
            route.fence.cancel();
        }
    }

    fn is_launch_superseded(
        &self,
        routing: &RoutingState,
        launch_instance_id: &str,
    ) -> Result<bool, BackendError> {
        if routing.superseded_launches.contains(launch_instance_id) {
            return Ok(true);
        }
        self.state_store.as_ref().map_or(Ok(false), |store| {
            store
                .is_launch_superseded(launch_instance_id)
                .map_err(|_| state_unavailable())
        })
    }

    fn record_superseded_launch(
        &self,
        routing: &mut RoutingState,
        launch_instance_id: &str,
    ) -> Result<(), BackendError> {
        if routing.superseded_launches.contains(launch_instance_id) {
            return Ok(());
        }
        if let Some(store) = &self.state_store {
            store
                .record_superseded_launch(launch_instance_id)
                .map_err(|_| state_unavailable())?;
        } else if routing.superseded_launches.len() >= StateStore::MAX_SUPERSEDED_LAUNCHES {
            return Err(state_unavailable());
        }
        routing
            .superseded_launches
            .insert(launch_instance_id.to_owned());
        Ok(())
    }

    fn ensure_state_available(&self) -> Result<(), BackendError> {
        if self.state_failed.load(Ordering::SeqCst) {
            return Err(state_unavailable());
        }
        Ok(())
    }

    fn lock_routing(&self) -> MutexGuard<'_, RoutingState> {
        self.routing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_vps(&self) -> MutexGuard<'_, Route> {
        self.vps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn lock_terminals(&self) -> MutexGuard<'_, HashMap<i32, TerminalHandle>> {
        self.terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn lock_cursors(&self) -> MutexGuard<'_, HashMap<String, CursorHandle>> {
        self.cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn lock_skills(&self) -> MutexGuard<'_, HashMap<String, SkillHandle>> {
        self.skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn public_package(resource: &str) -> String {
    resource.strip_prefix("gateway-skill://").map_or_else(
        || "invalid".to_owned(),
        |id| format!("gateway-package-{id}"),
    )
}

fn default_identity() -> CallIdentity {
    CallIdentity {
        principal_fingerprint: "local-anonymous".to_owned(),
        session_fingerprint: "local-default".to_owned(),
    }
}

impl ToolBackend for GatewayBackend {
    fn call(&self, context: CallContext, request: ToolRequest) -> BackendFuture<'_> {
        Box::pin(self.dispatch(context, request))
    }
}

#[derive(Clone, Debug)]
pub struct LocalLease {
    pub launch_instance_id: String,
    pub connection_epoch: u64,
    pub generation: u64,
    fence: CancellationToken,
}

impl LocalLease {
    #[must_use]
    pub fn is_fenced(&self) -> bool {
        self.fence.is_cancelled()
    }

    /// Resolves when a newer connection or launch permanently fences this lease.
    pub async fn cancelled(&self) {
        self.fence.cancelled().await;
    }
}

fn backend_changed(context: &RouteContext) -> BackendError {
    BackendError {
        code: "backend_changed",
        message: "execution backend changed; repeat the call to confirm this context".to_owned(),
        details: Some(
            json!({ "backend": match context.kind { BackendKind::Local => "local", BackendKind::Vps => "vps" }, "workspace_id": context.workspace_id, "generation": context.generation, "operating_system": context.operating_system, "privilege_posture": context.privilege_posture }),
        ),
    }
}

fn state_unavailable() -> BackendError {
    BackendError::new("state_unavailable", "durable gateway state is unavailable")
}
