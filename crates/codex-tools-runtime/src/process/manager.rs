use super::output::render_staged;
use super::pty::{self, RunningProcess};
use super::state::{OwnerId, ProcessError};
use crate::contracts::{ExecCommandInput, ExecCommandOutput, WriteStdinInput};
use crate::upstream_head_tail_buffer::HeadTailBuffer;
use mcp_agent_authority::sandbox::VerifiedSandbox;
use rand::Rng;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, OwnedMutexGuard};
use tokio::time::Instant;

use super::{
    DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS, MAX_UNIFIED_EXEC_PROCESSES, MAX_YIELD_TIME_MS,
    MIN_EMPTY_YIELD_TIME_MS, MIN_YIELD_TIME_MS, WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS,
};

#[derive(Clone, Debug)]
pub struct ProcessManagerConfig {
    pub capacity: usize,
    pub terminal_retention: Duration,
    pub max_empty_poll_yield: Duration,
}

impl Default for ProcessManagerConfig {
    fn default() -> Self {
        Self {
            capacity: MAX_UNIFIED_EXEC_PROCESSES,
            terminal_retention: Duration::from_mins(5),
            max_empty_poll_yield: Duration::from_millis(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS),
        }
    }
}

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Inner>,
}

struct Inner {
    sandbox: Arc<VerifiedSandbox>,
    config: ProcessManagerConfig,
    registry: Mutex<Registry>,
    next_id: AtomicI32,
    idle: Notify,
}

#[derive(Default)]
struct Registry {
    slots: HashMap<i32, Slot>,
    tombstones: HashMap<i32, Tombstone>,
    shutting_down: bool,
}

struct Slot {
    owner: OwnerId,
    phase: SlotPhase,
}

enum SlotPhase {
    Reserved,
    Unpublished(Arc<RunningProcess>),
    Published(Arc<RunningProcess>),
}

struct Tombstone {
    owner: OwnerId,
    process: Arc<RunningProcess>,
    expires_at: Instant,
}

#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellDialect {
    Posix,
    Fish,
}

#[cfg(not(windows))]
fn shell_dialect(shell: &str) -> Result<ShellDialect, ProcessError> {
    match Path::new(shell).file_name().and_then(OsStr::to_str) {
        Some("sh" | "bash" | "zsh") => Ok(ShellDialect::Posix),
        Some("fish") => Ok(ShellDialect::Fish),
        _ => Err(ProcessError::UnsupportedShell {
            shell: shell.to_owned(),
        }),
    }
}

#[cfg(not(windows))]
fn command_with_fixed_environment(
    dialect: ShellDialect,
    environment: &BTreeMap<String, OsString>,
    command: &str,
) -> Result<String, ProcessError> {
    let mut prologue = String::new();
    for (name, value) in environment {
        let value = value
            .to_str()
            .ok_or_else(|| ProcessError::UnsupportedShell {
                shell: "non-UTF-8 workload environment".to_owned(),
            })?;
        match dialect {
            ShellDialect::Posix => {
                let quoted = value.replace('\'', "'\\''");
                prologue.push_str("export ");
                prologue.push_str(name);
                prologue.push_str("='");
                prologue.push_str(&quoted);
                prologue.push_str("';");
            }
            ShellDialect::Fish => {
                let quoted = value.replace('\\', "\\\\").replace('\'', "\\'");
                prologue.push_str("set -gx ");
                prologue.push_str(name);
                prologue.push_str(" '");
                prologue.push_str(&quoted);
                prologue.push_str("';");
            }
        }
    }
    prologue.push(' ');
    prologue.push_str(command);
    Ok(prologue)
}

fn lock_registry(inner: &Inner) -> MutexGuard<'_, Registry> {
    inner
        .registry
        .lock()
        .expect("process registry mutex poisoned")
}

