//! Privacy-safe structured events and bounded process metrics.

use crate::{BackendKind, RouteContext};
use mcp_agent_tool_contracts::{BackendError, ToolRequest};
use rand::RngCore as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_RECENT_EVENTS: usize = 256;
const MAX_HOST_METRICS_BYTES: usize = 64 * 1024;
const MAX_HOST_METRIC_LINES: usize = 64;
const LATENCY_BUCKETS_MS: [u64; 11] = [1, 5, 10, 25, 50, 100, 250, 500, 1_000, 5_000, 300_000];
const TOOL_NAMES: [&str; 6] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "terminate_session",
    "skills_list",
    "skills_read",
];

const HOST_METRIC_NAMES: [&str; 22] = [
    "tools_mcp_sqlite_integrity_ok",
    "tools_mcp_sqlite_schema_version",
    "tools_mcp_backup_age_seconds",
    "tools_mcp_tls_certificate_lifetime_seconds",
    "tools_mcp_workload_bytes_used",
    "tools_mcp_workload_bytes_limit",
    "tools_mcp_workload_inodes_used",
    "tools_mcp_workload_inodes_limit",
    "tools_mcp_host_bytes_available",
    "tools_mcp_host_inodes_available",
    "tools_mcp_cgroup_memory_oom_total",
    "tools_mcp_cgroup_pids_events_total",
    "tools_mcp_cgroup_cpu_pressure_micros_total",
    "tools_mcp_journal_bytes",
    "tools_mcp_gateway_restart_count",
    "tools_mcp_external_ssh_probe_ok",
    "tools_mcp_external_vpn_probe_ok",
    "tools_mcp_yandex_mail_route_probe_ok",
    "tools_mcp_docker_baseline_match",
    "tools_mcp_amnezia_baseline_match",
    "tools_mcp_shared_route_matrix_ok",
    "tools_mcp_collector_ok",
];

/// Fixed dispatch states permitted in structured logs and metric labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDisposition {
    Completed,
    BackendChanged,
    NotDispatched,
    OutcomeUnknown,
    Cancelled,
    Failed,
}

impl CallDisposition {
    const ALL: [Self; 6] = [
        Self::Completed,
        Self::BackendChanged,
        Self::NotDispatched,
        Self::OutcomeUnknown,
        Self::Cancelled,
        Self::Failed,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::BackendChanged => "backend_changed",
            Self::NotDispatched => "not_dispatched",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Completed => 0,
            Self::BackendChanged => 1,
            Self::NotDispatched => 2,
            Self::OutcomeUnknown => 3,
            Self::Cancelled => 4,
            Self::Failed => 5,
        }
    }

    #[must_use]
    pub fn from_result(result: &Result<(), BackendError>) -> Self {
        match result {
            Ok(()) => Self::Completed,
            Err(error) => match error.code {
                "backend_changed" => Self::BackendChanged,
                "not_dispatched" | "no_backend" => Self::NotDispatched,
                "outcome_unknown" | "backend_lost" => Self::OutcomeUnknown,
                "cancelled" | "request_cancelled" => Self::Cancelled,
                _ => Self::Failed,
            },
        }
    }

    #[must_use]
    pub fn from_error(error: Option<&BackendError>) -> Self {
        match error {
            None => Self::Completed,
            Some(error) => match error.code {
                "backend_changed" => Self::BackendChanged,
                "not_dispatched" | "no_backend" => Self::NotDispatched,
                "outcome_unknown" | "backend_lost" => Self::OutcomeUnknown,
                "cancelled" | "request_cancelled" => Self::Cancelled,
                _ => Self::Failed,
            },
        }
    }
}

/// A rejected host metric file. Its display text never includes file contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MetricsError {
    #[error("host metrics exceed the fixed size limit")]
    Oversized,
    #[error("host metrics contain a non-allowlisted line")]
    InvalidLine,
}

