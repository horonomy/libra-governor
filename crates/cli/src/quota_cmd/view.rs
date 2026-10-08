//! CLI-owned output types for `libra-governor quota explain` (HORO-1767).
//!
//! Never a raw pass-through of a domain type whose shape could let an
//! honest caveat silently disappear — [`libra_governor_domain::BlockingStatus`]
//! and [`libra_governor_domain::Relief`] are re-expressed as
//! [`BlockingView`]/[`ReliefView`] below rather than serialized directly,
//! and every numeric figure is wrapped in [`Measured`] so a caller can
//! never mistake a non-authoritative estimate for an exact, verified
//! fact. See each type's own docs for which domain value it is derived
//! from and the specific honesty rule it enforces.

use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::pacing::{NextAdmit, PacingPreference, SimTask, UnavailableReason};
use libra_governor_domain::{
    BlockingStatus, GaugeFreshness, GaugeReading, GaugeState, Policy, QuotaAmount, QuotaSubject,
    QuotaUnit, Relief, ResourceAmount, StaleReason, WindowEvaluation, WindowState,
};

/// Renders an [`OffsetDateTime`] as RFC3339 UTC — the one timestamp shape
/// every figure in this output uses, so a reader (human or machine) never
/// has to handle more than one date format.
pub fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|_| format!("{t:?}"))
}

/// Every pacing-derived value in this output carries this tag (AC1) —
/// never Libra's own live enforcement authority, since none exists for
/// this surface (see `crates/cli/src/doctor_cmd.rs`'s `pacing_authority`
/// finding, added alongside this command).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    SimulatedAdvisory,
}

#[derive(Debug, Clone, Serialize)]
pub struct Simulated<T> {
    pub authority: Authority,
    pub value: T,
}

impl<T> Simulated<T> {
    pub fn new(value: T) -> Self {
        Self {
            authority: Authority::SimulatedAdvisory,
            value,
        }
    }
}

/// Which kind of figure a [`Figure`] carries (AC1: every one of these
/// must appear, correctly tagged, in the JSON output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FigureKind {
    QuotaSnapshot,
    Actual,
    Hold,
    Forecast,
}

/// Where a window's entitlement scope resolves to for this output —
/// deliberately has no `Task` variant: a window is never labeled `TASK`,
/// only a task row itself is (see [`TaskView::scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PoolScope {
    Host,
    Principal,
}