#[cfg(all(test, not(windows)))]
mod fixed_environment_tests {
    use super::{ShellDialect, command_with_fixed_environment, shell_dialect};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn environment() -> BTreeMap<String, OsString> {
        BTreeMap::from([
            ("CODEX_HOME".to_owned(), OsString::from("/tmp/codex home's")),
            ("TMPDIR".to_owned(), OsString::from("/tmp/fixed")),
        ])
    }

    #[test]
    fn supported_shells_receive_dialect_safe_post_startup_prologues() {
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            let dialect = shell_dialect(shell).unwrap();
            assert_eq!(dialect, ShellDialect::Posix);
            let command =
                command_with_fixed_environment(dialect, &environment(), "printf ok").unwrap();
            assert!(command.starts_with("export CODEX_HOME='/tmp/codex home'\\''s';"));
            assert!(command.ends_with("printf ok"));
        }

        let dialect = shell_dialect("/opt/homebrew/bin/fish").unwrap();
        assert_eq!(dialect, ShellDialect::Fish);
        let command = command_with_fixed_environment(dialect, &environment(), "printf ok").unwrap();
        assert!(command.starts_with("set -gx CODEX_HOME '/tmp/codex home\\'s';"));
        assert!(command.ends_with("printf ok"));
    }

    #[test]
    fn posix_prologue_reasserts_inherited_values_before_the_workload() {
        let command = command_with_fixed_environment(
            ShellDialect::Posix,
            &environment(),
            "printf '%s|%s' \"$CODEX_HOME\" \"$TMPDIR\"",
        )
        .unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .env("CODEX_HOME", "/hostile")
            .env("TMPDIR", "/hostile")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "/tmp/codex home's|/tmp/fixed"
        );
    }

    #[test]
    fn unsupported_shell_is_rejected_before_launch() {
        assert!(shell_dialect("/bin/tcsh").is_err());
        assert!(shell_dialect("not-a-shell").is_err());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessStats {
    pub occupied: usize,
    pub reserved: usize,
    pub live: usize,
    pub tombstones: usize,
}

pub struct PendingResult {
    manager: ProcessManager,
    owner: OwnerId,
    session_id: i32,
    process: Arc<RunningProcess>,
    output: ExecCommandOutput,
    staged: Option<HeadTailBuffer>,
    max_output_tokens: Option<usize>,
    kind: PendingKind,
    settled: bool,
}

enum PendingKind {
    Initial,
    Interaction(Option<OwnedMutexGuard<()>>),
}

impl std::fmt::Debug for PendingResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingResult")
            .field("session_id", &self.session_id)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

impl ProcessManager {
    #[must_use]
    pub fn new(sandbox: Arc<VerifiedSandbox>) -> Self {
        Self::with_config(sandbox, ProcessManagerConfig::default())
    }

    #[must_use]
    pub fn with_config(sandbox: Arc<VerifiedSandbox>, mut config: ProcessManagerConfig) -> Self {
        config.capacity = config.capacity.min(MAX_UNIFIED_EXEC_PROCESSES);
        config.max_empty_poll_yield = config
            .max_empty_poll_yield
            .max(Duration::from_millis(MIN_EMPTY_YIELD_TIME_MS));
        Self {
            inner: Arc::new(Inner {
                sandbox,
                config,
                registry: Mutex::new(Registry::default()),
                next_id: AtomicI32::new(1_000),
                idle: Notify::new(),
            }),
        }
    }

    /// Starts a sandboxed command and stages its first compatible result.
    ///
    /// # Errors
    ///
    /// Returns an admission, authority, sandbox-spawn, or shutdown error.
    pub async fn exec_command(
        &self,
        owner: &OwnerId,
        input: ExecCommandInput,
    ) -> Result<PendingResult, ProcessError> {
        let session_id = self.reserve(owner)?;
        let command = match self.build_command(&input) {
            Ok(command) => command,
            Err(error) => {
                self.release_reservation(session_id);
                return Err(error);
            }
        };
        let process = match pty::spawn(command, input.tty) {
            Ok(process) => process,
            Err(error) => {
                self.release_reservation(session_id);
                return Err(error);
            }
        };

        let rejected_by_shutdown = {
            let mut registry = lock_registry(&self.inner);
            if registry.shutting_down {
                registry.slots.remove(&session_id);
                true
            } else if let Some(slot) = registry.slots.get_mut(&session_id) {
                slot.phase = SlotPhase::Unpublished(Arc::clone(&process));
                false
            } else {
                true
            }
        };
        if rejected_by_shutdown {
            process.terminate().await;
            self.inner.idle.notify_waiters();
            return Err(ProcessError::ShuttingDown);
        }
        self.spawn_terminal_monitor(session_id, Arc::clone(&process));

        let started = Instant::now();
        let yield_time = clamp_initial_yield(input.yield_time_ms);
        let staged = process
            .output
            .collect_until(started + Duration::from_millis(yield_time))
            .await;
        let wall_time = started.elapsed();
        let output = output_for(
            &staged,
            input.max_output_tokens,
            wall_time,
            (!process.output.is_closed()).then_some(session_id),
            process.output.exit_code(),
        );

        Ok(PendingResult {
            manager: self.clone(),
            owner: owner.clone(),
            session_id,
            process,
            output,
            staged: Some(staged),
            max_output_tokens: input.max_output_tokens,
            kind: PendingKind::Initial,
            settled: false,
        })
    }

    /// Writes once to, or polls, an owner-scoped published session.
    ///
    /// # Errors
    ///
    /// Returns an unknown-session, closed-stdin, interaction, or shutdown error.
    pub async fn write_stdin(
        &self,
        owner: &OwnerId,
        input: WriteStdinInput,
    ) -> Result<PendingResult, ProcessError> {
        self.reap_expired();
        let process = self.lookup(owner, input.session_id)?;
        let guard = Arc::clone(&process.interaction).lock_owned().await;
        self.recheck(owner, input.session_id, &process)?;

        if !input.chars.is_empty() {
            if input.chars == "\u{3}" && !process.tty {
                process.interrupt().await?;
            } else if !process.tty {
                return Err(ProcessError::StdinClosed {
                    session_id: input.session_id,
                });
            } else {
                // This write is intentionally performed exactly once. A caller
                // must never retry it automatically after an ambiguous handoff.
                process.write(input.chars.as_bytes().to_vec()).await?;
            }
        }

        let yield_time = clamp_write_yield(
            input.yield_time_ms,
            input.chars.is_empty(),
            self.inner.config.max_empty_poll_yield,
        );
        let started = Instant::now();
        let staged = process.output.collect_until(started + yield_time).await;
        let wall_time = started.elapsed();
        let output = output_for(
            &staged,
            input.max_output_tokens,
            wall_time,
            (!process.output.is_closed()).then_some(input.session_id),
            process.output.exit_code(),
        );

        Ok(PendingResult {
            manager: self.clone(),
            owner: owner.clone(),
            session_id: input.session_id,
            process,
            output,
            staged: Some(staged),
            max_output_tokens: input.max_output_tokens,
            kind: PendingKind::Interaction(Some(guard)),
            settled: false,
        })
    }

    /// Interrupts an owner-scoped published session.
    ///
    /// # Errors
    ///
    /// Returns an unknown-session, interaction, or shutdown error.
    pub async fn interrupt(&self, owner: &OwnerId, session_id: i32) -> Result<(), ProcessError> {
        let process = self.lookup(owner, session_id)?;
        let _guard = Arc::clone(&process.interaction).lock_owned().await;
        self.recheck(owner, session_id, &process)?;
        process.interrupt().await
    }

    #[must_use]
    pub fn stats(&self) -> ProcessStats {
        let registry = lock_registry(&self.inner);
        ProcessStats {
            occupied: registry.slots.len(),
            reserved: registry
                .slots
                .values()
                .filter(|slot| !matches!(slot.phase, SlotPhase::Published(_)))
                .count(),
            live: registry
                .slots
                .values()
                .filter(|slot| matches!(slot.phase, SlotPhase::Published(_)))
                .count(),
            tombstones: registry.tombstones.len(),
        }
    }

    pub fn reap_expired(&self) {
        let now = Instant::now();
        let mut registry = lock_registry(&self.inner);
        let expired = registry
            .tombstones
            .iter()
            .filter_map(|(id, tombstone)| {
                (tombstone.expires_at <= now
                    && Arc::clone(&tombstone.process.interaction)
                        .try_lock_owned()
                        .is_ok())
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in expired {
            registry.tombstones.remove(&id);
        }
    }

    pub async fn shutdown(&self) {
        let processes = {
            let mut registry = lock_registry(&self.inner);
            registry.shutting_down = true;
            let session_ids = registry
                .slots
                .iter()
                .filter_map(|(session_id, slot)| {
                    (!matches!(slot.phase, SlotPhase::Reserved)).then_some(*session_id)
                })
                .collect::<Vec<_>>();
            let mut processes = session_ids
                .into_iter()
                .filter_map(|session_id| registry.slots.remove(&session_id))
                .filter_map(|slot| match slot.phase {
                    SlotPhase::Reserved => None,
                    SlotPhase::Unpublished(process) | SlotPhase::Published(process) => {
                        Some(process)
                    }
                })
                .collect::<Vec<_>>();
            processes.extend(
                registry
                    .tombstones
                    .drain()
                    .map(|(_, tombstone)| tombstone.process),
            );
            processes
        };
        self.inner.idle.notify_waiters();
        for process in processes {
            process.terminate().await;
        }
        self.wait_for_idle().await;
    }

    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock_registry(&self.inner).slots.is_empty() {
                return;
            }
            notified.await;
        }
    }

    fn reserve(&self, owner: &OwnerId) -> Result<i32, ProcessError> {
        self.reap_expired();
        let mut registry = lock_registry(&self.inner);
        if registry.shutting_down {
            return Err(ProcessError::ShuttingDown);
        }
        if registry.slots.len() >= self.inner.config.capacity {
            return Err(ProcessError::Capacity {
                limit: self.inner.config.capacity,
            });
        }
        let session_id = next_session_id(&self.inner.next_id, &registry);
        registry.slots.insert(
            session_id,
            Slot {
                owner: owner.clone(),
                phase: SlotPhase::Reserved,
            },
        );
        Ok(session_id)
    }

    fn release_reservation(&self, session_id: i32) {
        lock_registry(&self.inner).slots.remove(&session_id);
        self.inner.idle.notify_waiters();
    }

    fn build_command(
        &self,
        input: &ExecCommandInput,
    ) -> Result<std::process::Command, ProcessError> {
        let cwd = input
            .workdir
            .as_deref()
            .filter(|path| !path.is_empty())
            .map_or_else(|| PathBuf::from("."), PathBuf::from);

        let capabilities = self.inner.sandbox.capabilities().cloned();

        #[cfg(windows)]
        let (shell, args) = {
            let shell = input
                .shell
                .clone()
                .unwrap_or_else(|| "powershell.exe".to_owned());
            let args = vec![
                "-NoLogo".to_owned(),
                "-Command".to_owned(),
                input.cmd.clone(),
            ];
            (shell, args)
        };
        #[cfg(not(windows))]
        let (shell, args) = {
            let shell = input
                .shell
                .clone()
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/sh".to_owned());
            let dialect = shell_dialect(&shell)?;
            let command = capabilities.as_ref().map_or_else(
                || Ok(input.cmd.clone()),
                |snapshot| {
                    command_with_fixed_environment(dialect, snapshot.environment(), &input.cmd)
                },
            )?;
            let mode = if input.login.unwrap_or(true) {
                "-lc"
            } else {
                "-c"
            };
            (shell, vec![mode.to_owned(), command])
        };
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let mut command = self
            .inner
            .sandbox
            .command(&shell, &args, &cwd)
            .and_then(mcp_agent_authority::sandbox::SandboxCommand::into_std_command)
            .map_err(ProcessError::spawn)?;
        if let Some(capabilities) = capabilities {
            command.envs(capabilities.environment());
        }
        Ok(command)
    }

    fn spawn_terminal_monitor(&self, session_id: i32, process: Arc<RunningProcess>) {
        let manager = self.clone();
        tokio::spawn(async move {
            process.output.wait_closed().await;
            manager.finalize_published(session_id, &process);
        });
    }

    fn finalize_published(&self, session_id: i32, process: &Arc<RunningProcess>) {
        let mut registry = lock_registry(&self.inner);
        let should_finalize = registry.slots.get(&session_id).is_some_and(|slot| {
            matches!(&slot.phase, SlotPhase::Published(current) if Arc::ptr_eq(current, process))
        });
        if !should_finalize {
            return;
        }
        let Some(slot) = registry.slots.remove(&session_id) else {
            return;
        };
        let expires_at = Instant::now() + self.inner.config.terminal_retention;
        registry.tombstones.insert(
            session_id,
            Tombstone {
                owner: slot.owner,
                process: Arc::clone(process),
                expires_at,
            },
        );
        drop(registry);
        self.inner.idle.notify_waiters();
        self.spawn_tombstone_expiry(session_id, Arc::clone(process), expires_at);
    }

    fn spawn_tombstone_expiry(
        &self,
        session_id: i32,
        process: Arc<RunningProcess>,
        expires_at: Instant,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(expires_at).await;
            let _interaction = Arc::clone(&process.interaction).lock_owned().await;
            let mut registry = lock_registry(&manager.inner);
            let matches = registry
                .tombstones
                .get(&session_id)
                .is_some_and(|tombstone| {
                    Arc::ptr_eq(&tombstone.process, &process)
                        && tombstone.expires_at <= Instant::now()
                });
            if matches {
                registry.tombstones.remove(&session_id);
            }
        });
    }

    fn lookup(
        &self,
        owner: &OwnerId,
        session_id: i32,
    ) -> Result<Arc<RunningProcess>, ProcessError> {
        let registry = lock_registry(&self.inner);
        if registry.shutting_down {
            return Err(ProcessError::ShuttingDown);
        }
        if let Some(slot) = registry.slots.get(&session_id)
            && &slot.owner == owner
            && let SlotPhase::Published(process) = &slot.phase
        {
            return Ok(Arc::clone(process));
        }
        if let Some(tombstone) = registry.tombstones.get(&session_id)
            && &tombstone.owner == owner
        {
            return Ok(Arc::clone(&tombstone.process));
        }
        Err(ProcessError::UnknownSession { session_id })
    }

    fn recheck(
        &self,
        owner: &OwnerId,
        session_id: i32,
        expected: &Arc<RunningProcess>,
    ) -> Result<(), ProcessError> {
        let registry = lock_registry(&self.inner);
        let matches_slot = registry.slots.get(&session_id).is_some_and(|slot| {
            &slot.owner == owner
                && matches!(&slot.phase, SlotPhase::Published(process) if Arc::ptr_eq(process, expected))
        });
        let matches_tombstone = registry
            .tombstones
            .get(&session_id)
            .is_some_and(|tombstone| {
                &tombstone.owner == owner && Arc::ptr_eq(&tombstone.process, expected)
            });
        if matches_slot || matches_tombstone {
            Ok(())
        } else {
            Err(ProcessError::UnknownSession { session_id })
        }
    }

    async fn cancel_initial(&self, session_id: i32, process: &Arc<RunningProcess>) {
        let removed = {
            let mut registry = lock_registry(&self.inner);
            let matches = registry.slots.get(&session_id).is_some_and(|slot| {
                matches!(&slot.phase, SlotPhase::Unpublished(current) if Arc::ptr_eq(current, process))
            });
            matches
                .then(|| registry.slots.remove(&session_id))
                .flatten()
        };
        if removed.is_some() {
            process.terminate().await;
            self.inner.idle.notify_waiters();
        }
    }

    fn cancel_initial_detached(&self, session_id: i32, process: &Arc<RunningProcess>) {
        let removed = {
            let mut registry = lock_registry(&self.inner);
            let matches = registry.slots.get(&session_id).is_some_and(|slot| {
                matches!(&slot.phase, SlotPhase::Unpublished(current) if Arc::ptr_eq(current, process))
            });
            matches
                .then(|| registry.slots.remove(&session_id))
                .flatten()
        };
        if removed.is_some() {
            process.terminate_detached();
            self.inner.idle.notify_waiters();
        }
    }
}

