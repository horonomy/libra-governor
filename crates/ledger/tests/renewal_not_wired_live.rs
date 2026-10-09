//! Zero-live-wiring static guard for `LedgerStore::grant_renewal`
//! (HORO-1727), cloned from `crates/domain/tests/pacing_not_wired_live.rs`'s
//! pattern — a test-run-time scan of `src/**/*.rs`, not a single
//! `include_str!` (which would only ever scan the one file it is
//! compiled into).
//!
//! This ticket ships the bounded renewal mechanism and the read sites
//! that must respect its effect, but deliberately no CLI command,
//! protocol request, or webhook path that could create a renewal grant
//! in production — see `libra_governor_domain::renewal`'s and
//! `crate::renewal`'s module docs. This test fails the build if any of
//! `daemon/src`, `cli/src`, or `gateway/src` ever calls `grant_renewal`,
//! so that wiring a live authorization path is a deliberate, reviewed
//! decision (deleting or updating this test) rather than a silent drift.

use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rs_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn scan_for_grant_renewal_references(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut hits = Vec::new();
    for file in files {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        if contents.contains("grant_renewal") {
            hits.push(file);
        }
    }
    hits
}

/// Repo root, derived from this crate's own manifest dir
/// (`crates/ledger`) rather than hardcoding an absolute path, so the
/// test still finds the right directories when the workspace is checked
/// out anywhere.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ledger has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .to_path_buf()
}

#[test]
fn daemon_cli_and_gateway_never_reference_grant_renewal() {
    let root = repo_root();
    for crate_name in ["daemon", "cli", "gateway"] {
        let src = root.join("crates").join(crate_name).join("src");
        if !src.exists() {
            // A renamed/removed crate should fail loudly elsewhere, not
            // silently pass this guard by having nothing to scan.
            panic!("expected {crate_name}'s src directory to exist at {src:?}");
        }
        let hits = scan_for_grant_renewal_references(&src);
        assert!(
            hits.is_empty(),
            "HORO-1727 ships no production authorization path for task-budget renewals — \
             `{crate_name}` must not reference `grant_renewal` until a future ticket \
             deliberately wires one (CLI command, protocol request, or approval flow) and \
             updates or deletes this guard. Found references in: {hits:?}"
        );
    }
}
