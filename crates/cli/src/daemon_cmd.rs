//! `libra-governor daemon run` — the foreground daemon process entry
//! point. Spawned detached by [`crate::client::ensure_daemon_connection`]
//! when the `hook` subcommand finds no daemon listening.

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig, DaemonError};
use libra_governor_domain::ReplanHysteresisConfig;

/// Default TTL (HORO-1141) for how long a reservation may stay `Active`
/// before startup/opportunistic reconciliation reclaims it — generous
/// enough to cover a normal task's exploration-through-finalization span
/// without being effectively unbounded.
const DEFAULT_RESERVATION_TTL_SECS: u64 = 900;

/// Runs the daemon in the foreground: resolves state paths, binds the
/// socket (recovering a stale socket file, or exiting cleanly if another
/// daemon already owns it — see
/// `libra-governor-daemon::server::bind_or_detect_running`), and serves
/// forever.
pub fn run() {
    let state_dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor daemon: could not prepare state dir: {e}");
            std::process::exit(1);
        }
    };

    let config = DaemonConfig {
        socket_path: state_dir.join("daemon.sock"),
        ledger_path: state_dir.join("ledger.sqlite3"),
        log_path: state_dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: DEFAULT_RESERVATION_TTL_SECS,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
    };

    let listener = match libra_governor_daemon::bind_or_detect_running(&config.socket_path) {
        Ok(listener) => listener,
        Err(DaemonError::AlreadyRunning(path)) => {
            // Not an error from the caller's perspective: whichever
            // daemon spawned first wins, and losing this race is the
            // expected outcome of the spawn-if-absent pattern.
            libra_governor_daemon::log::append_line(
                &config.log_path,
                &format!("daemon already running at {}; exiting", path.display()),
            );
            return;
        }
        Err(e) => {
            eprintln!("libra-governor daemon: could not bind socket: {e}");
            std::process::exit(1);
        }
    };

    libra_governor_daemon::log::append_line(&config.log_path, "daemon started");
    if let Err(e) = libra_governor_daemon::serve(listener, &config) {
        libra_governor_daemon::log::append_line(
            &config.log_path,
            &format!("daemon exited with error: {e}"),
        );
        std::process::exit(1);
    }
}