fn next_session_id(next_id: &AtomicI32, registry: &Registry) -> i32 {
    loop {
        let current = next_id.load(Ordering::Relaxed);
        let candidate = if (1_000..i32::MAX).contains(&current) {
            current
        } else {
            1_000
        };
        let following = if candidate == i32::MAX - 1 {
            1_000
        } else {
            candidate + 1
        };
        if next_id
            .compare_exchange(current, following, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        if !registry.slots.contains_key(&candidate) && !registry.tombstones.contains_key(&candidate)
        {
            return candidate;
        }
    }
}

impl PendingResult {
    #[must_use]
    pub fn output(&self) -> &ExecCommandOutput {
        &self.output
    }

    /// Commits staged output after the transport accepts responsibility for it.
    ///
    /// # Errors
    ///
    /// Returns when shutdown or another committed interaction invalidated the
    /// pending result before handoff.
    pub async fn handoff(mut self) -> Result<ExecCommandOutput, ProcessError> {
        let result = self.commit_handoff();
        if matches!(result, Err(ProcessError::ShuttingDown))
            && matches!(self.kind, PendingKind::Initial)
        {
            self.process.terminate().await;
        }
        result?;
        self.staged.take();
        self.settled = true;
        Ok(self.output.clone())
    }

    fn commit_handoff(&mut self) -> Result<(), ProcessError> {
        let manager = self.manager.clone();
        let mut registry = lock_registry(&manager.inner);
        if registry.shutting_down {
            return Err(ProcessError::ShuttingDown);
        }
        self.refresh_terminal_output();

        match &mut self.kind {
            PendingKind::Initial => {
                let Some(slot) = registry.slots.get_mut(&self.session_id) else {
                    return Err(ProcessError::UnknownSession {
                        session_id: self.session_id,
                    });
                };
                if slot.owner != self.owner
                    || !matches!(&slot.phase, SlotPhase::Unpublished(process) if Arc::ptr_eq(process, &self.process))
                {
                    return Err(ProcessError::UnknownSession {
                        session_id: self.session_id,
                    });
                }
                if self.process.output.is_closed() {
                    registry.slots.remove(&self.session_id);
                    self.manager.inner.idle.notify_waiters();
                } else {
                    slot.phase = SlotPhase::Published(Arc::clone(&self.process));
                }
            }
            PendingKind::Interaction(guard) => {
                if self.process.output.is_closed() {
                    let remove_slot = registry.slots.get(&self.session_id).is_some_and(|slot| {
                        slot.owner == self.owner
                            && matches!(&slot.phase, SlotPhase::Published(process) if Arc::ptr_eq(process, &self.process))
                    });
                    if remove_slot {
                        registry.slots.remove(&self.session_id);
                        self.manager.inner.idle.notify_waiters();
                    }
                    let remove_tombstone =
                        registry
                            .tombstones
                            .get(&self.session_id)
                            .is_some_and(|tombstone| {
                                tombstone.owner == self.owner
                                    && Arc::ptr_eq(&tombstone.process, &self.process)
                            });
                    if remove_tombstone {
                        registry.tombstones.remove(&self.session_id);
                    }
                } else {
                    let valid = registry.slots.get(&self.session_id).is_some_and(|slot| {
                        slot.owner == self.owner
                            && matches!(&slot.phase, SlotPhase::Published(process) if Arc::ptr_eq(process, &self.process))
                    });
                    if !valid {
                        return Err(ProcessError::UnknownSession {
                            session_id: self.session_id,
                        });
                    }
                }
                guard.take();
            }
        }
        Ok(())
    }

    pub async fn cancel(mut self) {
        if let Some(staged) = self.staged.take() {
            self.process.output.restore(staged);
        }
        if matches!(self.kind, PendingKind::Initial) {
            self.manager
                .cancel_initial(self.session_id, &self.process)
                .await;
        }
        if let PendingKind::Interaction(guard) = &mut self.kind {
            guard.take();
        }
        self.settled = true;
    }

    fn refresh_terminal_output(&mut self) {
        if self.process.output.is_closed()
            && let Some(staged) = self.staged.as_mut()
        {
            staged.push_buffer(self.process.output.drain_now());
            let (output, tokens) = render_staged(staged, self.max_output_tokens);
            self.output.output = output;
            self.output.original_token_count = Some(tokens);
            self.output.session_id = None;
            self.output.exit_code = self.process.output.exit_code();
        }
    }
}

impl Drop for PendingResult {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if let Some(staged) = self.staged.take() {
            self.process.output.restore(staged);
        }
        if matches!(self.kind, PendingKind::Initial) {
            self.manager
                .cancel_initial_detached(self.session_id, &self.process);
        }
    }
}

