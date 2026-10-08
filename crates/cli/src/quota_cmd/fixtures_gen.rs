//! Generates `quota explain --replay` fixture files under
//! `crates/cli/tests/fixtures/quota/` by constructing real domain values
//! and serializing them — never hand-written JSON. Several wire fields
//! (`PacingEvent::at`, `SimTask::deadline`, `PacingPreference::Burst::target_end`)
//! use different serde timestamp representations (some have no
//! `#[serde(with = ...)]` at all, so they use `time`'s own default
//! format), so hand-writing this envelope would silently drift from
//! what the real types actually produce.
//!
//! Not a real test: `#[ignore]`d, run explicitly via
//! `cargo test -p libra-governor-cli --lib generate_quota_explain_fixtures -- --ignored`
//! whenever a fixture needs to be regenerated. Its output is committed.

use std::io::Write;
use std::path::PathBuf;

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use libra_governor_domain::pacing::{PacingEvent, PacingPreference, SimTask, SimTaskId};
use libra_governor_domain::{
    AutonomyBoundary, BucketTier, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, EntitlementSource, Feasibility, GaugeReading, NoSpendBasis, Policy, PoolId,
    PrincipalId, ProgressEvidence, ProviderSnapshot, QuotaScope, QuotaSubject, QuotaUnit,
    QuotaWindow, QuotaWindowId, RegimeBasis, RemainingDuration, RemainingResource,
    RemainingWorkEstimate, ResourceAmount, ResourceBound, SpendSoFar, TimeBound, TruthStrength,
    WindowKind,
};

fn fixed_uuid(tag: u8) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0] = 0xF1;
    bytes[15] = tag;
    Uuid::from_bytes(bytes)
}

fn estimate(tokens: u64, duration_secs: u64, confidence: Confidence) -> RemainingWorkEstimate {
    RemainingWorkEstimate {
        schema_version: "remaining-work-v1".to_string(),
        estimator_version: "fixture".to_string(),
        duration: RemainingDuration::Quantiles {
            p50_secs: duration_secs,
            p80_secs: duration_secs,
            p90_secs: duration_secs,
            conditional_n: 20,
        },
        resource: RemainingResource::Quantiles {
            kind: libra_governor_domain::ResourceKind::Tokens,
            p50: ResourceAmount::Tokens(tokens),
            p80: ResourceAmount::Tokens(tokens),
            p90: ResourceAmount::Tokens(tokens),
            conditional_n: 20,
            weakest_truth: TruthStrength::Metered,
        },
        feasibility: Feasibility::Insufficient {
            conditional_n: 0,
            required: 5,
        },
        confidence,
        regime: RegimeBasis::default(),
        bucket_tier: BucketTier::Global,
        evidence: ProgressEvidence {
            elapsed_secs: 0,
            spend_so_far: SpendSoFar::NoBasis {
                reason: NoSpendBasis::NoAccount,
            },
            tool_calls_total: 0,
            tool_calls_since_last_replan: 0,
            same_tool_streak: 0,
            plan_revision: 0,
            auto_replan_count: 0,
            active_lease_count: 0,
            child_account_count: 0,
            gateway_request_count: 0,
            observed_at: T0,
        },
    }
}

fn quality_floor() -> CompletionContract {
    CompletionContract::first(vec![CompletionCriterion::required("fixture criterion")])
}

fn policy(hard_ceiling_tokens: u64) -> Policy {
    Policy::validated(
        "fixture-policy",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(hard_ceiling_tokens / 3),
            elastic_ceiling: Some(ResourceAmount::Tokens(hard_ceiling_tokens * 2 / 3)),
            hard_ceiling: ResourceAmount::Tokens(hard_ceiling_tokens),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3_600,
            elastic_ceiling_secs: Some(7_200),
            hard_ceiling_secs: Some(10_800),
            deadline: None,
        },
        quality_floor(),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .expect("fixture policy must be valid")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/quota")
}

fn write_fixture(name: &str, value: serde_json::Value) {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create fixture file");
    let pretty = serde_json::to_string_pretty(&value).expect("serialize fixture");
    file.write_all(pretty.as_bytes()).expect("write fixture");
    file.write_all(b"\n").expect("write trailing newline");
}

const T0: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

/// AC2 fixture: a single idle task under one principal-scoped sliding
/// window, Burst mode, no spend/activity — exercises the "ceiling
/// equals the policy's hard ceiling", "no literal 100, no percent" and
/// "period window renders UpperBound" assertions. Also reused for the
/// width/no-ANSI/determinism/AC4 no-side-effect checks, none of which
/// need a second fixture.
#[test]
#[ignore]
fn generate_idle_task_fixture() {
    let window_id = QuotaWindowId(fixed_uuid(1));
    let window = QuotaWindow::validated(
        window_id,
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("solo".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: T0,
            valid_until: None,
        },
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 21_600,
            limit: 4_242,
        },
    )
    .unwrap();

    let task = SimTask {
        id: SimTaskId(7),
        principal: PrincipalId("solo".to_string()),
        depends_on: vec![],
        priority: libra_governor_domain::Priority::Normal,
        deadline: None,
        estimate: estimate(321, 600, Confidence::High),
    };

    let events = vec![PacingEvent::ModeChanged { at: T0, seq: 0 }];
    let preference = PacingPreference::Burst {
        target_end: T0 + time::Duration::hours(6),
        max_fanout: 9,
    };

    let value = json!({
        "schema_version": "quota-explain-input-v1",
        "policy": policy(327_000),
        "preference": preference,
        "contract": null,
        "tasks": [task],
        "windows": [window],
        "snapshots": [],
        "events": events,
    });
    write_fixture("idle_task.json", value);
}

