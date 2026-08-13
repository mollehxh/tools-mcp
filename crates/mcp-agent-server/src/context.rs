use codex_tools_runtime::process::{OwnerId, ProcessManager};
use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{SkillCatalog, SkillInstaller};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Long-lived, owner-scoped application capabilities shared by fresh MCP handlers.
#[derive(Clone)]
pub struct ApplicationContext {
    pub(crate) authority: WorkspaceAuthority,
    pub(crate) processes: Arc<ProcessManager>,
    pub(crate) catalog: Arc<SkillCatalog>,
    pub(crate) installer: Arc<SkillInstaller>,
    pub(crate) owner: OwnerId,
    install_operations: Arc<InstallOperations>,
}

impl ApplicationContext {
    #[must_use]
    pub fn new(
        authority: WorkspaceAuthority,
        processes: Arc<ProcessManager>,
        catalog: Arc<SkillCatalog>,
        installer: Arc<SkillInstaller>,
        owner: OwnerId,
    ) -> Self {
        Self {
            authority,
            processes,
            catalog,
            installer,
            owner,
            install_operations: Arc::new(InstallOperations::default()),
        }
    }

    pub(crate) fn begin_install_operation(
        &self,
        request_cancellation: CancellationToken,
    ) -> Option<(InstallRequest, InstallWorker)> {
        self.install_operations.begin(request_cancellation)
    }

    /// Closes install admission and cooperatively cancels active installers.
    pub fn cancel_install_operations(&self) {
        self.install_operations.close();
    }

    /// Waits until all blocking install workers have observed cancellation or
    /// completed their atomic publication step.
    pub async fn wait_for_install_operations(&self) {
        self.install_operations.wait_idle().await;
    }

    #[must_use]
    pub fn install_operations_closed(&self) -> bool {
        self.install_operations.closed.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default)]
struct InstallOperations {
    closed: AtomicBool,
    active: AtomicUsize,
    next_id: AtomicUsize,
    cancellations: Mutex<HashMap<usize, Weak<CancellationToken>>>,
    idle: Notify,
}

impl InstallOperations {
    fn begin(
        self: &Arc<Self>,
        request_cancellation: CancellationToken,
    ) -> Option<(InstallRequest, InstallWorker)> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = Arc::new(request_cancellation);
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::downgrade(&cancellation));
        if self.closed.load(Ordering::Acquire) {
            cancellation.cancel();
            self.finish_worker(id);
            return None;
        }
        Some((
            InstallRequest {
                cancellation: Arc::clone(&cancellation),
            },
            InstallWorker {
                operations: Arc::clone(self),
                cancellation,
                id,
            },
        ))
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel_active();
    }

    fn cancel_active(&self) {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, cancellation| {
                if let Some(cancellation) = cancellation.upgrade() {
                    cancellation.cancel();
                    true
                } else {
                    false
                }
            });
    }

    async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }

    fn finish_worker(&self, id: usize) {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.idle.notify_waiters();
        }
    }
}

pub(crate) struct InstallRequest {
    cancellation: Arc<CancellationToken>,
}

impl Drop for InstallRequest {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct InstallWorker {
    operations: Arc<InstallOperations>,
    cancellation: Arc<CancellationToken>,
    id: usize,
}

impl InstallWorker {
    #[must_use]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl Drop for InstallWorker {
    fn drop(&mut self) {
        self.operations.finish_worker(self.id);
    }
}