fn clamp_initial_yield(yield_time_ms: u64) -> u64 {
    let yield_time_ms = if cfg!(windows) {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

fn clamp_write_yield(requested_ms: u64, empty: bool, empty_max: Duration) -> Duration {
    let requested_ms = requested_ms.max(MIN_YIELD_TIME_MS);
    if empty {
        Duration::from_millis(requested_ms)
            .clamp(Duration::from_millis(MIN_EMPTY_YIELD_TIME_MS), empty_max)
    } else {
        Duration::from_millis(requested_ms.min(MAX_YIELD_TIME_MS))
    }
}

fn output_for(
    staged: &HeadTailBuffer,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
    session_id: Option<i32>,
    exit_code: Option<i32>,
) -> ExecCommandOutput {
    let (output, original_token_count) = render_staged(staged, max_output_tokens);
    ExecCommandOutput {
        chunk_id: Some(generate_chunk_id()),
        wall_time_seconds: wall_time.as_secs_f64(),
        exit_code,
        session_id,
        original_token_count: Some(original_token_count),
        output,
    }
}

fn generate_chunk_id() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = rand::rng();
    let mut chunk_id = String::with_capacity(6);
    for _ in 0..6 {
        chunk_id.push(char::from(HEX[random.random_range(0..16)]));
    }
    chunk_id
}

#[cfg(test)]
mod tests {
    use super::{Registry, next_session_id};
    use std::sync::atomic::{AtomicI32, Ordering};

    #[test]
    fn session_id_allocation_wraps_before_i32_max() {
        let next_id = AtomicI32::new(i32::MAX - 1);
        let registry = Registry::default();

        assert_eq!(next_session_id(&next_id, &registry), i32::MAX - 1);
        assert_eq!(next_id.load(Ordering::Relaxed), 1_000);
        assert_eq!(next_session_id(&next_id, &registry), 1_000);
        assert_eq!(next_id.load(Ordering::Relaxed), 1_001);
    }
}
