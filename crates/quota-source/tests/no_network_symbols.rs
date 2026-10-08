//! Zero-network static guard for `libra-governor-quota-source` (HORO-1764),
//! cloned from `crates/evidence-adapter/tests/no_network_symbols.rs` —
//! this crate must be held to the same standard: no live network call
//! anywhere in its source, enforced by a test-run-time scan of
//! `src/**/*.rs` rather than a single `include_str!` (which would only
//! ever scan the one file it is compiled into).

use std::path::{Path, PathBuf};

const FORBIDDEN_SYMBOLS: &[&str] = &[
    "TcpStream",
    "UdpSocket",
    "reqwest",
    "ureq",
    "http::",
    "hyper",
    "curl",
];

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
fn quota_source_contains_no_network_symbols() {
    let hits = scan_for_forbidden_symbols(&crate_src_dir());
    assert!(
        hits.is_empty(),
        "found network-capable symbol(s) in libra-governor-quota-source's source: {hits:?} — \
         this crate ships no live adapter (HORO-1764's own scope boundary) and must never make \
         a network call"
    );
}

#[test]
fn the_walk_actually_finds_real_files() {
    let mut files = Vec::new();
    collect_rs_files(&crate_src_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "the scanner found zero .rs files under src/ — it is not exercising anything"
    );
    assert!(files.iter().any(|f| f.ends_with("response.rs")));
}

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

#[test]
fn scanner_does_not_flag_clean_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("clean.rs"), "fn f() -> u32 { 42 }\n").unwrap();
    assert!(scan_for_forbidden_symbols(dir.path()).is_empty());
}
