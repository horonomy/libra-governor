//! Process isolation for integration tests that exercise daemon auto-start.

use std::path::Path;
use std::process::{Command, Stdio};

/// Owns the state directory for one auto-started daemon and stops that exact
/// daemon before deleting its files, including while a test is unwinding.
pub struct DaemonState {
    directory: tempfile::TempDir,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("create isolated daemon state"),
        }
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn stop(&self) {
        // `daemon stop` verifies the pid record, executable identity, and this
        // state's live socket before signalling. The former `pkill -f
        // <state-dir>` cleanup never matched: the state directory is inherited
        // through the environment and is absent from the daemon's argv.
        let _ = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
            .args(["daemon", "stop"])
            .env("LIBRA_GOVERNOR_STATE_DIR", self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for DaemonState {
    fn drop(&mut self) {
        self.stop();
    }
}
