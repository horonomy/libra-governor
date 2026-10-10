//! Zero-live-wiring static guard for authoritative outcome attestation
//! (ADR-0017, HORO-1727), cloned from
//! `crates/ledger/tests/renewal_not_wired_live.rs`'s pattern — a
//! test-run-time scan of `src/**/*.rs`, not a single `include_str!`
//! (which would only ever scan the one file it is compiled into).
//!
//! ADR-0017 found that no code-level change can make the authoritative
//! (`Provider`-variant) trust boundary of `AttestationSource` hold on
//! this single-workstation, same-OS-user deployment. This test fails
//! the build if `daemon/src`, `cli/src`, or `gateway/src` ever construct
//! the authoritative `Provider` or `GovernorLocal` variant of
//! `AttestationSource`, or write a literal authoritative-true flag to the
//! ledger, so reintroducing an authoritative path is a deliberate,
//! reviewed decision (updating or deleting this guard) rather than a
//! silent drift back to "every push is authoritative".
//!
//! # One allowed exception, one allowed file
//!
//! `crates/daemon/src/outcome_authority_wiring.rs` (HORO-1727 PR 5b) is
//! the single file this scan skips entirely — see that module's own docs
//! for why its construction of the authoritative variant is reviewed and
//! intentional, gated on `DaemonConfig::outcome_authority` (itself never
//! `Some` in production — see that field's docs and the second test
//! below). Every other file in the scanned crates is held to the zero-hit
//! bar.
//!
//! # Why this does not also scan `ledger/src`/`domain/src`
//!
//! Both crates' own unit tests legitimately construct every
//! `AttestationSource` variant and `authoritative: true` directly —
//! that's how `libra_governor_ledger::insert_outcome_attestation`,
//! `record_outcome_attestation`, and the `AttestationSource` type itself
//! are tested at their own layer, entirely independent of whether any
//! real request path can reach that state. A plain substring scan has no
//! way to distinguish that legitimate `#[cfg(test)]` exercise from a
//! live call site without becoming a real parser, so those two crates
//! are deliberately left out of this test's scope rather than given a
//! blanket allowlist that would mask a real future hit. The three crates
//! that ARE scanned (`daemon`, `cli`, `gateway`) are exactly the ones
//! that handle a live request end to end; this is the reviewed,
//! documented scope — not an oversight.
//!
//! This file's own scan patterns are intentionally not spelled out in
//! this module doc comment in their exact matched form, so that
//! documenting this guard does not itself trip it.

use std::path::{Path, PathBuf};

/// Relative to the repo root — matched against the FULL relative path,
/// not just the basename (a prior version of this guard compared only
/// `file_name()`, which would have silently exempted any other file
/// anywhere under the scanned crates that happened to share this same
/// basename; an independent review caught this before it mattered —
/// `git ls-tree` confirms exactly one file with this name exists today).
const ALLOWED_FILE: &str = "crates/daemon/src/outcome_authority_wiring.rs";

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

fn scan_for_patterns<'a>(
    repo_root: &Path,
    root: &Path,
    patterns: &[&'a str],
) -> Vec<(PathBuf, &'a str)> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut hits = Vec::new();
    for file in files {
        let relative = file.strip_prefix(repo_root).unwrap_or(&file);
        if relative.to_str() == Some(ALLOWED_FILE) {
            continue;
        }
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
fn daemon_cli_and_gateway_never_construct_an_authoritative_attestation_source() {
    let root = repo_root();
    // "AttestationSource :: Provider" / "AttestationSource :: GovernorLocal"
    // written without a space to match the real source formatting, kept
    // as two separate needles. This is a plain substring scan, not a
    // parser: it catches the qualified form used everywhere in this
    // codebase today, but a `use AttestationSource::Provider as X;`
    // import alias or a bare unqualified `Provider { .. }` after a glob
    // `use` would defeat it -- no such alias exists today (checked via
    // `git grep`), and adding a bare-`Provider`-only pattern would cause
    // too many false positives (it is a common identifier) to be worth
    // the trade-off for a lightweight guard. Likewise `authoritative:
    // true` only catches the literal boolean, not a renamed variable or
    // a positional bool -- the one real call site passes a variable, so
    // this is a real but currently-harmless limitation, documented
    // rather than silently assumed away.
    let patterns = [
        "AttestationSource::Provider",
        "AttestationSource::GovernorLocal",
        "authoritative: true",
    ];
    for crate_name in ["daemon", "cli", "gateway"] {
        let src = root.join("crates").join(crate_name).join("src");
        if !src.exists() {
            panic!("expected {crate_name}'s src directory to exist at {src:?}");
        }
        let hits = scan_for_patterns(&root, &src, &patterns);
        assert!(
            hits.is_empty(),
            "ADR-0017 (HORO-1727): no code-level fix can make an authoritative \
             attestation trust boundary hold on this single-workstation, same-OS-user \
             deployment. `{crate_name}` must not construct `AttestationSource::Provider`/\
             `GovernorLocal` or write `authoritative: true` outside {ALLOWED_FILE} until a \
             genuinely separate-principal deployment exists and this guard is deliberately \
             updated. Found: {hits:?}"
        );
    }
}

/// Decision 1's other half: even the one allowed construction site is
/// inert unless `DaemonConfig::outcome_authority` is `Some`. This asserts
/// no code in `daemon/src` or `cli/src` ever writes that, OUTSIDE the one
/// allowed file (`outcome_authority_wiring.rs`'s own `#[cfg(test)]`
/// fixtures, already excluded by `scan_for_patterns`'s `ALLOWED_FILE`
/// skip) -- a config file structurally cannot populate it either
/// (`config_file::load_overrides` returns no such value at all), and the
/// one real daemon entry point (`cli/src/daemon_cmd.rs::run`) must keep
/// hardcoding `None`.
///
/// Like the test above, this is a literal-substring scan, so it only
/// catches the struct-construction shape `outcome_authority: Some(` —
/// not a later field assignment (`config.outcome_authority = Some(..)`)
/// or a bound-variable construction (`outcome_authority: resolved,`
/// where `resolved` happens to be `Some(..)`). Confirmed (by an
/// independent review) that `daemon/src`/`cli/src` have exactly one
/// `DaemonConfig` construction site in production code
/// (`cli/src/daemon_cmd.rs::run`) and no later mutation of this field
/// anywhere, so today's real guarantee rests on that fact, not purely on
/// this scan — documented rather than silently assumed.
#[test]
fn nothing_outside_the_one_allowed_file_ever_sets_outcome_authority_to_some() {
    let root = repo_root();
    let patterns = ["outcome_authority: Some("];
    for crate_name in ["daemon", "cli"] {
        let src = root.join("crates").join(crate_name).join("src");
        let hits = scan_for_patterns(&root, &src, &patterns);
        assert!(
            hits.is_empty(),
            "ADR-0017 (HORO-1727): no code in `{crate_name}` outside {ALLOWED_FILE} may set \
             `DaemonConfig::outcome_authority` to `Some(..)` -- this field must stay `None` \
             in every real deployment until a genuinely separate-principal deployment exists. \
             Found: {hits:?}"
        );
    }
}
