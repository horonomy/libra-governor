//! The local monthly shared budget (HORO-2195, ADR-0026 D1/D3): admission,
//! check, post-hoc charge, status and tool-event heartbeat over one
//! `metered:local-budget/monthly-YYYY-MM` pool per zone-local calendar
//! month.
//!
//! # Capacity source
//!
//! The configured monthly cap is the effective capacity, passed to every
//! ledger call as `CapacitySource::LocalConfigured`. Nothing here reads a
//! provider snapshot or quota observation, and nothing calls the snapshot
//! ingest path -- that boundary (HORO-1915, ADR-0023) is unchanged and is
//! guarded by `tests/metered_pool_no_production_feed.rs`. Soft limits are
//! warn-only and never mint capacity.
//!
//! # Pool creation
//!
//! A period's pool is created lazily by the first `Admit` (or the first
//! charge that lands in a period with no pool yet). [`ensure_period_pool`]
//! holds the single production pool-creation call in the daemon outside the
//! launch gate, authorised by founder decision D1 / ADR-0026.
//!
//! # Rollover
//!
//! A hold belongs to the period it was admitted in, and a late settle lands
//! in the hold's own row whatever the clock says. A charge goes to the
//! period of its event time, falling back to the session's latest-hold pool.
//! An `Admit` reuses a lane's active hold only when that hold is in the
//! current period's pool: reusing last month's hold would admit this
//! month's turn without charging this month's cap.
//!
//! # Safe failure (ADR-0026)
//!
//! - No file: nothing is consulted or written ([`SharedBudgetOutcome::NotConfigured`]).
//! - File present but invalid: `Admit` and `Check` deny `config_invalid`.
//! - A pool of a later period than the clock's exists: deny `clock_anomaly`.
//!
//! # Identity
//!
//! The executor key is `(session_id, agent_id | none)`. A missing agent is
//! the session's main executor (`agent_absent`), never a root agent. Empty
//! session ids and the reserved `execution-owner-v1:` prefix are refused.
//! This path does not require a pre-established session; it never invents
//! one.
use std::sync::{Arc, Mutex};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use libra_governor_domain::{
    calendar_month_bounds, monthly_pool_alias, ChargeSource, ExecutorKind, MeteredPoolKey, PoolId,
    PrincipalId, QuotaUnit, SharedPoolReservation, LOCAL_BUDGET_OWNER,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerError, LedgerStore, MeteredReserveOutcome,
    MeteredReserveRequest, SharedPoolChargeOutcome, SharedPoolChargeRequest,
    SharedPoolExecutorReport, SharedPoolRenewOutcome, SharedPoolReserveRequest,
};
use libra_governor_protocol::{
    SharedBudgetCheckState, SharedBudgetConfigDrift, SharedBudgetConfigState,
    SharedBudgetConfigView, SharedBudgetDenyReason, SharedBudgetExecutorRow, SharedBudgetOutcome,
    SharedBudgetPeriod, SharedBudgetRefusal, SharedBudgetStatus, SoftLimitScope, SoftLimitWarning,
    SHARED_BUDGET_BASIS, SHARED_BUDGET_GUARANTEE, SHARED_BUDGET_SCOPE,
};

use crate::shared_budget_file::{
    ConfigDrift, LoadedSharedBudget, SharedBudgetConfig, SharedBudgetFileState, SoftLimits,
};

/// How long a turn hold lives without a heartbeat. Each tool event renews
/// it, so only an abandoned turn (a crash, a kill) ever waits this out.
pub const HOLD_TTL_SECS: u64 = 3600;

/// Every local-budget pool id starts with this.
const POOL_PREFIX: &str = "metered:local-budget/monthly-";
/// Holds of this owner, across periods, share this prefix.
const OWNER_POOL_PREFIX: &str = "metered:local-budget/";
const RESERVED_SESSION_PREFIX: &str = "execution-owner-v1:";