pub fn pool_scope(subject: &QuotaSubject) -> PoolScope {
    match subject {
        QuotaSubject::Principal(_) => PoolScope::Principal,
        QuotaSubject::SharedPool(_) | QuotaSubject::Provider(_) => PoolScope::Host,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReasonView {
    PastValidUntil,
    ExceedsMaxStaleness,
}

fn stale_reason_view(reason: StaleReason) -> StaleReasonView {
    match reason {
        StaleReason::PastValidUntil => StaleReasonView::PastValidUntil,
        StaleReason::ExceedsMaxStaleness => StaleReasonView::ExceedsMaxStaleness,
    }
}

/// A figure this output never overclaims about. See each variant's own
/// semantics:
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Measured<T> {
    /// A verified, exact fact — e.g. Libra's own recorded settled usage.
    Exact { value: T },
    /// A bound, never the real value itself — e.g. a period/bucket
    /// window's `remaining`, which can only ever overstate true
    /// headroom (Libra-observed usage is a lower bound on real usage).
    UpperBound { value: T, note: String },
    /// Genuinely not known — never collapsed to a zero or a default.
    Unknown { reason: String },
    /// A gauge reading exists but is too old (or past its own declared
    /// `valid_until`) to trust.
    Stale {
        reason: StaleReasonView,
        age_secs: Option<u64>,
    },
}

/// A figure tagged with its [`FigureKind`] and [`Authority`] — the unit
/// AC1's golden fixture asserts against.
#[derive(Debug, Clone, Serialize)]
pub struct Figure<T> {
    pub kind: FigureKind,
    pub authority: Authority,
    #[serde(flatten)]
    pub measured: Measured<T>,
}

impl<T> Figure<T> {
    fn new(kind: FigureKind, measured: Measured<T>) -> Self {
        Self {
            kind,
            authority: Authority::SimulatedAdvisory,
            measured,
        }
    }
}

/// A window's own blocking verdict, re-expressed so an
/// [`libra_governor_domain::IndeterminateReason`] is always carried as
/// readable text rather than a bare domain enum a reader has to already
/// know the meaning of.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BlockingView {
    Blocking,
    NotBlocking,
    Indeterminate { reason: String },
}

fn blocking_view(status: &BlockingStatus) -> BlockingView {
    match status {
        BlockingStatus::Blocking => BlockingView::Blocking,
        BlockingStatus::NotBlocking => BlockingView::NotBlocking,
        BlockingStatus::Indeterminate(reason) => BlockingView::Indeterminate {
            reason: format!("{reason:?}"),
        },
    }
}

/// When a blocking window stops blocking — re-expressed from
/// [`Relief`] so a provider's own claim ([`ReliefView::ProviderClaim`])
/// can never be mistaken for Libra's own computed guarantee
/// ([`ReliefView::Computed`]).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "relief", rename_all = "snake_case")]
pub enum ReliefView {
    NotBlocking,
    Computed { at: String },
    ProviderClaim { at: String },
    AfterHoldsSettle,
    Unknown,
}

fn relief_view(relief: &Relief) -> ReliefView {
    match relief {
        Relief::NotBlocking => ReliefView::NotBlocking,
        Relief::At(t) => ReliefView::Computed { at: rfc3339(*t) },
        Relief::ProviderDeclared(t) => ReliefView::ProviderClaim { at: rfc3339(*t) },
        Relief::AfterOutstandingHoldsSettle => ReliefView::AfterHoldsSettle,
        Relief::Unknown => ReliefView::Unknown,
    }
}

/// A provider-declared gauge reading, trusted only when fresh (see
/// [`GaugeState::trusted_reading`]) — a `Percent`-unit reading's
/// `used_value` is raw basis points (10_000 == 100%); rendering divides
/// by 100 only in the text renderer, never here.
#[derive(Debug, Clone, Serialize)]
pub struct GaugeFigure {
    pub used_value: u64,
    pub limit: Option<u64>,
    pub unit: String,
}

pub fn unit_label(unit: &QuotaUnit) -> String {
    match unit {
        QuotaUnit::UsdCents => "usd_cents".to_string(),
        QuotaUnit::Tokens => "tokens".to_string(),
        QuotaUnit::Requests => "requests".to_string(),
        QuotaUnit::Percent => "percent_bp".to_string(),
        QuotaUnit::OpaqueCredit(ns) => format!("opaque_credit:{}", ns.as_str()),
    }
}

fn snapshot_figure(gauge: &GaugeState) -> Measured<GaugeFigure> {
    match gauge.freshness {
        GaugeFreshness::NoSnapshot => Measured::Unknown {
            reason: "no provider snapshot has ever been ingested for this window".to_string(),
        },
        GaugeFreshness::Stale(reason) => Measured::Stale {
            reason: stale_reason_view(reason),
            age_secs: gauge.age_secs,
        },
        GaugeFreshness::Fresh => match gauge.trusted_reading() {
            Some(GaugeReading::Used { used, limit }) => Measured::Exact {
                value: GaugeFigure {
                    used_value: used.value,
                    limit: *limit,
                    unit: unit_label(&used.unit),
                },
            },
            Some(GaugeReading::Undisclosed) => Measured::Unknown {
                reason: "the provider declared this gauge's reading as undisclosed".to_string(),
            },
            None => Measured::Unknown {
                reason: "gauge reading unavailable".to_string(),
            },
        },
    }
}

/// One quota window's rendered evidence as of the explain command's
/// `--as-of` instant.
#[derive(Debug, Clone, Serialize)]
pub struct WindowView {
    pub window_id: String,
    pub scope: PoolScope,
    pub unit: String,
    pub blocking: BlockingView,
    pub snapshot: Figure<GaugeFigure>,
    pub actual: Figure<u64>,
    pub held: Figure<u64>,
    pub remaining: Figure<i64>,
    pub relief: ReliefView,
}

const LOWER_BOUND_NOTE: &str =
    "based on Libra-observed usage, which is a lower bound on real usage \
     (traffic that bypasses the gateway is never counted) — true remaining may be lower";

fn remaining_note(usage_records: usize) -> String {
    if usage_records == 0 {
        format!(
            "{LOWER_BOUND_NOTE}; no usage has been observed for this window — \
             this does not mean it is unused, only that none was observed"
        )
    } else {
        LOWER_BOUND_NOTE.to_string()
    }
}

pub fn window_view(
    eval: &WindowEvaluation,
    subject: &QuotaSubject,
    unit: &QuotaUnit,
) -> WindowView {
    let scope = pool_scope(subject);
    let unit_text = unit_label(unit);
    let blocking = blocking_view(&eval.blocking);

    match &eval.state {
        WindowState::Gauge(gauge) => WindowView {
            window_id: eval.window_id.0.to_string(),
            scope,
            unit: unit_text,
            blocking,
            snapshot: Figure::new(FigureKind::QuotaSnapshot, snapshot_figure(gauge)),
            actual: Figure::new(
                FigureKind::Actual,
                Measured::Unknown {
                    reason: "gauge windows have no settled-usage ledger — see snapshot instead"
                        .to_string(),
                },
            ),
            held: Figure::new(
                FigureKind::Hold,
                Measured::Unknown {
                    reason: "gauge windows have no outstanding-hold ledger — see snapshot instead"
                        .to_string(),
                },
            ),
            remaining: Figure::new(
                FigureKind::Forecast,
                Measured::Unknown {
                    reason: "gauge windows expose no numeric remaining".to_string(),
                },
            ),
            relief: relief_view(&gauge.relief),
        },
        WindowState::Period(period) => WindowView {
            window_id: eval.window_id.0.to_string(),
            scope,
            unit: unit_text,
            blocking,
            snapshot: Figure::new(
                FigureKind::QuotaSnapshot,
                Measured::Unknown {
                    reason: "not a provider gauge window".to_string(),
                },
            ),
            actual: Figure::new(
                FigureKind::Actual,
                Measured::Exact {
                    value: period.settled,
                },
            ),
            held: Figure::new(
                FigureKind::Hold,
                Measured::Exact {
                    value: period.outstanding,
                },
            ),
            remaining: Figure::new(
                FigureKind::Forecast,
                Measured::UpperBound {
                    value: period.remaining,
                    note: remaining_note(period.usage_records),
                },
            ),
            relief: relief_view(&period.relief),
        },
        WindowState::Bucket(bucket) => WindowView {
            window_id: eval.window_id.0.to_string(),
            scope,
            unit: unit_text,
            blocking,
            snapshot: Figure::new(
                FigureKind::QuotaSnapshot,
                Measured::Unknown {
                    reason: "not a provider gauge window".to_string(),
                },
            ),
            actual: Figure::new(
                FigureKind::Actual,
                Measured::Unknown {
                    reason: "refill-bucket windows track a continuously refilling level, \
                             not a cumulative settled total"
                        .to_string(),
                },
            ),
            held: Figure::new(
                FigureKind::Hold,
                Measured::Exact {
                    value: bucket.outstanding,
                },
            ),
            remaining: Figure::new(
                FigureKind::Forecast,
                Measured::UpperBound {
                    value: bucket.remaining,
                    note: remaining_note(bucket.usage_records),
                },
            ),
            relief: relief_view(&bucket.relief),
        },
    }
}

/// A policy's own declared hard ceiling — a definite, already-known
/// configuration value, not a measured/uncertain quantity, so it is
/// never wrapped in [`Measured`].
#[derive(Debug, Clone, Serialize)]
pub struct CeilingView {
    pub value: ResourceAmount,
    pub policy_name: String,
}

/// One task's rendered hold/actual/ceiling/need-range, always tagged
/// `scope: "TASK"` — never [`PoolScope`], which has no `Task` variant.
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub task: u32,
    pub principal: String,
    pub scope: &'static str,
    pub hold: Figure<u64>,
    pub actual: Figure<u64>,
    pub ceiling: CeilingView,
    pub need_range: Figure<(ResourceAmount, ResourceAmount)>,
}