/// AC5b fixture: two tasks under two distinct principals, sharing one
/// Principal-scoped sliding window — the window must render `Unknown`
/// (usage is not principal-attributed) once a second principal's tasks
/// also exist in the scenario.
#[test]
#[ignore]
fn generate_two_principal_fixture() {
    let window_id = QuotaWindowId(fixed_uuid(2));
    let window = QuotaWindow::validated(
        window_id,
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("alice".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: T0,
            valid_until: None,
        },
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 21_600,
            limit: 9_009,
        },
    )
    .unwrap();

    let task_a = SimTask {
        id: SimTaskId(1),
        principal: PrincipalId("alice".to_string()),
        depends_on: vec![],
        priority: libra_governor_domain::Priority::Normal,
        deadline: None,
        estimate: estimate(222, 300, Confidence::High),
    };
    let task_b = SimTask {
        id: SimTaskId(2),
        principal: PrincipalId("bob".to_string()),
        depends_on: vec![],
        priority: libra_governor_domain::Priority::Normal,
        deadline: None,
        estimate: estimate(333, 300, Confidence::High),
    };

    let events = vec![
        PacingEvent::ModeChanged { at: T0, seq: 0 },
        PacingEvent::Spend {
            at: T0 + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: libra_governor_domain::QuotaAmount::new(QuotaUnit::Tokens, 111),
        },
    ];
    let preference = PacingPreference::Burst {
        target_end: T0 + time::Duration::hours(6),
        max_fanout: 9,
    };

    let value = json!({
        "schema_version": "quota-explain-input-v1",
        "policy": policy(606_000),
        "preference": preference,
        "contract": null,
        "tasks": [task_a, task_b],
        "windows": [window],
        "snapshots": [],
        "events": events,
    });
    write_fixture("two_principal.json", value);
}

/// AC1/AC3 fixture: one Period (sliding, principal-scoped) window plus
/// one Gauge (opaque provider snapshot, host/shared-pool-scoped) window
/// with a *stale* snapshot reading — exercises all four `FigureKind`s
/// (the gauge window's own snapshot is `QUOTA_SNAPSHOT`; the period
/// window's `actual`/`held` and the task's `need_range` cover
/// `ACTUAL`/`HOLD`/`FORECAST`), the `STALE` state, and the
/// "simulator does not consume provider snapshots" forecast-Unknown rule.
#[test]
#[ignore]
fn generate_gauge_fixture() {
    let period_id = QuotaWindowId(fixed_uuid(3));
    let period_window = QuotaWindow::validated(
        period_id,
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("carol".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: T0,
            valid_until: None,
        },
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 21_600,
            limit: 7_777,
        },
    )
    .unwrap();

    let gauge_id = QuotaWindowId(fixed_uuid(4));
    let gauge_window = QuotaWindow::validated(
        gauge_id,
        QuotaScope {
            subject: QuotaSubject::SharedPool(PoolId("shared".to_string())),
            source: EntitlementSource::ProviderDeclared,
            confidence: Confidence::High,
            observed_at: T0,
            valid_until: None,
        },
        QuotaUnit::Percent,
        WindowKind::OpaqueProviderSnapshot {
            max_staleness_secs: 60,
        },
    )
    .unwrap();

    // Observed long before `--as-of` (fixtures/tests always pass an
    // explicit `--as-of` far enough past `observed_at` to be stale).
    let snapshot = ProviderSnapshot::validated(
        gauge_id,
        T0,
        None,
        None,
        GaugeReading::Used {
            used: libra_governor_domain::QuotaAmount::new(QuotaUnit::Percent, 5_000),
            limit: None,
        },
        Confidence::High,
    )
    .unwrap();

    let task = SimTask {
        id: SimTaskId(1),
        principal: PrincipalId("carol".to_string()),
        depends_on: vec![],
        priority: libra_governor_domain::Priority::Normal,
        deadline: None,
        estimate: estimate(654, 300, Confidence::High),
    };

    let events = vec![PacingEvent::ModeChanged { at: T0, seq: 0 }];
    let preference = PacingPreference::Burst {
        target_end: T0 + time::Duration::hours(6),
        max_fanout: 5,
    };

    let value = json!({
        "schema_version": "quota-explain-input-v1",
        "policy": policy(808_000),
        "preference": preference,
        "contract": null,
        "tasks": [task],
        "windows": [period_window, gauge_window],
        "snapshots": [snapshot],
        "events": events,
    });
    write_fixture("gauge.json", value);
}