/// A source of "now". Injectable so rollover and timezone-boundary tests
/// are deterministic without sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock a test moves by hand.
pub struct ManualClock(Mutex<OffsetDateTime>);

impl ManualClock {
    pub fn new(now: OffsetDateTime) -> Self {
        Self(Mutex::new(now))
    }

    pub fn set(&self, now: OffsetDateTime) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = now;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the file said when last consulted, and whether that differs from
/// what the daemon read at start.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub state: SharedBudgetFileState,
    pub changed_since_start: bool,
}

struct Live {
    state: SharedBudgetFileState,
    hash: Option<String>,
    /// `(mtime ns, size)` at the last read; `None` forces a read.
    key: Option<Option<(i128, u64)>>,
}

/// A file modified this recently is re-read even when its stat looks
/// unchanged, because coarse filesystem timestamps can hide a same-size
/// rewrite made within one tick.
const RECENT_MODIFICATION_SECS: u64 = 2;

/// The shared-budget configuration a daemon runs with, plus its clock.
///
/// The file is consulted on every request (a stat; a re-read only when it
/// changed), so a file created, edited, broken or deleted after start takes
/// effect from the next decision, failing closed: an unreadable, partial or
/// invalid file denies `config_invalid`, and a deleted file means the
/// operator removed the budget (`not_configured`).
pub struct SharedBudgetRuntime {
    loaded: LoadedSharedBudget,
    live: Mutex<Live>,
    clock: Arc<dyn Clock>,
}

impl Default for SharedBudgetRuntime {
    fn default() -> Self {
        Self::absent()
    }
}

impl SharedBudgetRuntime {
    /// No shared-budget file: the budget path is never consulted.
    pub fn absent() -> Self {
        Self::new(LoadedSharedBudget::absent())
    }

    pub fn new(loaded: LoadedSharedBudget) -> Self {
        let live = Live {
            state: loaded.state.clone(),
            hash: loaded.fingerprint.as_ref().map(|f| f.sha256_hex.clone()),
            key: None,
        };
        Self {
            loaded,
            live: Mutex::new(live),
            clock: Arc::new(SystemClock),
        }
    }

    /// Reads `<state_dir>/shared-budget.json`.
    pub fn load(state_dir: &std::path::Path) -> Self {
        Self::new(LoadedSharedBudget::load(state_dir))
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The configuration as read at daemon start.
    pub fn loaded(&self) -> &LoadedSharedBudget {
        &self.loaded
    }

    pub fn now(&self) -> OffsetDateTime {
        self.clock.now()
    }

    /// The configuration in force right now, re-reading the file if it
    /// changed since the last look.
    pub fn effective(&self) -> EffectiveConfig {
        let start_hash = self.loaded.fingerprint.as_ref().map(|f| &f.sha256_hex);
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = &self.loaded.path {
            let meta = std::fs::metadata(path).ok();
            let key = meta.as_ref().map(|m| {
                use std::os::unix::fs::MetadataExt;
                (
                    i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                    m.len(),
                )
            });
            let recent = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok().or(Some(std::time::Duration::ZERO)))
                .is_some_and(|age| age.as_secs() < RECENT_MODIFICATION_SECS);
            if live.key != Some(key) || recent {
                let (state, fingerprint) = crate::shared_budget_file::read_current(path);
                live.state = state;
                live.hash = fingerprint.map(|f| f.sha256_hex);
                live.key = Some(key);
            }
        }
        EffectiveConfig {
            changed_since_start: live.state != self.loaded.state
                || live.hash.as_ref() != start_hash,
            state: live.state.clone(),
        }
    }

    /// Whether a shared-budget file is in force (valid or not).
    pub fn is_configured(&self) -> bool {
        !matches!(self.effective().state, SharedBudgetFileState::Absent)
    }
}

/// A failure the handler cannot express as an [`SharedBudgetOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum SharedBudgetError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("internal invariant violated: {0}")]
    Internal(&'static str),
}

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .expect("instants derived from the clock or the ledger always format as RFC 3339")
}

