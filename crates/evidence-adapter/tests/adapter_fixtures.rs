//! Fixture-driven conformance tests for the Libra Governor DogFood
//! evidence adapter (HORO-1376). Cites ADR-0012 §16 test scenario IDs by
//! comment next to the assertion that proves each one, per
//! `dogfood-evidence-store-test-contract.md`'s "traceable to a specific
//! assertion" convention. This is a product-local suite, not the shared
//! conformance suite (that is HORO-1381's).

use libra_governor_domain::{
    Admission, ApprovalRequest, CompletionContract, CompletionCriterion, Confidence, DenyReason,
    ExecutionOutcome, ExecutionPlan, ExecutionReceipt, ResourceAmount, TaskIdentity,
};
use libra_governor_evidence_adapter::event::{Action, Coverage, GapReason};
use libra_governor_evidence_adapter::{
    summarize, AdapterConfig, AdapterConfigError, TransportPolicy,
};
use libra_governor_ledger::LedgerStore;
use time::OffsetDateTime;

fn contract() -> CompletionContract {
    CompletionContract::first(vec![CompletionCriterion {
        description: "tests pass".to_string(),
        required: true,
    }])
}

/// Seeds one task with a plan (and optional receipt) and returns nothing
/// — callers just want the store mutated. `admission` mirrors
/// `set_plan_admission`'s signature: `None` leaves the plan's admission
/// column genuinely unset (a pre-HORO-1146-shaped row), matching
/// `admission_unrecorded_count`'s existing meaning.
#[allow(clippy::too_many_arguments)]
fn seed_task(
    store: &mut LedgerStore,
    admission: Option<Admission>,
    with_receipt: bool,
    actual_usage: Vec<ResourceAmount>,
) {
    let now = OffsetDateTime::now_utc();
    let identity = TaskIdentity::new(None);
    let task_id = identity.id;
    store.insert_task(&identity, now).unwrap();
    store.insert_contract(task_id, &contract(), now).unwrap();

    let mut plan = ExecutionPlan::new(task_id, 1, None, now);
    if let Some(admission) = admission.clone() {
        plan = plan.with_admission(admission);
    }
    store.insert_plan(&plan).unwrap();
    if let Some(admission) = admission {
        store.set_plan_admission(plan.id, &admission).unwrap();
    }

    if with_receipt {
        let receipt = ExecutionReceipt::new(
            task_id,
            1,
            plan.id,
            60,
            actual_usage,
            ExecutionOutcome::Completed {
                evidence: vec!["evidence-adapter-fixture".to_string()],
            },
            now,
        );
        store.insert_receipt(&receipt).unwrap();
    }
}

/// DFC-ADAPT-07 (primary): a realistic mixed fixture — Admit+receipt,
/// Deny+no-receipt, ApprovalRequired+receipt, unrecorded-admission — all
/// build successfully and every event passes `DogfoodEvent::validate()`.
/// The zero-network static guard staying green is separately proven by
/// `tests/no_network_symbols.rs` (this crate has no `[[test]]` that could
/// disable it, and the guard runs unconditionally in the same `cargo
/// test` invocation).
#[test]
fn dfc_adapt_07_mixed_fixture_builds_valid_events() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store, Some(Admission::Admit), true, vec![]);
    seed_task(
        &mut store,
        Some(Admission::Deny(vec![
            DenyReason::ConfidenceBelowThreshold {
                actual: Confidence::Low,
                required: Confidence::High,
            },
        ])),
        false,
        vec![],
    );
    seed_task(
        &mut store,
        Some(Admission::ApprovalRequired(vec![ApprovalRequest::Time {
            projected_secs: 7200,
            target_secs: 3600,
            elastic_ceiling_secs: Some(5400),
            hard_ceiling_secs: Some(9000),
        }])),
        true,
        vec![ResourceAmount::Tokens(500)],
    );
    seed_task(&mut store, None, false, vec![]);

    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();

    // 4 plans + 2 receipts (Admit+receipt, ApprovalRequired+receipt).
    assert_eq!(events.len(), 6);
    for event in &events {
        event
            .validate()
            .expect("every built event must be well-formed");
    }
}

