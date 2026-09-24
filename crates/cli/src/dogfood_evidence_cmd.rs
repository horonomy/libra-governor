//! `libra-governor dogfood-evidence export` (HORO-1376) — projects the
//! existing local ledger into ADR-0012 §3 evidence events and writes
//! them as local NDJSON, under the exact same consent gate as
//! `evidence_report_cmd.rs`'s `evidence-report`.
//!
//! # Relationship to `evidence_report_cmd.rs`
//!
//! This is a **separate module and a separate CLI surface** from
//! `evidence_report_cmd.rs`. That module (HORO-1154) is explicitly
//! zero-network by design and regression-guarded by a static source-text
//! scan of itself (`evidence_report_module_source_contains_no_network_symbols`).
//! ADR-0012 §11.4 is explicit that any future transport must live
//! **outside** that module boundary so its guard keeps meaning what it
//! says — this module is that "outside", and it carries its **own**
//! copy of the identical guard (below), so it is held to the same
//! standard from day one rather than inheriting it implicitly.
//!
//! This module never modifies, imports from as a dependency edge beyond
//! sharing the consent filename convention, or otherwise touches
//! `evidence_report_cmd.rs` — that file is untouched by this PR (verify
//! with `git diff origin/main -- crates/cli/src/evidence_report_cmd.rs`).
//!
//! # Consent
//!
//! Reuses the exact same consent marker file
//! (`evidence_report_cmd::CONSENT_FILE_NAME`, already `pub`) as
//! `evidence-report` — one opt-in covers both local export tools, since
//! both are "local, opt-in, evaluator-facing" in the same sense. This
//! module cannot call `evidence_report_cmd::read_consent` (private), so
//! it re-reads the file itself; [`tests::both_modules_agree_on_the_consent_filename`]
//! guards against the two ever drifting onto different filenames.
//!
//! # `local_only` only, in v1
//!
//! `export` always uses `libra_governor_evidence_adapter::TransportPolicy::LocalOnly`.
//! There is no CLI flag to request `manual_window`/`monthly_window` —
//! the adapter crate itself refuses to construct them
//! (`AdapterConfig::validate`, DFC-ADAPT-08); this command does not even
//! expose the option.

use std::path::{Path, PathBuf};

use libra_governor_evidence_adapter::{AdapterConfig, TransportPolicy};
use libra_governor_ledger::LedgerStore;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Deliberately identical string to `evidence_report_cmd::CONSENT_FILE_NAME`
/// — see [`tests::both_modules_agree_on_the_consent_filename`].
const DOGFOOD_CONSENT_FILE_NAME: &str = "evidence_consent.json";

const DOGFOOD_EVIDENCE_DIR_NAME: &str = "dogfood-evidence";

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

fn consent_present(consent_path: &Path) -> bool {
    std::fs::read(consent_path).is_ok()
}

