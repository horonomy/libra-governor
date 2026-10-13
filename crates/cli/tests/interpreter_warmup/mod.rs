//! One-time cold-start warm-up for the fixture interpreter (HORO-1872).
//!
//! On macOS `/usr/bin/python3` is an xcrun stub. Its first launch after the
//! per-user temp directory is emptied (every fresh CI VM, every reboot)
//! rebuilds the `xcrun_db` cache under code-signing assessment, measured at
//! 6-12s. The product charges that to its fixed 4s per-request adapter budget,
//! so whichever test happens to exec the driver first observed a spurious
//! `timeout` / `exec_status_closed`. Paying the cost once, before any test can
//! exec the driver, keeps those tests about adapter behavior, not host warmth.

use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

const INTERPRETER: &str = "/usr/bin/python3";
// Generous: this only absorbs a one-off host cold start, never a product wait.
const WARMUP_BUDGET: Duration = Duration::from_secs(120);

/// Blocks every caller until one warm-up launch has completed.
pub fn warm_fixture_interpreter() {
    static WARMED: Once = Once::new();
    WARMED.call_once(|| {
        // Mirror the product's cleared adapter environment so the warm-up
        // populates the same cache the governed child process will consult.
        let mut child = Command::new(INTERPRETER)
            .args(["-c", "import copy, hashlib, json, os, sys, time"])
            .env_clear()
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("fixture interpreter must be installed");
        let deadline = Instant::now() + WARMUP_BUDGET;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "fixture interpreter warm-up failed");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("fixture interpreter cold start exceeded the warm-up budget");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}