pub fn task_view(
    task: &SimTask,
    hold: Option<&QuotaAmount>,
    actual_basis: Option<u64>,
    completed: bool,
    policy: &Policy,
) -> TaskView {
    let hold_figure = match hold {
        Some(h) => Figure::new(FigureKind::Hold, Measured::Exact { value: h.value }),
        None => Figure::new(
            FigureKind::Hold,
            Measured::Unknown {
                reason: "task has completed — it no longer holds anything".to_string(),
            },
        ),
    };

    let actual_figure = if completed {
        match actual_basis {
            Some(v) => Figure::new(
                FigureKind::Actual,
                Measured::UpperBound {
                    value: v,
                    note: "settled at max(reported spend, reservation floor) — never \
                           understates real spend, but may overstate it when no Spend \
                           at or above the floor was ever reported"
                        .to_string(),
                },
            ),
            None => Figure::new(
                FigureKind::Actual,
                Measured::Unknown {
                    reason: "completed, but no settled usage record was found".to_string(),
                },
            ),
        }
    } else {
        match actual_basis {
            Some(v) => Figure::new(FigureKind::Actual, Measured::Exact { value: v }),
            None => Figure::new(
                FigureKind::Actual,
                Measured::Unknown {
                    reason: "no spend has been reported for this task".to_string(),
                },
            ),
        }
    };

    let need_range = match &task.estimate.resource {
        libra_governor_domain::RemainingResource::Quantiles { kind, p50, p90, .. } => Figure::new(
            FigureKind::Forecast,
            Measured::Exact {
                value: (
                    resource_amount_of(*kind, *p50),
                    resource_amount_of(*kind, *p90),
                ),
            },
        ),
        _ => Figure::new(
            FigureKind::Forecast,
            Measured::Unknown {
                reason: "task's remaining-work estimate is insufficient or unavailable".to_string(),
            },
        ),
    };

    TaskView {
        task: task.id.0,
        principal: task.principal.0.clone(),
        scope: "TASK",
        hold: hold_figure,
        actual: actual_figure,
        ceiling: CeilingView {
            value: policy.resource.hard_ceiling,
            policy_name: policy.name.clone(),
        },
        need_range,
    }
}

