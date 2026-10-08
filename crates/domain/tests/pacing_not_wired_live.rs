//! Zero-live-wiring static guard for `libra_governor_domain::pacing`
//! (HORO-1765), cloned from
//! `crates/quota-source/tests/no_network_symbols.rs`'s pattern — a
//! test-run-time scan of `src/**/*.rs`, not a single `include_str!`
//! (which would only ever scan the one file it is compiled into).
//!
//! HORO-1727 has not yet resolved the live-admission architecture
//! question. Until it does, `pacing` must stay serialize-only data and
//! pure functions with no `apply`, no daemon/ledger consumer, and no
//! feature flag gating a live path — see `pacing`'s own module docs and
//! `docs/adr/0016-sustain-burst-pacing.md`. This test fails the build if
//! any of `daemon/src`, `gateway/src`, or `ledger/src` references
//! `pacing` at all, so that wiring a live consumer is a deliberate,
//! reviewed decision (deleting or updating this test) rather than a
//! silent drift.

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

fn scan_for_pacing_references(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut hits = Vec::new();
    for file in files {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        if contents.contains("pacing") {
            hits.push(file);
        }
    }
    hits
}

/// Repo root, derived from this crate's own manifest dir
/// (`crates/domain`) rather than hardcoding an absolute path, so the
/// test still finds the right directories when the workspace is
/// checked out anywhere.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/domain has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .to_path_buf()
}

#[test]
fn daemon_gateway_and_ledger_never_reference_pacing() {
    let root = repo_root();
    for crate_name in ["daemon", "gateway", "ledger"] {
        let src = root.join("crates").join(crate_name).join("src");
        if !src.exists() {
            // A renamed/removed crate should fail loudly elsewhere, not
            // silently pass this guard by having nothing to scan.
            panic!("expected {crate_name}'s src directory to exist at {src:?}");
        }
        let hits = scan_for_pacing_references(&src);
        assert!(
            hits.is_empty(),
            "HORO-1727 has not resolved the live-admission architecture question yet — \
             `{crate_name}` must not reference `pacing` until that ticket authorizes a live \
             consumer (see docs/adr/0016-sustain-burst-pacing.md). Found references in: {hits:?}"
        );
    }
}
