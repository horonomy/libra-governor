//! `libra-governor-daemon` — the local long-running Governor daemon:
//! Unix-socket server, bounded reconnaissance, and Completion Contract
//! drafting.
//!
//! See `ARCHITECTURE.md` for how this fits the overall system shape:
//! Claude Code hooks/statusline talk to this daemon over the protocol
//! defined in `libra-governor-protocol`; the daemon is the only
//! component that reads the repository or writes to the ledger.

pub mod config_file;
pub mod contract;
mod execution_owner;
pub mod extension_authority;
pub mod features;
pub mod gateway_authority;
pub mod host_binding;
pub mod host_runtime;
pub mod launch_gate;
pub mod legacy_executor;
pub mod log;
pub mod metered_pool;
pub mod outcome_authority_wiring;
pub mod paths;
pub mod pidfile;
pub mod recon;
pub mod server;
pub mod shared_budget;
pub mod shared_budget_file;
#[cfg(feature = "trusted-principal")]
pub mod trusted_principal;
pub mod usage;

pub use server::{
    bind_or_detect_running, default_admission_policy, handle_connection, legacy_bounded_policy,
    serve, DaemonConfig, DaemonError, DEFAULT_PROGRESSIVE_INTERVAL_SECS,
};

pub mod write_lock;
