//! Transport-neutral, owner-scoped unified execution registry.

mod manager;
mod output;
mod pty;
mod state;

pub use manager::{PendingResult, ProcessManager, ProcessManagerConfig, ProcessStats};
pub use state::{OwnerId, ProcessError};

pub const MIN_YIELD_TIME_MS: u64 = 250;
pub const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 10_000;
pub const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub const MAX_YIELD_TIME_MS: u64 = 30_000;
pub const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;