fn principal() -> PrincipalId {
    PrincipalId(LOCAL_BUDGET_OWNER.to_string())
}

/// Length-prefixed component, so concatenated key parts are unambiguous.
fn lp(s: &str) -> String {
    format!("{}:{s}", s.len())
}

fn source(cfg: &SharedBudgetConfig) -> CapacitySource {
    CapacitySource::LocalConfigured {
        cap: cfg.monthly_cap_usd_cents,
    }
}

struct Period {
    start: OffsetDateTime,
    end: OffsetDateTime,
    pool_id: PoolId,
}

fn period_at(cfg: &SharedBudgetConfig, at: OffsetDateTime) -> Result<Period, SharedBudgetError> {
    let (start, end) = calendar_month_bounds(at, &cfg.time_zone);
    let alias = monthly_pool_alias(start, &cfg.time_zone);
    let key = MeteredPoolKey::new(&principal(), &alias)
        .map_err(|_| SharedBudgetError::Internal("monthly pool alias is not a valid slug"))?;
    Ok(Period {
        start,
        end,
        pool_id: key.pool_id(),
    })
}

fn denied(
    reason: SharedBudgetDenyReason,
    remaining: Option<i64>,
    period_end: Option<OffsetDateTime>,
    detail: Option<String>,
) -> SharedBudgetOutcome {
    SharedBudgetOutcome::Denied {
        reason,
        remaining_usd_cents: remaining,
        period_end: period_end.map(rfc),
        detail,
    }
}

fn refused(reason: SharedBudgetRefusal) -> SharedBudgetOutcome {
    SharedBudgetOutcome::Refused { reason }
}

/// Either the usable configuration or the outcome that ends the request.
fn configured(state: &SharedBudgetFileState) -> Result<SharedBudgetConfig, SharedBudgetOutcome> {
    match state {
        SharedBudgetFileState::Absent => Err(SharedBudgetOutcome::NotConfigured),
        SharedBudgetFileState::Invalid(reason) => Err(denied(
            SharedBudgetDenyReason::ConfigInvalid,
            None,
            None,
            Some(reason.clone()),
        )),
        SharedBudgetFileState::Valid(cfg) => Ok(cfg.clone()),
    }
}

fn validate_identity(
    session_id: &str,
    agent_id: Option<&str>,
) -> Result<ExecutorKind, SharedBudgetRefusal> {
    if session_id.trim().is_empty() {
        return Err(SharedBudgetRefusal::EmptySessionId);
    }
    if session_id.trim_start().starts_with(RESERVED_SESSION_PREFIX) {
        return Err(SharedBudgetRefusal::ReservedSessionScope);
    }
    match agent_id {
        Some(a) if a.trim().is_empty() => Err(SharedBudgetRefusal::InvalidAgentId),
        Some(_) => Ok(ExecutorKind::Agent),
        None => Ok(ExecutorKind::AgentAbsent),
    }
}

/// The latest existing local-budget pool, when it is of a later period
/// than `current`.
fn later_pool(ledger: &LedgerStore, current: &Period) -> Result<Option<PoolId>, SharedBudgetError> {
    Ok(ledger
        .shared_pool_ids_with_prefix(POOL_PREFIX)?
        .into_iter()
        .rfind(|p| p.0 > current.pool_id.0))
}

fn anomaly(later: &PoolId, current: &Period) -> SharedBudgetOutcome {
    denied(
        SharedBudgetDenyReason::ClockAnomaly,
        None,
        Some(current.end),
        Some(format!(
            "pool {} is of a later period than the current {}",
            later.0, current.pool_id.0
        )),
    )
}

/// The only place a daemon-side local-budget pool is created. Idempotent;
/// the capacity recorded at first creation is informational only, because
/// every ledger call here passes the configured cap as the effective one.
fn ensure_period_pool(
    ledger: &mut LedgerStore,
    pool_id: &PoolId,
    cfg: &SharedBudgetConfig,
    now: OffsetDateTime,
) -> Result<(), SharedBudgetError> {
    ledger.ensure_pool(
        pool_id,
        &QuotaUnit::UsdCents,
        cfg.monthly_cap_usd_cents,
        now,
    )?;
    Ok(())
}