/// DFC-ADAPT-08: window transport is rejected at config load, with a
/// specific named error — not merely "doesn't work".
#[test]
fn dfc_adapt_08_window_transport_rejected_at_load_with_named_error() {
    let manual = AdapterConfig::new(TransportPolicy::ManualWindow);
    assert_eq!(
        manual.unwrap_err(),
        AdapterConfigError::WindowTransportNotOfferedInV1(TransportPolicy::ManualWindow)
    );

    let monthly = AdapterConfig::new(TransportPolicy::MonthlyWindow);
    assert_eq!(
        monthly.unwrap_err(),
        AdapterConfigError::WindowTransportNotOfferedInV1(TransportPolicy::MonthlyWindow)
    );

    // local_only remains legal.
    assert!(AdapterConfig::new(TransportPolicy::LocalOnly).is_ok());
}

/// DFC-MODE-01: personal + observe on a would-deny operation — the
/// operation "completed" (a receipt exists), `would_action=deny`,
/// `actual_action=allow`.
#[test]
fn dfc_mode_01_would_deny_operation_that_actually_ran() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(
        &mut store,
        Some(Admission::Deny(vec![
            DenyReason::ConfidenceBelowThreshold {
                actual: Confidence::Low,
                required: Confidence::High,
            },
        ])),
        true,
        vec![],
    );
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    let plan_event = events
        .iter()
        .find(|e| {
            e.eligibility == libra_governor_evidence_adapter::event::Eligibility::ReplayableEvidence
        })
        .unwrap();
    assert_eq!(plan_event.would_action, Some(Action::Deny));
    assert_eq!(plan_event.actual_action, Action::Allow);
}

/// DFC-MODE-02: personal profile cannot be configured to `enforce` —
/// this adapter's `build_events` never constructs `DecisionMode::Enforce`
/// at all (see `crate::adapter` module docs); every event it emits
/// validates as observe-only.
#[test]
fn dfc_mode_02_personal_profile_never_produces_enforce() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store, Some(Admission::Admit), true, vec![]);
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    assert!(events
        .iter()
        .all(|e| e.decision_mode == libra_governor_evidence_adapter::event::DecisionMode::Observe));
}

/// No emitted record ever has `actual_action=deny` alone, regardless of
/// admission verdict — the exact malformed combination ADR-0012 §3 rules
/// out for `decision_mode=observe`. Also: no record carries a fabricated
/// `scope_id` (always `None`, since `enforce` — the only mode that would
/// need one — is never produced).
#[test]
fn no_record_ever_carries_actual_action_deny_or_a_fabricated_scope_id() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(
        &mut store,
        Some(Admission::Deny(vec![
            DenyReason::ConfidenceBelowThreshold {
                actual: Confidence::Low,
                required: Confidence::High,
            },
        ])),
        false,
        vec![],
    );
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    for event in &events {
        assert_ne!(event.actual_action, Action::Deny);
        assert_eq!(event.scope_id, None);
        event.validate().unwrap();
    }
}

/// DFC-ELIG-01: a receipt (tool/command execution record) is classified
/// `non_replayable_operation`.
#[test]
fn dfc_elig_01_receipt_is_non_replayable() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store, Some(Admission::Admit), true, vec![]);
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    let receipt_events: Vec<_> = events
        .iter()
        .filter(|e| {
            e.eligibility
                == libra_governor_evidence_adapter::event::Eligibility::NonReplayableOperation
        })
        .collect();
    assert_eq!(receipt_events.len(), 1);
}

/// DFC-ELIG-05: `coverage=gap` with `gap_reason=unknown` is never
/// produced by this adapter — the only non-full coverage this adapter
/// ever emits is `partial`/`source_unavailable` (an unrecorded
/// admission), never `gap`/`unknown`.
#[test]
fn dfc_elig_05_adapter_never_emits_gap_unknown() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store, None, false, vec![]);
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    let plan_event = events.first().unwrap();
    assert_eq!(plan_event.coverage, Coverage::Partial);
    assert_eq!(plan_event.gap_reason, Some(GapReason::SourceUnavailable));
    assert_ne!(plan_event.gap_reason, Some(GapReason::Unknown));
}