fn resource_amount_of(
    kind: libra_governor_domain::ResourceKind,
    amount: ResourceAmount,
) -> ResourceAmount {
    debug_assert_eq!(
        amount.kind(),
        kind,
        "quantile amount must already match its own kind"
    );
    amount
}

/// This explain run's resolved pacing mode, as of `--as-of` — carries
/// only what the caller actually configured, never a live mode a daemon
/// might be in (there is no live mode here — see `ModeView`'s own
/// `Authority` wrapper at the top level).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModeView {
    Sustain {
        horizon_secs: u64,
        continuity_reserve_bp: u16,
    },
    Burst {
        target_end: String,
        max_fanout: u16,
    },
}

pub fn mode_view(preference: &PacingPreference) -> ModeView {
    match preference {
        PacingPreference::Sustain {
            horizon_secs,
            continuity_reserve_bp,
            ..
        } => ModeView::Sustain {
            horizon_secs: *horizon_secs,
            continuity_reserve_bp: *continuity_reserve_bp,
        },
        PacingPreference::Burst {
            target_end,
            max_fanout,
        } => ModeView::Burst {
            target_end: rfc3339(*target_end),
            max_fanout: *max_fanout,
        },
    }
}

/// The scenario-level next safe action (never task-scoped — see module
/// docs on `Proposal::Hold` carrying no task id).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NextView {
    Now,
    At { at: String },
    Unknown { reason: String },
}

/// [`next_safe_action`] plus the window (if any) actually binding it —
/// computed together so the two can never silently drift apart (e.g.
/// reporting an exact time while leaving `binding_window` stale from a
/// different, unrelated proposal).
pub struct NextResolution {
    pub next: NextView,
    pub binding_window: Option<String>,
}