fn attribution_for(agent_id: Option<&str>, charge_source: ChargeSource) -> ExecutorAttribution<'_> {
    match agent_id {
        Some(a) => ExecutorAttribution::agent(a, Some(charge_source)),
        None => ExecutorAttribution::main_executor(Some(charge_source)),
    }
}

fn remaining_of(
    ledger: &LedgerStore,
    pool_id: &PoolId,
    cfg: &SharedBudgetConfig,
    now: OffsetDateTime,
) -> Result<i64, SharedBudgetError> {
    Ok(
        match ledger.pool_admission_for(pool_id, now, source(cfg))? {
            Some(a) => a.remaining(),
            // No pool yet: nothing has been spent or held.
            None => i64::try_from(cfg.monthly_cap_usd_cents).unwrap_or(i64::MAX),
        },
    )
}

/// Soft-limit warnings from an executor report. `focus` limits them to one
/// session and lane (an `Admit` reports only about its own executor);
/// `None` reports every executor (`Status`).
fn soft_warnings(
    report: &SharedPoolExecutorReport,
    soft: &SoftLimits,
    focus: Option<(&str, Option<&str>, ExecutorKind)>,
) -> Vec<SoftLimitWarning> {
    let mut out = Vec::new();
    if let Some(threshold) = soft.per_session_usd_cents {
        let mut sessions: Vec<&str> = report
            .executors
            .iter()
            .map(|e| e.session_id.as_str())
            .collect();
        sessions.sort_unstable();
        sessions.dedup();
        for session in sessions {
            if focus.is_some_and(|(s, _, _)| s != session) {
                continue;
            }
            let total: u64 = report
                .executors
                .iter()
                .filter(|e| e.session_id == session)
                .map(|e| e.used.saturating_add(e.reserved))
                .fold(0, u64::saturating_add);
            if total >= threshold {
                out.push(SoftLimitWarning {
                    scope: SoftLimitScope::PerSession,
                    session_id: session.to_string(),
                    agent_id: None,
                    used_plus_reserved_usd_cents: total,
                    threshold_usd_cents: threshold,
                });
            }
        }
    }
    if let Some(threshold) = soft.per_agent_usd_cents {
        for e in &report.executors {
            if let Some((session, agent, kind)) = focus {
                if e.session_id != session
                    || e.agent_id.as_deref() != agent
                    || e.executor_kind != kind
                {
                    continue;
                }
            }
            let total = e.used.saturating_add(e.reserved);
            if total >= threshold {
                out.push(SoftLimitWarning {
                    scope: SoftLimitScope::PerAgent,
                    session_id: e.session_id.clone(),
                    agent_id: e.agent_id.clone(),
                    used_plus_reserved_usd_cents: total,
                    threshold_usd_cents: threshold,
                });
            }
        }
    }
    out
}

fn allowed(
    ledger: &LedgerStore,
    cfg: &SharedBudgetConfig,
    period: &Period,
    hold: &SharedPoolReservation,
    reused: bool,
    now: OffsetDateTime,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let executor_kind = hold.executor_kind.ok_or(SharedBudgetError::Internal(
        "a granted hold has no executor kind",
    ))?;
    let warnings = if cfg.soft_limits == SoftLimits::default() {
        Vec::new()
    } else {
        let report = ledger.shared_pool_executor_usage(&hold.pool_id)?;
        soft_warnings(
            &report,
            &cfg.soft_limits,
            Some((&hold.session_id, hold.agent_id.as_deref(), executor_kind)),
        )
    };
    Ok(SharedBudgetOutcome::Allowed {
        reservation_id: hold.id,
        pool_id: hold.pool_id.0.clone(),
        hold_usd_cents: hold.amount,
        hold_reused: reused,
        executor_kind,
        remaining_usd_cents: remaining_of(ledger, &hold.pool_id, cfg, now)?,
        period_end: rfc(period.end),
        warnings,
    })
}

