//! HORO-1670: the v0.0.3 counterfactual policy-replay harness.
//!
//! For every task with recorded `shadow_runtime_decisions` rows, replays
//! each of [`NAMED_PRESETS`] against the task's own recorded decision
//! points (`libra_governor_domain::replay`) and reports, per candidate
//! preset: how often it would have agreed with the actually-recorded
//! policy, how often it would have been more/less restrictive, and
//! whether it would have dropped a required completion criterion
//! (quality-floor violation — reported separately, never folded into a
//! disagreement count. See [`PolicyComparison`]).
//!
//! Replay reads every decision point's recorded
//! [`libra_governor_domain::RemainingWorkEstimate`] verbatim — this
//! binary never calls `remaining_bucketed` or reads receipt history. See
//! `libra_governor_domain::replay` module docs for why that is the single
//! most load-bearing property here.
//!
//! ```bash
//! cargo run -p libra-governor-daemon --example v003_replay -- <ledger.sqlite3> [--task <id>]
//! ```
//!
//! Zero eligible decision points (a fresh checkout, or every recorded row
//! predates migration `0014`) is reported as an honest `Insufficient`,
//! never a fabricated number.

use std::path::PathBuf;

use libra_governor_domain::{
    preset_by_name, DecisionPoint, PersistedPins, Policy, PolicyComparison, PolicyPresetInputs,
    ReplayEligibility, ReplayPins, RuntimeDecision, TaskId, NAMED_PRESETS,
};
use libra_governor_ledger::{LedgerStore, RecordedShadowDecision};

fn decode_point(row: &RecordedShadowDecision) -> Option<(DecisionPoint, ReplayEligibility)> {
    let (Some(policy_json), Some(pins_json)) = (&row.policy_json, &row.pins_json) else {
        return None;
    };

    // `Shadow<RuntimeDecision>` is `#[serde(transparent)]` and carries no
    // `Deserialize` impl (deliberately — see its module docs); its
    // serialized JSON is therefore byte-identical to `RuntimeDecision`'s
    // own, so decoding straight into `RuntimeDecision` here is the
    // intended reconstruction path for replay, not a bypass of that
    // guarantee (no daemon act-path does this).
    let recorded_decision: RuntimeDecision = match serde_json::from_str(&row.decision_json) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("v003_replay: skipping undecodable decision_json: {e}");
            return None;
        }
    };
    let recorded_policy: Policy = match serde_json::from_str(policy_json) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("v003_replay: skipping undecodable policy_json: {e}");
            return None;
        }
    };
    let persisted_pins: PersistedPins = match serde_json::from_str(pins_json) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("v003_replay: skipping undecodable pins_json: {e}");
            return None;
        }
    };

    let pins = ReplayPins::from_decision(&recorded_decision, &persisted_pins);
    let current = ReplayPins::from_decision(&recorded_decision, &PersistedPins::current());
    let eligibility = match pins.comparison(&current) {
        libra_governor_domain::PinComparison::Identical => ReplayEligibility::Identical,
        libra_governor_domain::PinComparison::Drifted { differing } => {
            ReplayEligibility::Drifted { differing }
        }
    };

    let point = DecisionPoint {
        task_id: row.task_id,
        plan_id: row.plan_id,
        session_id: row.session_id.clone(),
        decided_at: row.decided_at,
        recorded_policy,
        pins,
        recorded_decision,
    };
    Some((point, eligibility))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut ledger_path: Option<PathBuf> = None;
    let mut only_task: Option<TaskId> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--task" => {
                let raw = args.next().expect("--task requires a value");
                only_task = Some(TaskId(
                    uuid::Uuid::parse_str(&raw).expect("--task value must be a UUID"),
                ));
            }
            other => ledger_path = Some(PathBuf::from(other)),
        }
    }
    let ledger_path = ledger_path.unwrap_or_else(|| PathBuf::from("ledger.sqlite3"));

    let ledger = match LedgerStore::open(&ledger_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("v003_replay: could not open ledger at {ledger_path:?}: {e}");
            std::process::exit(2);
        }
    };

    println!("v0.0.3 counterfactual policy replay");
    println!("ledger: {ledger_path:?}");

    let task_ids = match only_task {
        Some(t) => vec![t],
        None => ledger
            .shadow_decision_task_ids()
            .expect("shadow_decision_task_ids query must succeed against a valid ledger schema"),
    };

    if task_ids.is_empty() {
        println!(
            "INSUFFICIENT — no task has any recorded shadow decision yet. This is the honest \
             result for a fresh checkout or CI run; it is not a harness failure."
        );
        return;
    }

    let mut any_replayed = false;

    for &candidate_name in NAMED_PRESETS.iter() {
        println!("\n== candidate preset: {candidate_name} ==");
        let mut total_replayed = 0usize;
        let mut total_skipped = 0usize;
        let mut agree = 0u32;
        let mut more_restrictive = 0u32;
        let mut more_permissive = 0u32;
        let mut mixed = 0u32;
        let mut no_comparable = 0u32;
        let mut floor_violations = 0usize;

        for &task_id in &task_ids {
            let rows = ledger
                .shadow_decisions_for_task(task_id)
                .expect("shadow_decisions_for_task query must succeed");
            let points: Vec<_> = rows.iter().filter_map(decode_point).collect();
            if points.is_empty() {
                continue;
            }

            let Some(first_recorded) = points.first().map(|(p, _)| p.recorded_policy.clone())
            else {
                continue;
            };
            let inputs = PolicyPresetInputs::from_recorded_policy(&first_recorded);
            let candidate = match preset_by_name(candidate_name, inputs) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("v003_replay: candidate preset {candidate_name} invalid for task {task_id:?}: {e}");
                    continue;
                }
            };

            let Some(comparison) = libra_governor_domain::replay_trajectory(points, &candidate)
            else {
                continue;
            };
            any_replayed = true;

            match comparison {
                PolicyComparison::QualityFloorViolated { .. } => {
                    floor_violations += 1;
                }
                PolicyComparison::QualityFloorPreserved { regret } => {
                    total_replayed += regret.points_replayed;
                    total_skipped += regret.points_skipped;
                    agree += regret.disagreement_counts.agree;
                    more_restrictive += regret.disagreement_counts.candidate_more_restrictive;
                    more_permissive += regret.disagreement_counts.candidate_more_permissive;
                    mixed += regret.disagreement_counts.mixed;
                    no_comparable += regret.disagreement_counts.no_comparable_dimension;
                }
            }
        }

        println!(
            "points_replayed={total_replayed} points_skipped={total_skipped} \
             agree={agree} more_restrictive={more_restrictive} more_permissive={more_permissive} \
             mixed={mixed} no_comparable_dimension={no_comparable} \
             quality_floor_violations={floor_violations}"
        );
    }

    if !any_replayed {
        println!(
            "\nINSUFFICIENT — every recorded shadow decision lacked a cohort to replay against \
             (unpinned rows, or no two rows shared a cohort). This is the honest result; it is \
             not a harness failure."
        );
    }
}
