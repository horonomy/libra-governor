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
pub mod features;
pub mod gateway_authority;
pub mod log;
pub mod paths;
pub mod recon;
pub mod server;

pub use server::{
    bind_or_detect_running, default_admission_policy, handle_connection, serve, DaemonConfig,
    DaemonError,
};