/// Handles `SharedBudgetRequest::Admit`.
pub fn admit(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    agent_id: Option<&str>,
    prompt_id: Option<&str>,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(SharedBudgetOutcome::NotConfigured);
    }
    let kind = match validate_identity(session_id, agent_id) {
        Ok(kind) => kind,
        Err(reason) => return Ok(refused(reason)),
    };
    let cfg = match configured(&state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(outcome),
    };
    let cfg = &cfg;
    let now = rt.now();
    let period = period_at(cfg, now)?;
    if let Some(later) = later_pool(ledger, &period)? {
        return Ok(anomaly(&later, &period));
    }
    ensure_period_pool(ledger, &period.pool_id, cfg, now)?;
    ledger.expire_stale_shared_pool_reservations(now)?;

    // One active hold per lane: a re-prompt (Stop does not fire on
    // interrupt) renews the existing hold instead of reserving again.
    if let Some(existing) = ledger.active_shared_hold_for_lane(
        &period.pool_id,
        &principal(),
        session_id,
        agent_id,
        kind,
        now,
    )? {
        if let SharedPoolRenewOutcome::Renewed(renewed) =
            ledger.renew_shared(existing.id, &principal(), session_id, now, HOLD_TTL_SECS)?
        {
            return allowed(ledger, cfg, &period, &renewed, true, now);
        }
    }

    let agent_part = agent_id.map_or_else(|| "-".to_string(), lp);
    let prompt_key =
        |tag: &str, id: &str| format!("hold:{}|{agent_part}|{tag}{}", lp(session_id), lp(id));
    let mut key = match prompt_id.filter(|p| !p.trim().is_empty()) {
        Some(p) => prompt_key("p", p),
        None => prompt_key("n", &Uuid::new_v4().to_string()),
    };
    // At most one retry: the first key may name a hold that already
    // finished (the prompt was re-sent after its turn ended), which is a
    // new turn and needs a new hold.
    for attempt in 0..2 {
        let outcome = ledger.reserve_shared_metered(MeteredReserveRequest {
            base: SharedPoolReserveRequest {
                pool_id: &period.pool_id,
                principal_id: &principal(),
                session_id,
                amount: cfg.turn_hold_usd_cents,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: &key,
                now,
                ttl_secs: HOLD_TTL_SECS,
                attribution: attribution_for(agent_id, ChargeSource::TurnHold),
            },
            session_cap: None,
            capacity_source: source(cfg),
        });
        let outcome = match outcome {
            Ok(o) => o,
            Err(LedgerError::QuotaUnitMismatch { .. }) => {
                return Ok(refused(SharedBudgetRefusal::PoolUnitMismatch))
            }
            Err(LedgerError::SharedPoolKeyConflict) => {
                return Ok(refused(SharedBudgetRefusal::KeyConflict))
            }
            Err(e) => return Err(e.into()),
        };
        match outcome {
            MeteredReserveOutcome::Granted(hold) => {
                return allowed(ledger, cfg, &period, &hold, false, now);
            }
            MeteredReserveOutcome::AlreadyGranted(hold) => {
                let live = hold.state == libra_governor_domain::ReservationState::Active
                    && hold.expires_at > now
                    && hold.session_id == session_id
                    && hold.agent_id.as_deref() == agent_id;
                if live {
                    return allowed(ledger, cfg, &period, &hold, true, now);
                }
                if attempt == 0 {
                    key = prompt_key("n", &Uuid::new_v4().to_string());
                    continue;
                }
                return Err(SharedBudgetError::Internal(
                    "a fresh idempotency key named an existing hold",
                ));
            }
            MeteredReserveOutcome::Insufficient { admission, .. } => {
                return Ok(denied(
                    SharedBudgetDenyReason::CapExhausted,
                    Some(admission.remaining()),
                    Some(period.end),
                    None,
                ))
            }
            MeteredReserveOutcome::NoPool => {
                return Err(SharedBudgetError::Internal(
                    "pool missing right after creation",
                ))
            }
            MeteredReserveOutcome::SnapshotUnusable(_)
            | MeteredReserveOutcome::SessionCapExceeded { .. } => {
                return Err(SharedBudgetError::Internal(
                    "a locally configured cap consulted a provider snapshot or session cap",
                ))
            }
        }
    }
    Err(SharedBudgetError::Internal("admit retry loop exhausted"))
}

