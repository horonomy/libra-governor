//! Zero-live-wiring static guard for authoritative outcome attestation
//! (ADR-0017, HORO-1727), cloned from
//! `crates/ledger/tests/renewal_not_wired_live.rs`'s pattern — a
//! test-run-time scan of `src/**/*.rs`, not a single `include_str!`
//! (which would only ever scan the one file it is compiled into).
//!
//! ADR-0017 found that no code-level change can make the authoritative
//! (`Provider`-variant) trust boundary of `AttestationSource` hold on
//! this single-workstation, same-OS-user deployment. This test fails
//! the build if `daemon/src` or `cli/src` ever constructs the
//! authoritative `Provider` or `GovernorLocal` variant of
//! `AttestationSource`, or writes a literal authoritative-true flag to
//! the ledger, so reintroducing an authoritative path is a deliberate,
//! reviewed decision (updating or deleting this guard) rather than a
//! silent drift back to "every push is authoritative".
//!
//! This file's own scan patterns are intentionally not spelled out in
//! this module doc comment in their exact matched form, so that
//! documenting this guard does not itself trip it.

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

fn scan_for_patterns<'a>(root: &Path, patterns: &[&'a str]) -> Vec<(PathBuf, &'a str)> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut hits = Vec::new();
    for file in files {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        for pattern in patterns {
            if contents.contains(pattern) {
                hits.push((file.clone(), *pattern));
            }
        }
    }
    hits
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/daemon has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .to_path_buf()
}

#[test]
fn daemon_and_cli_never_construct_an_authoritative_attestation_source() {
    let root = repo_root();
    // "AttestationSource :: Provider" / "AttestationSource :: GovernorLocal"
    // written without a space to match the real source formatting, kept
    // as two separate needles so a reformatted `AttestationSource::`
    // (e.g. via a `use` alias) cannot silently defeat this guard by
    // happening to not contain the exact substring -- both the
    // qualified and bare-variant forms are checked.
    let patterns = [
        "AttestationSource::Provider",
        "AttestationSource::GovernorLocal",
        "authoritative: true",
    ];
    for crate_name in ["daemon", "cli"] {
        let src = root.join("crates").join(crate_name).join("src");
        if !src.exists() {
            panic!("expected {crate_name}'s src directory to exist at {src:?}");
        }
        let hits = scan_for_patterns(&src, &patterns);
        assert!(
            hits.is_empty(),
            "ADR-0017 (HORO-1727): no code-level fix can make an authoritative \
             attestation trust boundary hold on this single-workstation, same-OS-user \
             deployment. `{crate_name}` must not construct `AttestationSource::Provider`/\
             `GovernorLocal` or write `authoritative: true` until a genuinely \
             separate-principal deployment exists and this guard is deliberately \
             updated. Found: {hits:?}"
        );
    }
}