/// `dropped_count=0` provability (per the ticket brief): grepping the
/// ledger crate's own source and migrations for any row-eviction path
/// confirms there is none, so a real run's `dropped_total` (DFC-RETN-02)
/// is a genuine `0`, not a placeholder. If a future change adds a byte
/// cap without incrementing a counter first, *this* test starts failing
/// the moment `DELETE FROM`/`DROP TABLE` appears, which is exactly the
/// regression this test exists to catch.
#[test]
fn dropped_count_zero_is_provable_no_eviction_path_exists_in_ledger() {
    let ledger_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ledger");
    let mut offending = Vec::new();
    for entry in walk(&ledger_src) {
        if entry.extension().and_then(|e| e.to_str()) != Some("rs")
            && entry.extension().and_then(|e| e.to_str()) != Some("sql")
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&entry) else {
            continue;
        };
        if contents.contains("DELETE FROM") || contents.contains("DROP TABLE") {
            offending.push(entry);
        }
    }
    assert!(
        offending.is_empty(),
        "found a row-eviction path in the ledger crate: {offending:?} — \
         `dropped_count=0` is no longer a provable assertion; the adapter must start \
         reporting a real nonzero dropped_count instead of a hardcoded 0"
    );

    let mut store = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store, Some(Admission::Admit), true, vec![]);
    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events =
        libra_governor_evidence_adapter::build_events(&store, &config, "0.0.2", "0.0.2").unwrap();
    let summary = summarize(&events);
    assert_eq!(summary.dropped_total, 0);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Token/currency non-fabrication negative control (per the ticket
/// brief): the frozen ADR-0012 §3 schema carries **no**
/// token/currency/usage field at all — this adapter must never widen the
/// schema to add one, and in particular must never derive a value from
/// `tool_call_count` or `actual_usage`. Proven two ways: (1) an event's
/// serialized JSON keys are exactly the frozen allowlist (a naive
/// `tool_count * some_rate` implementation that added a field would fail
/// this immediately, mirroring `evidence_report_cmd.rs`'s own
/// `exported_json_top_level_keys_match_the_expected_allowlist` pattern);
/// (2) two receipts differing only in `actual_usage` (one empty, one a
/// large token count) produce byte-identical canonical JSON once
/// `event_id`/timestamps are normalized — proving usage data has zero
/// causal influence on the emitted event, which structurally forecloses
/// fabricating a usage-derived figure.
#[test]
fn token_currency_non_fabrication_schema_has_no_usage_field_and_usage_never_influences_output() {
    let mut store_empty = LedgerStore::open_in_memory().unwrap();
    seed_task(&mut store_empty, Some(Admission::Admit), true, vec![]);
    let mut store_large = LedgerStore::open_in_memory().unwrap();
    seed_task(
        &mut store_large,
        Some(Admission::Admit),
        true,
        vec![
            ResourceAmount::Tokens(10_000_000),
            ResourceAmount::UsdCents(999_999),
        ],
    );

    let config = AdapterConfig::new(TransportPolicy::LocalOnly).unwrap();
    let events_empty =
        libra_governor_evidence_adapter::build_events(&store_empty, &config, "0.0.2", "0.0.2")
            .unwrap();
    let events_large =
        libra_governor_evidence_adapter::build_events(&store_large, &config, "0.0.2", "0.0.2")
            .unwrap();

    let receipt_empty = events_empty
        .iter()
        .find(|e| {
            e.eligibility
                == libra_governor_evidence_adapter::event::Eligibility::NonReplayableOperation
        })
        .unwrap();
    let receipt_large = events_large
        .iter()
        .find(|e| {
            e.eligibility
                == libra_governor_evidence_adapter::event::Eligibility::NonReplayableOperation
        })
        .unwrap();

    // (1) exact frozen key allowlist — a fabricated usage field would
    // show up here immediately.
    let value = serde_json::to_value(receipt_empty).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "actual_action",
            "adapter_version",
            "coverage",
            "decision_mode",
            "destination",
            "dropped_count",
            "eligibility",
            "event_id",
            "gap_reason",
            "imported",
            "ingested_at",
            "integrity",
            "occurred_at",
            "origin_profile",
            "payload_classification",
            "permanently_ineligible",
            "product",
            "product_version",
            "profile",
            "schema_version",
            "scope_id",
            "tenant_id",
            "transport_state",
            "would_action",
        ]
    );

    // (2) normalize the only fields that legitimately differ between two
    // distinct receipts (identity + timestamps + the hash that covers
    // them), then assert everything else is byte-identical.
    let mut a = receipt_empty.clone();
    let mut b = receipt_large.clone();
    a.event_id = "x".to_string();
    b.event_id = "x".to_string();
    a.occurred_at = "x".to_string();
    b.occurred_at = "x".to_string();
    a.ingested_at = "x".to_string();
    b.ingested_at = "x".to_string();
    a.integrity = libra_governor_evidence_adapter::canon::placeholder_integrity();
    b.integrity = libra_governor_evidence_adapter::canon::placeholder_integrity();
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap(),
        "actual_usage must have zero influence on the emitted event"
    );
}