/// Handles `SharedBudgetRequest::Check`. A read: it never creates a pool,
/// reserves, or sweeps.
pub fn check(
    ledger: &LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    agent_id: Option<&str>,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(SharedBudgetOutcome::NotConfigured);
    }
    let kind = match validate_identity(session_id, agent_id) {
        Ok(kind) => kind,
        Err(reason) => return Ok(refused(reason)),
    };
    let cfg = match configured(&state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(outcome),
    };
    let cfg = &cfg;
    let now = rt.now();
    let period = period_at(cfg, now)?;
    if let Some(later) = later_pool(ledger, &period)? {
        return Ok(anomaly(&later, &period));
    }
    let remaining = remaining_of(ledger, &period.pool_id, cfg, now)?;
    let lane_hold = ledger.active_shared_hold_for_lane(
        &period.pool_id,
        &principal(),
        session_id,
        agent_id,
        kind,
        now,
    )?;
    let state = if lane_hold.is_some() {
        SharedBudgetCheckState::Admitted
    } else if remaining < i64::try_from(cfg.turn_hold_usd_cents).unwrap_or(i64::MAX) {
        SharedBudgetCheckState::Exhausted
    } else {
        SharedBudgetCheckState::Available
    };
    Ok(SharedBudgetOutcome::Checked {
        state,
        remaining_usd_cents: remaining,
        period_end: rfc(period.end),
    })
}

/// Years outside this window are refused as event times: the calendar
/// arithmetic is only specified for real clocks.
const EVENT_YEARS: std::ops::RangeInclusive<i32> = 2000..=2200;

