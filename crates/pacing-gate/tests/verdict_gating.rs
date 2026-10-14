//! Proves `report::decide` can never reach `GoDecision::Go` from this
//! crate's own output, regardless of what `ValueCriterion` a human
//! supplies — see `src/report.rs`'s own module docs for why.

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::report::{
    decide, rows_for_reports, EvidenceClass, GoDecision, ValueCriterion,
};
use libra_governor_pacing_gate::strategy::StaticBudget;
use libra_governor_pacing_gate::verify::{verify, StrategyMeta};

#[test]
fn go_is_unreachable_even_when_every_row_is_green_and_a_value_criterion_is_supplied() {
    // Build a report from a fixture every honest strategy completes
    // cleanly (t0_occupancy, uncapped StaticBudget) — every row this
    // produces is as "green" as this crate's own output ever gets: full
    // completion, zero violations, zero false throttles.
    let spec = fixtures::load("t0_occupancy").unwrap();
    let strategy = StaticBudget::new(spec.tasks.len());
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert_eq!(report.tasks_completed, report.tasks_total);
    assert!(report.hard_limit_violations.is_empty());
    assert!(report.principal_violations.is_empty());

    let rows = rows_for_reports(&[report]);
    assert!(
        !rows.is_empty(),
        "sanity: there must be at least one row for the gating check to be meaningful"
    );
    assert!(
        rows.iter().all(|r| r.evidence == EvidenceClass::Simulation),
        "every row this crate produces must be Simulation"
    );

    let value_criterion = Some(ValueCriterion {
        signed_off_by: "a human reviewer".to_string(),
        note: "every simulated metric looks good".to_string(),
    });
    let verdict = decide(rows, value_criterion);

    assert_eq!(
        verdict.decision,
        GoDecision::NoGoPreview,
        "Go must be structurally unreachable from Simulation-only evidence, even with every \
         row green and a value_criterion supplied"
    );
}

#[test]
fn go_is_unreachable_without_a_value_criterion_even_if_evidence_were_non_simulation() {
    use libra_governor_pacing_gate::report::MetricRow;

    let rows = vec![MetricRow {
        label: "hypothetical live row".to_string(),
        evidence: EvidenceClass::ProductionObserved,
        summary: "would be non-simulation evidence, if this crate ever produced it".to_string(),
    }];
    let verdict = decide(rows, None);
    assert_eq!(
        verdict.decision,
        GoDecision::NoGoPreview,
        "Go must require an explicit human value_criterion even given non-simulation evidence"
    );
}
