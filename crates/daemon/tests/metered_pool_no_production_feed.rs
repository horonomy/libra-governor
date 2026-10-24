//! Static guard (HORO-1915): confirms the metered shared-pool is NOT
//! wired to any production provisioning/feed path. This is the regression
//! test for exactly the trust-boundary line the founder decision drew —
//! see `docs/adr/0023-metered-shared-pool.md` and HORO-1915's Jira
//! comment 27743. If this test ever needs to change, that is itself the
//! signal that a separate, explicit trust-boundary authorization is
//! required before merging — not a reason to just update the test.

use std::fs;
use std::path::Path;

fn read_src(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {full:?}: {e}"))
}

fn walk_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_production_call_site_provisions_or_feeds_a_metered_pool() {
    // Every `ensure_pool(`/`ingest_pool_provider_snapshot(` call site
    // under `crates/daemon/src` or `crates/cli/src` must be the launch
    // gate's own `ensure_pool` call (the one pre-existing, unrelated
    // production caller) or the local shared budget's single pool-creation
    // helper (founder decision D1, ADR-0026; see the exactly-once test
    // below) — never anything else metered-pool-shaped, and never any
    // `ingest_pool_provider_snapshot` call at all.
    let daemon_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let cli_src = daemon_src
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cli")
        .join("src");

    let mut files = Vec::new();
    walk_rs_files(&daemon_src, &mut files);
    walk_rs_files(&cli_src, &mut files);

    for file in files {
        // Test modules (`#[cfg(test)]`) are exempt — this guard is about
        // the shipped binary's reachable production code, not unit tests
        // that legitimately provision a synthetic pool to exercise the
        // reserve/settle/release path in isolation (see this crate's own
        // `metered_pool.rs` test module).
        let content = fs::read_to_string(&file).unwrap_or_default();
        let is_test_file = file
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with("_test.rs") || n == "tests.rs")
            .unwrap_or(false);
        if is_test_file {
            continue;
        }
        // Strip out any `#[cfg(test)] mod tests { ... }` block heuristically
        // by only scanning lines before the first `mod tests` occurrence —
        // good enough for this repo's consistent convention of putting the
        // test module at the end of the file.
        let production_slice = match content.find("mod tests") {
            Some(idx) => &content[..idx],
            None => &content[..],
        };

        if production_slice.contains("ingest_pool_provider_snapshot(") {
            panic!(
                "{file:?} contains a production call to ingest_pool_provider_snapshot — \
                 this is the exact production-feed wiring the founder decision (HORO-1915 \
                 Jira comment 27743) explicitly did NOT authorize"
            );
        }
        for (i, line) in production_slice.lines().enumerate() {
            let allowlisted =
                file.ends_with("launch_gate.rs") || file.ends_with("shared_budget.rs");
            if line.contains("ensure_pool(") && !allowlisted {
                panic!(
                    "{file:?}:{}: unexpected production ensure_pool call site outside \
                     launch_gate.rs — this is the exact production-feed wiring the founder \
                     decision (HORO-1915 Jira comment 27743) explicitly did NOT authorize",
                    i + 1
                );
            }
        }
    }
}

#[test]
fn the_local_shared_budget_has_exactly_one_pool_creation_call_site() {
    // Founder decision D1 / ADR-0026 authorises the locally configured
    // monthly cap to provision its own pool. That authority is one helper,
    // not a pattern: a second call site anywhere in the file must be a
    // reviewed decision, so this pins the count.
    let content = read_src("src/shared_budget.rs");
    let production_slice = match content.find("mod tests") {
        Some(idx) => &content[..idx],
        None => &content[..],
    };
    assert_eq!(
        production_slice.matches("ensure_pool(").count(),
        1,
        "src/shared_budget.rs must contain exactly one pool-creation call"
    );
    assert!(
        production_slice.contains("fn ensure_period_pool"),
        "the single call site must stay inside the named helper"
    );
}

#[test]
fn the_local_shared_budget_never_reads_quota_observation_or_a_provider_snapshot() {
    for path in ["src/shared_budget.rs", "src/shared_budget_file.rs"] {
        let content = read_src(path);
        for banned in [
            "quota_observation",
            "quota-observation",
            "quota_source",
            "quota-source",
            "ingest_pool_provider_snapshot",
        ] {
            assert!(
                !content.contains(banned),
                "{path} must never reference {banned}"
            );
        }
    }
}

#[test]
fn metered_pool_module_never_references_quota_observation_or_quota_source() {
    for path in ["src/metered_pool.rs", "../cli/src/metered_pool_cmd.rs"] {
        let content = read_src(path);
        assert!(
            !content.contains("quota_observation") && !content.contains("quota-observation"),
            "{path} must never reference quota-observation — see HORO-1915's founder decision"
        );
        assert!(
            !content.contains("quota_source") && !content.contains("quota-source"),
            "{path} must never reference quota-source — see HORO-1915's founder decision"
        );
    }
}