/// Handles `SharedBudgetRequest::ChargeAgent`.
#[allow(clippy::too_many_arguments)]
pub fn charge(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    agent_id: Option<&str>,
    amount_usd_cents: u64,
    idempotency_key: &str,
    charge_source: ChargeSource,
    event_time: Option<&str>,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(SharedBudgetOutcome::NotConfigured);
    }
    if let Err(reason) = validate_identity(session_id, agent_id) {
        return Ok(refused(reason));
    }
    if amount_usd_cents == 0 || i64::try_from(amount_usd_cents).is_err() {
        return Ok(refused(SharedBudgetRefusal::InvalidAmount));
    }
    if idempotency_key.trim().is_empty() {
        return Ok(refused(SharedBudgetRefusal::InvalidIdempotencyKey));
    }
    let event = match event_time {
        None => None,
        Some(raw) => match OffsetDateTime::parse(raw, &Rfc3339) {
            Ok(t) if EVENT_YEARS.contains(&t.year()) => Some(t),
            _ => return Ok(refused(SharedBudgetRefusal::InvalidEventTime)),
        },
    };
    let cfg = match configured(&state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(outcome),
    };
    let cfg = &cfg;
    let now = rt.now();
    let current = period_at(cfg, now)?;

    let pool_id = match event {
        Some(t) => {
            let target = period_at(cfg, t)?;
            if target.pool_id.0 > current.pool_id.0 {
                return Ok(denied(
                    SharedBudgetDenyReason::ClockAnomaly,
                    None,
                    Some(current.end),
                    Some("event time is in a later period than now".to_string()),
                ));
            }
            target.pool_id
        }
        None => ledger
            .latest_shared_hold_pool_for_session(&principal(), session_id, OWNER_POOL_PREFIX)?
            .unwrap_or_else(|| current.pool_id.clone()),
    };
    ensure_period_pool(ledger, &pool_id, cfg, now)?;

    // An unknown agent on a subagent charge is explicitly UNKNOWN, not the
    // main executor; any other source without an agent is the main one.
    let attribution = match (agent_id, charge_source) {
        (None, ChargeSource::SubagentTranscript) => {
            ExecutorAttribution::unattributed(Some(charge_source))
        }
        _ => attribution_for(agent_id, charge_source),
    };
    let key = format!("charge:{}|{}", lp(session_id), lp(idempotency_key));
    let outcome = ledger.charge_shared(SharedPoolChargeRequest {
        pool_id: &pool_id,
        principal_id: &principal(),
        session_id,
        amount: amount_usd_cents,
        unit: &QuotaUnit::UsdCents,
        idempotency_key: &key,
        now,
        attribution,
        capacity_source: source(cfg),
    });
    let outcome = match outcome {
        Ok(o) => o,
        Err(LedgerError::QuotaUnitMismatch { .. }) => {
            return Ok(refused(SharedBudgetRefusal::PoolUnitMismatch))
        }
        Err(e) => return Err(e.into()),
    };
    let executor_kind_of = |r: &SharedPoolReservation| {
        r.executor_kind.ok_or(SharedBudgetError::Internal(
            "a charge row has no executor kind",
        ))
    };
    Ok(match outcome {
        SharedPoolChargeOutcome::Charged {
            reservation,
            admission,
            overrun,
            capacity_known,
        } => SharedBudgetOutcome::Charged {
            reservation_id: reservation.id,
            pool_id: pool_id.0,
            amount_usd_cents,
            already_charged: false,
            executor_kind: executor_kind_of(&reservation)?,
            remaining_usd_cents: admission.remaining(),
            overrun_usd_cents: overrun,
            capacity_known,
        },
        SharedPoolChargeOutcome::AlreadyCharged(reservation) => {
            let admission = ledger.pool_admission_for(&pool_id, now, source(cfg))?;
            SharedBudgetOutcome::Charged {
                reservation_id: reservation.id,
                pool_id: pool_id.0,
                amount_usd_cents,
                already_charged: true,
                executor_kind: executor_kind_of(&reservation)?,
                remaining_usd_cents: admission.map_or(0, |a| a.remaining()),
                overrun_usd_cents: admission.map_or(0, |a| a.overrun()),
                capacity_known: admission.is_some_and(|a| a.capacity_known),
            }
        }
        SharedPoolChargeOutcome::KeyConflict(_) => refused(SharedBudgetRefusal::KeyConflict),
        SharedPoolChargeOutcome::NoPool => {
            return Err(SharedBudgetError::Internal(
                "pool missing right after creation",
            ))
        }
    })
}