/// Process-local telemetry with a fixed schema and no payload-bearing fields.
pub struct Observability {
    emit_stderr: bool,
    opaque_salt: [u8; 32],
    process_start_unix: u64,
    process_start: Instant,
    ready: AtomicBool,
    vps_eligible: AtomicBool,
    local_eligible: AtomicBool,
    vps_generation: AtomicU64,
    local_generation: AtomicU64,
    local_lease_heartbeat_ms: AtomicU64,
    relay_connections: AtomicU64,
    relay_rejections: AtomicU64,
    relay_queue_drops: AtomicU64,
    calls_in_flight: AtomicU64,
    call_sequence: AtomicU64,
    call_counts: [AtomicU64; TOOL_NAMES.len()],
    dispositions: [AtomicU64; CallDisposition::ALL.len()],
    duration_buckets: [AtomicU64; LATENCY_BUCKETS_MS.len()],
    duration_count: AtomicU64,
    duration_sum_ms: AtomicU64,
    http_requests: AtomicU64,
    http_5xx: AtomicU64,
    http_duration_buckets: [AtomicU64; LATENCY_BUCKETS_MS.len()],
    http_duration_count: AtomicU64,
    http_duration_sum_ms: AtomicU64,
    refresh_success: AtomicU64,
    refresh_failure: AtomicU64,
    events_dropped: AtomicU64,
    recent_events: Mutex<VecDeque<String>>,
}

impl Default for Observability {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Observability {
    #[must_use]
    pub fn new(emit_stderr: bool) -> Self {
        let mut opaque_salt = [0_u8; 32];
        rand::rng().fill_bytes(&mut opaque_salt);
        Self {
            emit_stderr,
            opaque_salt,
            process_start_unix: unix_seconds(),
            process_start: Instant::now(),
            ready: AtomicBool::new(false),
            vps_eligible: AtomicBool::new(false),
            local_eligible: AtomicBool::new(false),
            vps_generation: AtomicU64::new(0),
            local_generation: AtomicU64::new(0),
            local_lease_heartbeat_ms: AtomicU64::new(0),
            relay_connections: AtomicU64::new(0),
            relay_rejections: AtomicU64::new(0),
            relay_queue_drops: AtomicU64::new(0),
            calls_in_flight: AtomicU64::new(0),
            call_sequence: AtomicU64::new(0),
            call_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            dispositions: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_count: AtomicU64::new(0),
            duration_sum_ms: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            http_5xx: AtomicU64::new(0),
            http_duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            http_duration_count: AtomicU64::new(0),
            http_duration_sum_ms: AtomicU64::new(0),
            refresh_success: AtomicU64::new(0),
            refresh_failure: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
            recent_events: Mutex::new(VecDeque::with_capacity(MAX_RECENT_EVENTS)),
        }
    }

    #[must_use]
    pub fn test_instance() -> Self {
        Self::new(false)
    }

    pub fn gateway_started(&self) {
        self.emit(&Event::simple("gateway_started"));
    }

    pub fn set_ready(&self, ready: bool) {
        let previous = self.ready.swap(ready, Ordering::AcqRel);
        if previous != ready {
            self.emit(&Event::state("gateway_readiness", ready));
        }
    }

    pub fn backend_connected(&self, route: &RouteContext, lease_id: Option<&str>) {
        let (eligible, generation) = self.backend_state(route.kind);
        eligible.store(true, Ordering::Release);
        generation.store(route.generation, Ordering::Release);
        if route.kind == BackendKind::Local {
            self.local_lease_heartbeat_ms
                .store(self.elapsed_millis(), Ordering::Release);
        }
        self.emit(&Event::backend(
            "backend_connected",
            route,
            lease_id.map(|value| self.opaque(value)),
            self.opaque(&route.workspace_id),
        ));
    }

    pub fn backend_disconnected(&self, route: &RouteContext, error_class: &'static str) {
        let (eligible, generation) = self.backend_state(route.kind);
        if generation.load(Ordering::Acquire) == route.generation {
            eligible.store(false, Ordering::Release);
        }
        self.emit(&Event::backend_error(
            "backend_disconnected",
            route,
            safe_error_class(error_class),
            self.opaque(&route.workspace_id),
        ));
    }

    pub fn local_heartbeat(&self, generation: u64) {
        if self.local_generation.load(Ordering::Acquire) == generation {
            self.local_lease_heartbeat_ms
                .store(self.elapsed_millis(), Ordering::Release);
        }
    }

