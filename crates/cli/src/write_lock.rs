//! Compatibility surface for the canonical library lock.

pub use libra_governor_daemon::write_lock::{acquire, LockFailure};

/// Hidden `__lock_test_hold` subcommand — see `main.rs`'s dispatch arm
/// for why this exists and what it prints. Only ever invoked by
/// `tests/write_lock_cross_process.rs` as a genuinely separate OS
/// process; never reachable from documented CLI usage.
pub mod test_hold_cmd {
    use std::io::Write as _;

    pub fn run(config_path: &str, hold_ms: &str) {
        let path = std::path::PathBuf::from(config_path);
        let hold_ms: u64 = hold_ms.parse().unwrap_or(0);
        match super::acquire(&path) {
            Ok(guard) => {
                println!("LOCKED");
                std::io::stdout().flush().ok();
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                drop(guard);
            }
            Err(e) => {
                println!("LOCK_FAILED: {e}");
                std::io::stdout().flush().ok();
                std::process::exit(1);
            }
        }
    }
}
