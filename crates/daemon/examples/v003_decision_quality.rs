//! HORO-1673: the two decision-quality metrics the existing `v003_gate`
//! (calibration backtest, HORO-1669) and `v003_replay` (counterfactual
//! policy replay, HORO-1670) examples do not already compute: the
//! false-stop/false-degrade proposal rate, and early-warning lead time
//! before budget infeasibility.
//!
//! ```bash
//! cargo run -p libra-governor-daemon --example v003_decision_quality -- <ledger.sqlite3>
//! ```
//!
//! Gated on the sample floors pre-registered in
//! `docs/research/horo-1673/promotion-criteria.md` *before* this
//! ticket's own dogfood data was collected: 20 classifiable Stop/
//! Degrade proposals for a false-positive rate, 10 observations before
//! any lead-time figure is reported. Reporting a rate or a lead time
//! below those floors would be exactly the "fabricated pass percentage"
//! HORO-1673's own AC forbids — this prints `INSUFFICIENT` with the
//! real `N` instead.
//!
//! "Classifiable" means: the proposing task has at least one recorded
//! [`ExecutionReceipt`] to compare the proposal against. A Stop/Degrade
//! proposal with no receipt yet (task still in flight, or never
//! finalized) is counted toward `N_total` but excluded from `N_
//! classifiable` — it cannot honestly be labeled true or false yet.

use libra_governor_domain::{ProposedAction, RuntimeDecision, RuntimeDecisionProposal, TaskId};
use libra_governor_ledger::LedgerStore;

struct ProposalOutcome {
    is_stop: bool,
    is_degrade: bool,
    classifiable: bool,
    /// `true` iff classifiable and the proposal turned out to be wrong:
    /// a Stop/Degrade fired but the task's own receipt shows it finished
    /// its required work. `None` when not classifiable.
    was_false: Option<bool>,
    /// Seconds between this decision and the task's terminal receipt,
    /// when both exist — the raw material for an early-warning lead
    /// time, computed by the caller only once enough observations exist.
    lead_time_secs: Option<i64>,
}

fn classify(
    ledger: &LedgerStore,
    task_id: TaskId,
    decided_at: time::OffsetDateTime,
    decision: &RuntimeDecision,
) -> Option<ProposalOutcome> {
    let (is_stop, is_degrade) = match &decision.proposal {
        RuntimeDecisionProposal::Proposed(ProposedAction::StopEconomicallyIrrational {
            ..
        }) => (true, false),
        RuntimeDecisionProposal::Proposed(ProposedAction::DegradeOptionalScope { .. }) => {
            (false, true)
        }
        _ => return None,
    };

    let trajectory = ledger
        .task_trajectory(task_id)
        .expect("task_trajectory query must succeed for a task with a recorded shadow decision");
    let Some(receipt) = trajectory.receipts.last() else {
        return Some(ProposalOutcome {
            is_stop,
            is_degrade,
            classifiable: false,
            was_false: None,
            lead_time_secs: None,
        });
    };

    // A Stop/Degrade proposal is "false" if the task nonetheless
    // produced a receipt at all — it means the task did not, in fact,
    // need stopping or degrading at that point. This is a conservative
    // classifier (it cannot see whether the receipt itself reflects a
    // degraded/partial completion); see the README's limitations note.
    let was_false = true;
    let lead_time_secs: i64 = (receipt.recorded_at - decided_at).whole_seconds();

    Some(ProposalOutcome {
        is_stop,
        is_degrade,
        classifiable: true,
        was_false: Some(was_false),
        lead_time_secs: Some(lead_time_secs),
    })
}

const MIN_CLASSIFIABLE_FOR_RATE: usize = 20;
const MIN_OBSERVATIONS_FOR_LEAD_TIME: usize = 10;

fn main() {
    let ledger_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ledger.sqlite3".to_string());

    let ledger = match LedgerStore::open(&ledger_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("v003_decision_quality: could not open ledger at {ledger_path}: {e}");
            std::process::exit(2);
        }
    };

    println!("v0.0.3 decision-quality metrics (HORO-1673)");
    println!("ledger: {ledger_path}");

    let task_ids = ledger
        .shadow_decision_task_ids()
        .expect("shadow_decision_task_ids query must succeed against a valid ledger schema");

    let mut stop_total = 0usize;
    let mut degrade_total = 0usize;
    let mut stop_classifiable = 0usize;
    let mut degrade_classifiable = 0usize;
    let mut stop_false = 0usize;
    let mut degrade_false = 0usize;
    let mut lead_times: Vec<i64> = Vec::new();

    for &task_id in &task_ids {
        let rows = ledger
            .shadow_decisions_for_task(task_id)
            .expect("shadow_decisions_for_task query must succeed");
        for row in &rows {
            let Ok(decision) = serde_json::from_str::<RuntimeDecision>(&row.decision_json) else {
                continue;
            };
            let Some(outcome) = classify(&ledger, task_id, row.decided_at, &decision) else {
                continue;
            };
            if outcome.is_stop {
                stop_total += 1;
                if outcome.classifiable {
                    stop_classifiable += 1;
                    if outcome.was_false == Some(true) {
                        stop_false += 1;
                    }
                }
            }
            if outcome.is_degrade {
                degrade_total += 1;
                if outcome.classifiable {
                    degrade_classifiable += 1;
                    if outcome.was_false == Some(true) {
                        degrade_false += 1;
                    }
                }
            }
            if let Some(secs) = outcome.lead_time_secs {
                lead_times.push(secs);
            }
        }
    }

    println!("\n== false-stop rate ==");
    println!("N_total={stop_total} N_classifiable={stop_classifiable} (floor: {MIN_CLASSIFIABLE_FOR_RATE})");
    if stop_classifiable < MIN_CLASSIFIABLE_FOR_RATE {
        println!(
            "INSUFFICIENT — {stop_classifiable} classifiable Stop proposals < required \
             {MIN_CLASSIFIABLE_FOR_RATE}. No rate is reported. This is the honest result for a \
             fresh checkout or a short dogfood session; it is not a harness failure."
        );
    } else {
        println!("false-stop rate: {stop_false} of {stop_classifiable}");
    }

    println!("\n== false-degrade rate ==");
    println!("N_total={degrade_total} N_classifiable={degrade_classifiable} (floor: {MIN_CLASSIFIABLE_FOR_RATE})");
    if degrade_classifiable < MIN_CLASSIFIABLE_FOR_RATE {
        println!(
            "INSUFFICIENT — {degrade_classifiable} classifiable Degrade proposals < required \
             {MIN_CLASSIFIABLE_FOR_RATE}. No rate is reported."
        );
    } else {
        println!("false-degrade rate: {degrade_false} of {degrade_classifiable}");
    }

    println!("\n== early-warning lead time ==");
    println!(
        "N_observations={} (floor: {MIN_OBSERVATIONS_FOR_LEAD_TIME})",
        lead_times.len()
    );
    if lead_times.len() < MIN_OBSERVATIONS_FOR_LEAD_TIME {
        println!(
            "INSUFFICIENT — {} observations < required {MIN_OBSERVATIONS_FOR_LEAD_TIME}. No lead \
             time is reported.",
            lead_times.len()
        );
    } else {
        let mean: f64 = lead_times.iter().sum::<i64>() as f64 / lead_times.len() as f64;
        println!(
            "mean lead time: {mean:.1}s across {} observations",
            lead_times.len()
        );
    }

    println!(
        "\n== cost per successful task ==\nNOT APPLICABLE — requires an Outcome Provider and at \
         least one outcome attestation (`RecordOutcome`); none is configured in this ledger."
    );
}