/// Handles `SharedBudgetRequest::Status`.
pub fn status(
    ledger: &LedgerStore,
    rt: &SharedBudgetRuntime,
) -> Result<SharedBudgetStatus, SharedBudgetError> {
    let loaded = rt.loaded();
    let effective = rt.effective();
    let (config_state, config_invalid_reason) = match &effective.state {
        SharedBudgetFileState::Absent => (SharedBudgetConfigState::Absent, None),
        SharedBudgetFileState::Valid(_) => (SharedBudgetConfigState::Valid, None),
        SharedBudgetFileState::Invalid(reason) => {
            (SharedBudgetConfigState::Invalid, Some(reason.clone()))
        }
    };
    let config_drift = match loaded.drift() {
        ConfigDrift::None => SharedBudgetConfigDrift::None,
        ConfigDrift::Changed => SharedBudgetConfigDrift::Changed,
        ConfigDrift::Appeared => SharedBudgetConfigDrift::Appeared,
        ConfigDrift::Removed => SharedBudgetConfigDrift::Removed,
        ConfigDrift::Unreadable => SharedBudgetConfigDrift::Unreadable,
    };
    let mut report = SharedBudgetStatus {
        basis: SHARED_BUDGET_BASIS.to_string(),
        guarantee: SHARED_BUDGET_GUARANTEE.to_string(),
        scope: SHARED_BUDGET_SCOPE.to_string(),
        config_state,
        config_invalid_reason,
        config_drift,
        config_changed_since_start: effective.changed_since_start,
        config_path: loaded.path.as_ref().map(|p| p.display().to_string()),
        config_permissions_note: loaded.permissions_note(),
        config: None,
        clock_anomaly: false,
        period: None,
        pool_exists: false,
        capacity_known: false,
        used_usd_cents: 0,
        reserved_usd_cents: 0,
        remaining_usd_cents: None,
        executors: Vec::new(),
        warnings: Vec::new(),
    };
    let SharedBudgetFileState::Valid(cfg) = &effective.state else {
        return Ok(report);
    };
    report.config = Some(SharedBudgetConfigView {
        monthly_cap_usd_cents: cfg.monthly_cap_usd_cents,
        time_zone: cfg.time_zone.as_str().to_string(),
        turn_hold_usd_cents: cfg.turn_hold_usd_cents,
        soft_limit_per_session_usd_cents: cfg.soft_limits.per_session_usd_cents,
        soft_limit_per_agent_usd_cents: cfg.soft_limits.per_agent_usd_cents,
        subagent_unmeasured_charge_usd_cents: cfg.subagent_unmeasured_charge_usd_cents,
    });
    let now = rt.now();
    let period = period_at(cfg, now)?;
    report.clock_anomaly = later_pool(ledger, &period)?.is_some();
    report.period = Some(SharedBudgetPeriod {
        pool_id: period.pool_id.0.clone(),
        start: rfc(period.start),
        end: rfc(period.end),
    });
    // Capacity is the configured cap whether or not the pool exists yet.
    report.capacity_known = true;
    report.remaining_usd_cents = Some(i64::try_from(cfg.monthly_cap_usd_cents).unwrap_or(i64::MAX));
    if let Some(admission) = ledger.pool_admission_for(&period.pool_id, now, source(cfg))? {
        report.pool_exists = true;
        report.capacity_known = admission.capacity_known;
        report.used_usd_cents = admission.settled;
        report.reserved_usd_cents = admission.active;
        report.remaining_usd_cents = Some(admission.remaining());
        let executors = ledger.shared_pool_executor_usage(&period.pool_id)?;
        report.warnings = soft_warnings(&executors, &cfg.soft_limits, None);
        report.executors = executors
            .executors
            .into_iter()
            .map(|e| SharedBudgetExecutorRow {
                session_id: e.session_id,
                agent_id: e.agent_id,
                executor_kind: e.executor_kind,
                used_usd_cents: e.used,
                reserved_usd_cents: e.reserved,
            })
            .collect();
    }
    Ok(report)
}

/// Tool-event heartbeat: renews every active, unexpired local-budget hold
/// of `session_id`, whichever lane it belongs to (a tool event carries no
/// agent id). Returns how many were renewed. Does nothing -- not even a
/// ledger read -- unless a valid shared-budget file is loaded.
pub fn heartbeat(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
) -> Result<usize, SharedBudgetError> {
    if !matches!(rt.effective().state, SharedBudgetFileState::Valid(_))
        || validate_identity(session_id, None).is_err()
    {
        return Ok(0);
    }
    let now = rt.now();
    let mut renewed = 0;
    for hold in
        ledger.active_shared_holds_for_session(&principal(), session_id, OWNER_POOL_PREFIX, now)?
    {
        if matches!(
            ledger.renew_shared(hold.id, &principal(), session_id, now, HOLD_TTL_SECS)?,
            SharedPoolRenewOutcome::Renewed(_)
        ) {
            renewed += 1;
        }
    }
    Ok(renewed)
}
