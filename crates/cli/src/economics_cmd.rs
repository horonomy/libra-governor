//! `libra-governor economics explain` (HORO-1672) — reconstructs "what
//! did this cost, really" entirely from the real persisted ledger
//! tables, without manual DB inspection.
//!
//! Thin per this repo's own architecture rule: every number here is
//! read straight through
//! `libra_governor_ledger::LedgerStore::economic_truth_for_*`, which
//! itself only ever reuses or narrowly extends existing row readers —
//! no new policy/business logic is introduced in this module.
//!
//! `--principal`/`--organization` are accepted selectors (never rejected
//! as unknown), always answered [`NoEconomicBasis::NotConfigured`] in
//! v0.0.3 — see `libra_governor_domain::economic_truth` module docs on
//! why no configured principal/organization account exists yet.

use libra_governor_domain::{
    EconomicTruth, NoEconomicBasis, SelectorEcho, ECONOMIC_TRUTH_SCHEMA_VERSION,
};
use libra_governor_ledger::LedgerStore;

fn open_ledger() -> Result<LedgerStore, String> {
    let ledger_path = libra_governor_daemon::paths::ledger_path()
        .map_err(|e| format!("could not resolve ledger path: {e}"))?;
    LedgerStore::open(&ledger_path).map_err(|e| {
        format!(
            "could not open local ledger at {}: {e}",
            ledger_path.display()
        )
    })
}

/// `libra-governor economics explain --task|--session|--account|--principal|--organization <value> [--json]`.
pub fn run(selector: &str, value: &str, json: bool) {
    let truth = match selector {
        "--task" => {
            let Ok(uuid) = uuid::Uuid::parse_str(value) else {
                render_and_exit(
                    EconomicTruth::no_basis(
                        SelectorEcho {
                            selector: "task".to_string(),
                            value: value.to_string(),
                        },
                        NoEconomicBasis::NoSuchTask,
                    ),
                    json,
                );
            };
            let store = open_or_exit();
            store
                .economic_truth_for_task(libra_governor_domain::TaskId(uuid))
                .unwrap_or_else(|e| fail(&format!("could not read ledger: {e}")))
        }
        "--session" => {
            let store = open_or_exit();
            store
                .economic_truth_for_session(value)
                .unwrap_or_else(|e| fail(&format!("could not read ledger: {e}")))
        }
        "--account" => {
            let store = open_or_exit();
            store
                .economic_truth_for_account_str(value)
                .unwrap_or_else(|e| fail(&format!("could not read ledger: {e}")))
        }
        "--principal" | "--organization" => EconomicTruth::no_basis(
            SelectorEcho {
                selector: if selector == "--principal" {
                    "principal".to_string()
                } else {
                    "organization".to_string()
                },
                value: value.to_string(),
            },
            NoEconomicBasis::NotConfigured,
        ),
        other => {
            eprintln!(
                "libra-governor economics explain: unknown selector {other}\n\n\
                 Usage: libra-governor economics explain --task|--session|--account|--principal|--organization <value> [--json]"
            );
            std::process::exit(2);
        }
    };
    render_and_exit(truth, json);
}

fn open_or_exit() -> LedgerStore {
    match open_ledger() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("libra-governor economics explain: {e}");
            std::process::exit(1);
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("libra-governor economics explain: {message}");
    std::process::exit(1);
}

fn render_and_exit(truth: EconomicTruth, json: bool) -> ! {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&truth).unwrap_or_else(|_| format!(
                "{{\"schema_version\":\"{ECONOMIC_TRUTH_SCHEMA_VERSION}\",\"error\":\"serialization_failed\"}}"
            ))
        );
    } else {
        println!("{}", render_human(&truth));
    }
    std::process::exit(0);
}

fn render_human(truth: &EconomicTruth) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "economic truth — selector: {} = {}",
        truth.selector.selector, truth.selector.value
    );
    match &truth.resolution {
        libra_governor_domain::ScopeResolution::NoBasis { reason } => {
            let _ = writeln!(out, "  no economic basis: {reason:?}");
            return out;
        }
        libra_governor_domain::ScopeResolution::Account { account_id } => {
            let _ = writeln!(out, "  resolved account: {account_id}");
        }
        libra_governor_domain::ScopeResolution::FilteredWithin { account_id, filter } => {
            let _ = writeln!(
                out,
                "  resolved within account {account_id} (filter: {filter:?})"
            );
        }
    }

    if let Some(categories) = &truth.categories {
        let _ = writeln!(
            out,
            "  allocation: {:?}",
            categories.allocation.granted_capacity
        );
        let _ = writeln!(
            out,
            "  active leases (count {}): work_hold={:?} subaccount_funding={:?}",
            categories.active_leases.count,
            categories.active_leases.work_hold_required,
            categories.active_leases.subaccount_funding,
        );
        let _ = writeln!(
            out,
            "  settled: observed={:?} assumed={:?} (never summed as one figure)",
            categories.settled.observed, categories.settled.assumed
        );
        let _ = writeln!(
            out,
            "  totals: exclusive={:?} inclusive={:?}",
            categories.totals.exclusive, categories.totals.inclusive
        );
        let _ =
            writeln!(
            out,
            "  unattributed: legacy={} gateway_no_task={} gateway_no_lease={} orphan_accounts={}",
            categories.unattributed.legacy_backfilled.row_count,
            categories.unattributed.gateway_rows_without_task.row_count,
            categories.unattributed.gateway_settled_without_lease.row_count,
            categories.unattributed.accounts_without_proven_lineage,
        );
    }

    if let Some(tree) = &truth.custody_tree {
        let _ = writeln!(
            out,
            "  custody tree: {} nodes{}",
            tree.nodes.len(),
            match tree.truncated {
                Some(t) => format!(" (truncated: {t:?})"),
                None => String::new(),
            }
        );
    }

    if let Some(recon) = &truth.reconciliation {
        let _ = writeln!(
            out,
            "  reconciliation: all_reconciled={}",
            recon.all_reconciled
        );
        for check in &recon.checks {
            let _ = writeln!(out, "    {:?}: {:?}", check.check, check.outcome);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_human_never_panics_on_no_basis() {
        let truth = EconomicTruth::no_basis(
            SelectorEcho {
                selector: "task".to_string(),
                value: "missing".to_string(),
            },
            NoEconomicBasis::NoSuchTask,
        );
        let rendered = render_human(&truth);
        assert!(rendered.contains("no economic basis"));
    }
}