/// `libra-governor dogfood-evidence export` — refuses without consent
/// (same marker file as `evidence-report`); with consent, builds every
/// evidence event this ledger's plan/receipt rows carry a
/// `dogfood_event_id` for, and writes them as one NDJSON file. Prints
/// the file path and nothing else.
pub fn run() {
    let consent_path = match libra_governor_daemon::paths::state_dir() {
        Ok(dir) => dir.join(DOGFOOD_CONSENT_FILE_NAME),
        Err(e) => {
            eprintln!("libra-governor dogfood-evidence export: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };
    if !consent_present(&consent_path) {
        eprintln!(
            "libra-governor dogfood-evidence export: no consent on record at {}.\n\
             This tool refuses to run without your explicit opt-in — nothing is read or \
             exported until you consent.\n\
             Run `libra-governor evidence-report consent` first (the same consent marker \
             covers both local export tools), then re-run this command.",
            consent_path.display()
        );
        std::process::exit(1);
    }

    let state_dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor dogfood-evidence export: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };

    let ledger_path = match libra_governor_daemon::paths::ledger_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor dogfood-evidence export: could not resolve ledger path: {e}");
            std::process::exit(1);
        }
    };

    let events = if ledger_path.exists() {
        match LedgerStore::open(&ledger_path) {
            Ok(store) => {
                let config = AdapterConfig::new(TransportPolicy::LocalOnly)
                    .expect("LocalOnly always validates");
                match libra_governor_evidence_adapter::build_events(
                    &store,
                    &config,
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_VERSION"),
                ) {
                    Ok(events) => events,
                    Err(e) => {
                        eprintln!(
                            "libra-governor dogfood-evidence export: could not build evidence \
                             events: {e}"
                        );
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "libra-governor dogfood-evidence export: could not open local ledger at \
                     {}: {e}",
                    ledger_path.display()
                );
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };

    let out_dir = state_dir.join(DOGFOOD_EVIDENCE_DIR_NAME);
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "libra-governor dogfood-evidence export: could not create {}: {e}",
            out_dir.display()
        );
        std::process::exit(1);
    }
    let stamp = now_rfc3339().replace([':', '.'], "-");
    let out_path: PathBuf = out_dir.join(format!("{stamp}.ndjson"));

    let mut ndjson = String::new();
    for event in &events {
        match serde_json::to_string(event) {
            Ok(line) => {
                ndjson.push_str(&line);
                ndjson.push('\n');
            }
            Err(e) => {
                eprintln!(
                    "libra-governor dogfood-evidence export: could not serialize an event: {e}"
                );
                std::process::exit(1);
            }
        }
    }

    #[cfg(unix)]
    let write_result = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&out_path, ndjson.as_bytes()).and_then(|_| {
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o600))
        })
    };
    #[cfg(not(unix))]
    let write_result = std::fs::write(&out_path, ndjson.as_bytes());

    if let Err(e) = write_result {
        eprintln!(
            "libra-governor dogfood-evidence export: could not write {}: {e}",
            out_path.display()
        );
        std::process::exit(1);
    }

    println!(
        "Local DogFood evidence export written:\n  {}\n\n\
         {} event(s), transport=local_only. Nothing was sent anywhere — this tool has no \
         network capability by design (ADR-0012 §11.4).",
        out_path.display(),
        events.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for this module's own "no network call, ever"
    /// claim — the identical fixed symbol list and identical
    /// self-scanning-via-`include_str!` technique as
    /// `evidence_report_cmd::evidence_report_module_source_contains_no_network_symbols`,
    /// so this newer CLI surface is held to the same standard from day
    /// one rather than inheriting it implicitly. Kept inside `mod tests`
    /// deliberately: if the symbol list lived in production code, this
    /// file's own scan of its pre-`#[cfg(test)]` half would find
    /// `"reqwest"` inside the list literal and fail against itself.
    const FORBIDDEN_SYMBOLS: &[&str] = &[
        "TcpStream",
        "UdpSocket",
        "reqwest",
        "ureq",
        "http::",
        "hyper",
        "curl",
    ];

    fn scan(source: &str) -> Vec<&'static str> {
        FORBIDDEN_SYMBOLS
            .iter()
            .copied()
            .filter(|symbol| source.contains(symbol))
            .collect()
    }

    #[test]
    fn dogfood_evidence_cmd_module_source_contains_no_network_symbols() {
        let source = include_str!("dogfood_evidence_cmd.rs");
        let production_source = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("this file always contains its own test module marker");
        let hits = scan(production_source);
        assert!(
            hits.is_empty(),
            "found network-capable symbol(s) {hits:?} in dogfood_evidence_cmd.rs's production \
             code — this module must never make a network call (ADR-0012 §11.4)"
        );
    }

    /// Negative control: the scan function itself must actually flag a
    /// planted violation, proving it is a real check.
    #[test]
    fn scanner_flags_a_planted_network_symbol() {
        let hits = scan("use reqwest::Client;\nfn f(){}\n");
        assert_eq!(hits, vec!["reqwest"]);
    }

    #[test]
    fn scanner_does_not_flag_clean_source() {
        assert!(scan("fn f() -> u32 { 42 }\n").is_empty());
    }

    /// Guards against this module and `evidence_report_cmd` silently
    /// drifting onto two different consent mechanisms — see this
    /// module's doc comment.
    #[test]
    fn both_modules_agree_on_the_consent_filename() {
        assert_eq!(
            DOGFOOD_CONSENT_FILE_NAME,
            crate::evidence_report_cmd::CONSENT_FILE_NAME
        );
    }
}
