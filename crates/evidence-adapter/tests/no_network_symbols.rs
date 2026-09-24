//! Zero-network static guard for `libra-governor-evidence-adapter`
//! (HORO-1376), held to the *same* standard as
//! `crates/cli/src/evidence_report_cmd.rs`'s
//! `evidence_report_module_source_contains_no_network_symbols` — but
//! deliberately **stronger** in one respect: that guard's `include_str!`
//! only ever scans the one file it is compiled into (ADR-0012 §11.4's
//! own warning about that exact hole). This test instead walks
//! `src/**/*.rs` at **test-run time** via `std::fs::read_dir`, so a new
//! file added later to this crate cannot silently escape the scan.
//!
//! This is an integration test (`tests/`, not `src/`), so its own
//! forbidden-symbol literals are structurally invisible to a scan scoped
//! to `src/` — no self-matching trick needed here, unlike the CLI-side
//! copy of this guard.

use std::path::{Path, PathBuf};

/// The exact same forbidden-symbol list as
/// `evidence_report_module_source_contains_no_network_symbols` — this
/// crate must never be held to a looser standard than the module it
/// exists alongside.
const FORBIDDEN_SYMBOLS: &[&str] = &[
    "TcpStream",
    "UdpSocket",
    "reqwest",
    "ureq",
    "http::",
    "hyper",
    "curl",
];

/// Recursively collects every `.rs` file under `dir`.
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

/// Returns every `(file, symbol)` pair found across `root`'s `.rs`
/// files. Extracted as a plain function so the negative-control test
/// below can exercise it directly against a planted violation, proving
/// this scanner is a real check rather than a vacuous pass.
fn scan_for_forbidden_symbols(root: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut hits = Vec::new();
    for file in files {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        for symbol in FORBIDDEN_SYMBOLS {
            if contents.contains(symbol) {
                hits.push((file.clone(), *symbol));
            }
        }
    }
    hits
}

fn crate_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn evidence_adapter_source_contains_no_network_symbols() {
    let hits = scan_for_forbidden_symbols(&crate_src_dir());
    assert!(
        hits.is_empty(),
        "found network-capable symbol(s) in libra-governor-evidence-adapter's source: {hits:?} \
         — this crate must never make a network call (ADR-0012 §11.4)"
    );
}

/// Sanity: the walk must actually find real files, or the test above
/// would pass vacuously against an empty/misconfigured `src/` path.
#[test]
fn the_walk_actually_finds_source_files() {
    let mut files = Vec::new();
    collect_rs_files(&crate_src_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "the scanner found zero .rs files under src/ — it is not exercising anything"
    );
    assert!(files.iter().any(|f| f.ends_with("adapter.rs")));
}

/// Negative control: the scanner must actually flag a planted violation,
/// proving `scan_for_forbidden_symbols` is a real check and not one that
/// is green only because the code happens to be clean today.
#[test]
fn scanner_flags_a_planted_network_symbol() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("planted.rs"),
        "use reqwest::Client;\nfn f() {}\n",
    )
    .unwrap();
    let hits = scan_for_forbidden_symbols(dir.path());
    assert!(
        hits.iter().any(|(_, symbol)| *symbol == "reqwest"),
        "the scanner failed to flag a deliberately planted `reqwest` symbol: {hits:?}"
    );
}

/// A clean, single-symbol-free directory must produce zero hits — proves
/// the scanner does not over-fire on ordinary source text.
#[test]
fn scanner_does_not_flag_clean_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("clean.rs"), "fn f() -> u32 { 42 }\n").unwrap();
    assert!(scan_for_forbidden_symbols(dir.path()).is_empty());
}