    pub fn relay_connection_opened(&self) {
        self.relay_connections.fetch_add(1, Ordering::AcqRel);
    }

    pub fn relay_connection_closed(&self) {
        let _ = self
            .relay_connections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.saturating_sub(1))
            });
    }

    pub fn record_relay_rejection(&self, queue_drop: bool, error_class: &'static str) {
        self.relay_rejections.fetch_add(1, Ordering::Relaxed);
        if queue_drop {
            self.relay_queue_drops.fetch_add(1, Ordering::Relaxed);
        }
        self.emit(&Event::error(
            "relay_rejected",
            safe_error_class(error_class),
        ));
    }

    pub fn record_refresh(&self, success: bool, error_class: Option<&'static str>) {
        if success {
            self.refresh_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.refresh_failure.fetch_add(1, Ordering::Relaxed);
        }
        self.emit(&Event::refresh(
            success,
            error_class.map_or("none", safe_error_class),
        ));
    }

    pub fn record_http(&self, status: u16, duration: Duration) {
        self.http_requests.fetch_add(1, Ordering::Relaxed);
        if status >= 500 {
            self.http_5xx.fetch_add(1, Ordering::Relaxed);
        }
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.http_duration_count.fetch_add(1, Ordering::Relaxed);
        self.http_duration_sum_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        for (index, upper) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if duration_ms <= *upper {
                self.http_duration_buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn principal_revoked(&self, owner_id: &str) {
        self.emit(&Event::owner("principal_revoked", self.opaque(owner_id)));
    }

    #[must_use]
    pub fn begin_call<'a>(&'a self, request: &ToolRequest, owner_id: &str) -> CallObservation<'a> {
        let tool_index = tool_index(request);
        self.calls_in_flight.fetch_add(1, Ordering::AcqRel);
        self.call_counts[tool_index].fetch_add(1, Ordering::Relaxed);
        let sequence = self.call_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        CallObservation {
            telemetry: self,
            tool_index,
            owner_id: self.opaque(owner_id),
            call_id: format!("call-{sequence}"),
            finished: false,
        }
    }

    #[must_use]
    pub fn recent_events(&self) -> Vec<String> {
        self.lock_events().iter().cloned().collect()
    }

    /// Renders process metrics and a strictly validated root-owned host snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if host data is oversized, labelled, non-numeric, or not allowlisted.
    pub fn render_metrics(&self, host_metrics: Option<&str>) -> Result<String, MetricsError> {
        let mut output = String::with_capacity(4_096);
        self.render_runtime_metrics(&mut output);
        self.render_call_metrics(&mut output);
        self.render_http_metrics(&mut output);
        if let Some(host_metrics) = host_metrics {
            output.push_str(&validate_host_metrics(host_metrics)?);
        }
        Ok(output)
    }

    fn render_runtime_metrics(&self, output: &mut String) {
        metric(
            output,
            "tools_mcp_process_start_time_seconds",
            self.process_start_unix,
        );
        metric_bool(
            output,
            "tools_mcp_gateway_ready",
            self.ready.load(Ordering::Acquire),
        );
        backend_metrics(
            output,
            "vps",
            self.vps_eligible.load(Ordering::Acquire),
            self.vps_generation.load(Ordering::Acquire),
        );
        backend_metrics(
            output,
            "local",
            self.local_eligible.load(Ordering::Acquire),
            self.local_generation.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_local_lease_age_seconds",
            self.local_lease_age_seconds(),
        );
        metric(
            output,
            "tools_mcp_relay_connections",
            self.relay_connections.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_relay_rejections_total",
            self.relay_rejections.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_relay_queue_drops_total",
            self.relay_queue_drops.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_calls_in_flight",
            self.calls_in_flight.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_observability_events_dropped_total",
            self.events_dropped.load(Ordering::Acquire),
        );
    }

    fn render_call_metrics(&self, output: &mut String) {
        for (index, name) in TOOL_NAMES.iter().enumerate() {
            labelled_metric(
                output,
                "tools_mcp_calls_total",
                "tool",
                name,
                self.call_counts[index].load(Ordering::Acquire),
            );
        }
        for disposition in CallDisposition::ALL {
            labelled_metric(
                output,
                "tools_mcp_call_disposition_total",
                "disposition",
                disposition.name(),
                self.dispositions[disposition.index()].load(Ordering::Acquire),
            );
        }
        for (index, upper) in LATENCY_BUCKETS_MS.iter().enumerate() {
            labelled_metric(
                output,
                "tools_mcp_call_duration_ms_bucket",
                "le",
                &upper.to_string(),
                self.duration_buckets[index].load(Ordering::Acquire),
            );
        }
        metric(
            output,
            "tools_mcp_call_duration_ms_count",
            self.duration_count.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_call_duration_ms_sum",
            self.duration_sum_ms.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_call_duration_p95_ms",
            histogram_p95(
                &self.duration_buckets,
                self.duration_count.load(Ordering::Acquire),
            ),
        );
    }

    fn render_http_metrics(&self, output: &mut String) {
        metric(
            output,
            "tools_mcp_gateway_http_requests_total",
            self.http_requests.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_gateway_http_5xx_total",
            self.http_5xx.load(Ordering::Acquire),
        );
        for (index, upper) in LATENCY_BUCKETS_MS.iter().enumerate() {
            labelled_metric(
                output,
                "tools_mcp_gateway_http_duration_ms_bucket",
                "le",
                &upper.to_string(),
                self.http_duration_buckets[index].load(Ordering::Acquire),
            );
        }
        metric(
            output,
            "tools_mcp_gateway_http_duration_ms_count",
            self.http_duration_count.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_gateway_http_duration_ms_sum",
            self.http_duration_sum_ms.load(Ordering::Acquire),
        );
        metric(
            output,
            "tools_mcp_gateway_http_p95_ms",
            histogram_p95(
                &self.http_duration_buckets,
                self.http_duration_count.load(Ordering::Acquire),
            ),
        );
        labelled_metric(
            output,
            "tools_mcp_oauth_refresh_total",
            "outcome",
            "success",
            self.refresh_success.load(Ordering::Acquire),
        );
        labelled_metric(
            output,
            "tools_mcp_oauth_refresh_total",
            "outcome",
            "failure",
            self.refresh_failure.load(Ordering::Acquire),
        );
    }

    fn finish_call(
        &self,
        observation: &CallObservation<'_>,
        route: Option<&RouteContext>,
        disposition: CallDisposition,
        error: Option<&BackendError>,
        duration: Duration,
    ) {
        self.calls_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.dispositions[disposition.index()].fetch_add(1, Ordering::Relaxed);
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.duration_count.fetch_add(1, Ordering::Relaxed);
        self.duration_sum_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        for (index, upper) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if duration_ms <= *upper {
                self.duration_buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.emit(&Event::call(
            &observation.owner_id,
            &observation.call_id,
            TOOL_NAMES[observation.tool_index],
            route,
            disposition,
            error.map_or("none", |value| safe_error_class(value.code)),
            duration_ms,
            route.map(|value| self.opaque(&value.workspace_id)),
        ));
    }

    fn emit(&self, event: &Event) {
        let Ok(encoded) = serde_json::to_string(event) else {
            self.events_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if self.emit_stderr {
            eprintln!("{encoded}");
        }
        let mut events = self.lock_events();
        if events.len() == MAX_RECENT_EVENTS {
            events.pop_front();
            self.events_dropped.fetch_add(1, Ordering::Relaxed);
        }
        events.push_back(encoded);
    }

    fn opaque(&self, value: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(self.opaque_salt);
        digest.update(value.as_bytes());
        digest
            .finalize()
            .iter()
            .take(12)
            .fold(String::with_capacity(24), |mut output, byte| {
                write!(output, "{byte:02x}").expect("write to String cannot fail");
                output
            })
    }

    fn backend_state(&self, kind: BackendKind) -> (&AtomicBool, &AtomicU64) {
        match kind {
            BackendKind::Local => (&self.local_eligible, &self.local_generation),
            BackendKind::Vps => (&self.vps_eligible, &self.vps_generation),
        }
    }

    fn elapsed_millis(&self) -> u64 {
        u64::try_from(self.process_start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn local_lease_age_seconds(&self) -> u64 {
        let last = self.local_lease_heartbeat_ms.load(Ordering::Acquire);
        if last == 0 || !self.local_eligible.load(Ordering::Acquire) {
            return 0;
        }
        self.elapsed_millis().saturating_sub(last) / 1_000
    }

    fn lock_events(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.recent_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub struct CallObservation<'a> {
    telemetry: &'a Observability,
    tool_index: usize,
    owner_id: String,
    call_id: String,
    finished: bool,
}

impl CallObservation<'_> {
    pub fn finish(
        mut self,
        route: Option<&RouteContext>,
        disposition: CallDisposition,
        error: Option<&BackendError>,
        duration: Duration,
    ) {
        self.telemetry
            .finish_call(&self, route, disposition, error, duration);
        self.finished = true;
    }
}

impl Drop for CallObservation<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.telemetry
            .finish_call(self, None, CallDisposition::Cancelled, None, Duration::ZERO);
    }
}

#[derive(Serialize)]
struct Event {
    #[serde(rename = "event")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disposition: Option<CallDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<bool>,
}

impl Event {
    const fn simple(event: &'static str) -> Self {
        Self::empty(event)
    }

    fn state(event: &'static str, state: bool) -> Self {
        Self {
            state: Some(state),
            ..Self::empty(event)
        }
    }

    fn backend(
        event: &'static str,
        route: &RouteContext,
        lease_id: Option<String>,
        workspace_id: String,
    ) -> Self {
        Self {
            lease_id,
            workspace_id: Some(workspace_id),
            backend: Some(backend_name(route.kind)),
            generation: Some(route.generation),
            ..Self::empty(event)
        }
    }

    fn backend_error(
        event: &'static str,
        route: &RouteContext,
        error_class: &'static str,
        workspace_id: String,
    ) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            backend: Some(backend_name(route.kind)),
            generation: Some(route.generation),
            error_class: Some(error_class),
            ..Self::empty(event)
        }
    }

    fn error(event: &'static str, error_class: &'static str) -> Self {
        Self {
            error_class: Some(error_class),
            ..Self::empty(event)
        }
    }

    fn refresh(success: bool, error_class: &'static str) -> Self {
        Self {
            kind: "oauth_refresh",
            disposition: Some(if success {
                CallDisposition::Completed
            } else {
                CallDisposition::Failed
            }),
            error_class: Some(error_class),
            ..Self::empty("oauth_refresh")
        }
    }

    fn owner(event: &'static str, owner_id: String) -> Self {
        Self {
            owner_id: Some(owner_id),
            ..Self::empty(event)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        owner_id: &str,
        call_id: &str,
        tool: &'static str,
        route: Option<&RouteContext>,
        disposition: CallDisposition,
        error_class: &'static str,
        duration_ms: u64,
        workspace_id: Option<String>,
    ) -> Self {
        Self {
            kind: "call_finished",
            owner_id: Some(owner_id.to_owned()),
            call_id: Some(call_id.to_owned()),
            workspace_id,
            backend: route.map(|value| backend_name(value.kind)),
            generation: route.map(|value| value.generation),
            tool: Some(tool),
            duration_ms: Some(duration_ms),
            disposition: Some(disposition),
            error_class: Some(error_class),
            ..Self::empty("call_finished")
        }
    }

    const fn empty(event: &'static str) -> Self {
        Self {
            kind: event,
            owner_id: None,
            call_id: None,
            lease_id: None,
            workspace_id: None,
            backend: None,
            generation: None,
            tool: None,
            duration_ms: None,
            disposition: None,
            error_class: None,
            state: None,
        }
    }
}

const fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Local => "local",
        BackendKind::Vps => "vps",
    }
}

const fn tool_index(request: &ToolRequest) -> usize {
    match request {
        ToolRequest::ExecCommand(_) => 0,
        ToolRequest::WriteStdin(_) => 1,
        ToolRequest::ApplyPatch(_) => 2,
        ToolRequest::TerminateSession(_) => 3,
        ToolRequest::SkillsList(_) => 4,
        ToolRequest::SkillsRead(_) => 5,
    }
}

fn safe_error_class(value: &'static str) -> &'static str {
    match value {
        "none"
        | "apply_patch_failed"
        | "authority_cleanup_failed"
        | "authority_revoked"
        | "backend_changed"
        | "backend_lost"
        | "cancelled"
        | "connection_saturated"
        | "containment_unavailable"
        | "invalid_client"
        | "invalid_cursor"
        | "invalid_grant"
        | "invalid_resource"
        | "invalid_scope"
        | "invalid_target"
        | "lease_expired"
        | "lease_lost"
        | "no_backend"
        | "not_dispatched"
        | "outcome_unknown"
        | "registration_rejected"
        | "relay_disconnected"
        | "remote_error"
        | "request_cancelled"
        | "skill_store_failed"
        | "state_unavailable"
        | "tls_handshake_failed"
        | "unsupported_grant_type" => value,
        _ => "other",
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn metric(output: &mut String, name: &str, value: u64) {
    writeln!(output, "{name} {value}").expect("write to String cannot fail");
}

fn metric_bool(output: &mut String, name: &str, value: bool) {
    metric(output, name, u64::from(value));
}

fn labelled_metric(output: &mut String, name: &str, label: &str, label_value: &str, value: u64) {
    writeln!(output, "{name}{{{label}=\"{label_value}\"}} {value}")
        .expect("write to String cannot fail");
}

fn backend_metrics(output: &mut String, backend: &str, eligible: bool, generation: u64) {
    labelled_metric(
        output,
        "tools_mcp_backend_eligible",
        "backend",
        backend,
        u64::from(eligible),
    );
    labelled_metric(
        output,
        "tools_mcp_backend_generation",
        "backend",
        backend,
        generation,
    );
    labelled_metric(
        output,
        "tools_mcp_backend_compatible",
        "backend",
        backend,
        u64::from(eligible),
    );
}

fn histogram_p95(buckets: &[AtomicU64; LATENCY_BUCKETS_MS.len()], count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let target = count.saturating_mul(95).div_ceil(100);
    LATENCY_BUCKETS_MS
        .iter()
        .zip(buckets)
        .find_map(|(upper, bucket)| (bucket.load(Ordering::Acquire) >= target).then_some(*upper))
        .unwrap_or(u64::MAX)
}

fn validate_host_metrics(input: &str) -> Result<String, MetricsError> {
    if input.len() > MAX_HOST_METRICS_BYTES {
        return Err(MetricsError::Oversized);
    }
    let mut output = String::new();
    let mut lines = 0_usize;
    for line in input.lines() {
        if line.is_empty() {
            continue;
        }
        lines += 1;
        if lines > MAX_HOST_METRIC_LINES {
            return Err(MetricsError::Oversized);
        }
        let Some((name, value)) = line.split_once(' ') else {
            return Err(MetricsError::InvalidLine);
        };
        if !HOST_METRIC_NAMES.contains(&name)
            || value.is_empty()
            || value.contains(char::is_whitespace)
            || value.parse::<f64>().is_err()
        {
            return Err(MetricsError::InvalidLine);
        }
        writeln!(output, "{name} {value}").expect("write to String cannot fail");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn host_collector_and_gateway_share_one_exact_metric_allowlist() {
        let collector = include_str!("../../../deploy/vps/scripts/collect-host-metrics");
        let emitted = collector
            .lines()
            .filter_map(|line| line.trim().strip_prefix("emit tools_mcp_"))
            .filter_map(|tail| tail.split_ascii_whitespace().next())
            .map(|suffix| format!("tools_mcp_{suffix}"))
            .collect::<HashSet<_>>();
        let allowed = HOST_METRIC_NAMES.into_iter().map(str::to_owned).collect();
        assert_eq!(emitted, allowed);
    }

    #[test]
    fn p95_uses_cumulative_fixed_buckets() {
        let telemetry = Observability::test_instance();
        for duration in 1..=100 {
            telemetry.record_http(200, Duration::from_millis(duration));
        }
        let rendered = telemetry.render_metrics(None).unwrap();
        assert!(rendered.contains("tools_mcp_gateway_http_p95_ms 100\n"));
    }
}