/// Classifies the scenario's pending admit instant honestly:
/// - no pending timer at all => [`NextView::Now`] (nothing left to wait for).
/// - a pending instant strictly after `as_of`, with every window
///   determinate at `as_of` and no gauge window present => [`NextView::At`].
/// - anything else (a provider-only claim, an indeterminate window, a
///   gauge window in the scenario, or a probe/tick-budget refusal) =>
///   [`NextView::Unknown`] with the specific reason — never guessed as a
///   computed time.
///
/// Never re-runs [`libra_governor_domain::pacing::step::step`] — it only
/// reads the final `pending_timer` the replay already computed plus the
/// last processed tick's own `Proposal::Hold` entries, matching by
/// instant.
pub fn resolve_next(
    pending_timer: Option<OffsetDateTime>,
    last_tick_proposals: &[libra_governor_domain::pacing::Proposal],
    as_of: OffsetDateTime,
    any_gauge_window: bool,
    any_indeterminate_window: bool,
    tick_budget_exhausted: bool,
) -> NextResolution {
    let unknown = |reason: String| NextResolution {
        next: NextView::Unknown { reason },
        binding_window: None,
    };

    if tick_budget_exhausted {
        return unknown(
            "the replay's timer-tick budget was exhausted before every pending instant \
             up to --as-of could be resolved"
                .to_string(),
        );
    }
    // A gauge window's own evaluation never participates in
    // `earliest_safe_admit` with any snapshot evidence (`step` always
    // passes `snapshots: &[]` into the probe — see `forecast.rs`'s own
    // module docs on the Gauge exception) — a scenario that includes one
    // can never honestly claim an exact scenario-level forecast, Now or
    // otherwise, since the one gauge-derived constraint the real
    // decision would depend on was never actually consulted.
    if any_gauge_window {
        return unknown("simulator does not consume provider snapshots".to_string());
    }
    let Some(pending) = pending_timer else {
        return NextResolution {
            next: NextView::Now,
            binding_window: None,
        };
    };
    if pending <= as_of {
        // `simulate_until` never processes past `as_of`, so a pending
        // timer at or before it means this pass did not fully resolve —
        // honest refusal rather than reporting a stale instant as if it
        // were still the real next action.
        return unknown("the simulator's own pending timer is at or before --as-of".to_string());
    }

    let matching = last_tick_proposals.iter().find_map(|p| match p {
        libra_governor_domain::pacing::Proposal::Hold { next } => match next {
            NextAdmit::At { at, .. } | NextAdmit::Paced { at, .. } if *at == pending => Some(*next),
            _ => None,
        },
        _ => None,
    });

    match matching {
        Some(NextAdmit::At { limiting, .. }) => {
            if any_indeterminate_window {
                unknown("at least one window is indeterminate as of --as-of".to_string())
            } else {
                NextResolution {
                    next: NextView::At {
                        at: rfc3339(pending),
                    },
                    binding_window: Some(limiting.0.to_string()),
                }
            }
        }
        Some(NextAdmit::Paced { .. }) => {
            if any_indeterminate_window {
                unknown("at least one window is indeterminate as of --as-of".to_string())
            } else {
                NextResolution {
                    next: NextView::At {
                        at: rfc3339(pending),
                    },
                    binding_window: None,
                }
            }
        }
        Some(NextAdmit::Unavailable(UnavailableReason::ProviderDeclaredReliefOnly)) => unknown(
            "relief here is only a provider's own claim, never Libra's computed guarantee"
                .to_string(),
        ),
        Some(NextAdmit::Unavailable(reason)) => unknown(format!("{reason:?}")),
        Some(NextAdmit::Now) | None => {
            unknown("could not resolve which proposal produced the pending timer".to_string())
        }
    }
}

/// Overrides a Period/Bucket window's `actual`/`held`/`remaining` to
/// [`Measured::Unknown`] when more than one principal appears in the
/// scenario — `PacerState::usage_log` is a single flat log, never split
/// by principal (ADR-0016 §5), so a Principal-scoped window's usage
/// cannot honestly be attributed to just that principal once a second
/// principal's tasks also share the replay. `snapshot`/`blocking`/
/// `relief` are untouched — those are independent of principal
/// attribution (a gauge reading, or a window's own blocking verdict from
/// evidence that is still genuinely valid even though it can't be
/// decomposed per principal).
pub const UNATTRIBUTED_PRINCIPAL_REASON: &str = "usage not principal-attributed (ADR-0016 §5)";

pub fn redact_for_unattributed_principal(mut wv: WindowView) -> WindowView {
    wv.actual = Figure::new(
        FigureKind::Actual,
        Measured::Unknown {
            reason: UNATTRIBUTED_PRINCIPAL_REASON.to_string(),
        },
    );
    wv.held = Figure::new(
        FigureKind::Hold,
        Measured::Unknown {
            reason: UNATTRIBUTED_PRINCIPAL_REASON.to_string(),
        },
    );
    wv.remaining = Figure::new(
        FigureKind::Forecast,
        Measured::Unknown {
            reason: UNATTRIBUTED_PRINCIPAL_REASON.to_string(),
        },
    );
    wv
}

/// The full rendered explanation (AC1-AC5).
#[derive(Debug, Clone, Serialize)]
pub struct Explanation {
    pub schema_version: &'static str,
    pub as_of: String,
    pub mode: Simulated<ModeView>,
    pub binding_window: Simulated<Option<String>>,
    pub next_safe_action: Simulated<NextView>,
    pub active_tasks: usize,
    pub configured_cap: u16,
    pub host: Vec<WindowView>,
    pub principal: Vec<WindowView>,
    pub tasks: Vec<TaskView>,
}

pub const EXPLANATION_SCHEMA_VERSION: &str = "quota-explain-v1";
