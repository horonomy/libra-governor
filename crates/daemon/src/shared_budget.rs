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
use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use libra_governor_domain::{
    calendar_month_bounds, monthly_pool_alias, ChargeSource, ExecutorKind, MeteredPoolKey, PoolId,
    PrincipalId, QuotaUnit, ReservationState, SharedPoolReservation, LOCAL_BUDGET_OWNER,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerError, LedgerStore, MeteredReserveOutcome,
    MeteredReserveRequest, SettleWatermark, SharedPoolChargeOutcome, SharedPoolChargeRequest,
    SharedPoolExecutorReport, SharedPoolRenewOutcome, SharedPoolReserveRequest,
    SharedPoolSettleOutcome,
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
use crate::usage::priced::{
    observe_priced, PricedObservation, ReadMode, BUDGET_SCAN_CAP_BYTES, FUTURE_SKEW,
};
use crate::usage::UsageUnavailable;

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
    telemetry: Mutex<Telemetry>,
    /// `(size, mtime)` of each subagent file as of its last sweep, keyed
    /// `(session, agent)`. Process memory only: a restart re-checks every
    /// file once, which the delta logic makes harmless.
    sweep_seen: Mutex<std::collections::HashMap<(String, String), (u64, i128)>>,
}

/// At most this many unpriced model ids are remembered for `Status`.
const MAX_UNPRICED_MODELS: usize = 32;
/// Longest model id kept for display.
const MAX_MODEL_DISPLAY_LEN: usize = 96;

/// Settlement telemetry (HORO-2196). Process memory only: it counts since
/// the daemon started. The durable record is the ledger (`usage_known`).
#[derive(Default)]
struct Telemetry {
    unpriced_models: BTreeSet<String>,
    binding_mismatches: u64,
    unverified: u64,
    unmeasured_settlements: u64,
    unmeasured_charges: u64,
    id_less_records: u64,
    foreign_lines: u64,
    sweep_deferred_files: u64,
    future_timestamps: u64,
}

/// A model id from a transcript, reduced to a safe, bounded display token:
/// it came from a file any same-user process can write.
fn display_model(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._:-/<>".contains(*c))
        .take(MAX_MODEL_DISPLAY_LEN)
        .collect();
    if cleaned.is_empty() {
        "<unprintable>".to_string()
    } else {
        cleaned
    }
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
            telemetry: Mutex::new(Telemetry::default()),
            sweep_seen: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn note(&self, f: impl FnOnce(&mut Telemetry)) {
        f(&mut self.telemetry.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn note_unpriced(&self, models: &[String]) {
        self.note(|t| {
            for m in models {
                if t.unpriced_models.len() < MAX_UNPRICED_MODELS {
                    t.unpriced_models.insert(display_model(m));
                }
            }
        });
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

/// The idempotency key of a hold. `tag` is `p` for a hold admitted for a
/// host-named prompt and `n` for a nonce. [`settle_turn`] rebuilds the `p`
/// key to recognise a replayed `Stop`, so both sides must use this.
fn hold_key(session_id: &str, agent_id: Option<&str>, tag: &str, id: &str) -> String {
    let agent_part = agent_id.map_or_else(|| "-".to_string(), lp);
    format!("hold:{}|{agent_part}|{tag}{}", lp(session_id), lp(id))
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
            if e.executor_kind != ExecutorKind::Agent || e.agent_id.is_none() {
                continue;
            }
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
    ledger: &mut LedgerStore,
    cfg: &SharedBudgetConfig,
    period: &Period,
    hold: &SharedPoolReservation,
    reused: bool,
    now: OffsetDateTime,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let executor_kind = hold.executor_kind.ok_or(SharedBudgetError::Internal(
        "a granted hold has no executor kind",
    ))?;
    // The hold is already granted. Nothing after this point may strand it:
    // warnings are best-effort, and if even the remaining figure cannot be
    // read, a hold this call just created is released before the error.
    let remaining = match remaining_of(ledger, &hold.pool_id, cfg, now) {
        Ok(remaining) => remaining,
        Err(e) => {
            if !reused {
                let _ = ledger.release_shared(hold.id, now);
            }
            return Err(e);
        }
    };
    let (warnings, warnings_unavailable) = if cfg.soft_limits == SoftLimits::default() {
        (Vec::new(), false)
    } else {
        match ledger.shared_pool_executor_usage(&hold.pool_id) {
            Ok(report) => (
                soft_warnings(
                    &report,
                    &cfg.soft_limits,
                    Some((&hold.session_id, hold.agent_id.as_deref(), executor_kind)),
                ),
                false,
            ),
            Err(_) => (Vec::new(), true),
        }
    };
    Ok(SharedBudgetOutcome::Allowed {
        reservation_id: hold.id,
        pool_id: hold.pool_id.0.clone(),
        hold_usd_cents: hold.amount,
        hold_reused: reused,
        executor_kind,
        remaining_usd_cents: remaining,
        period_end: rfc(period.end),
        warnings,
        warnings_unavailable,
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
        // The prompt joins the hold's accepted set in the renewal's own
        // transaction (HORO-2250). A renewal drops the idempotency key, so
        // without this a replayed Stop for the renewed prompt is unknown.
        // Main lane only: a subagent's Admit may carry the parent's prompt id
        // and must not take over its mapping.
        let renewing = prompt_id
            .filter(|p| !p.trim().is_empty())
            .filter(|_| agent_id.is_none());
        if let SharedPoolRenewOutcome::Renewed(renewed) = ledger.renew_shared_for_prompt(
            existing.id,
            &principal(),
            session_id,
            now,
            HOLD_TTL_SECS,
            renewing,
        )? {
            return allowed(ledger, cfg, &period, &renewed, true, now);
        }
    }

    let prompt_key = |tag: &str, id: &str| hold_key(session_id, agent_id, tag, id);
    let mut key = match prompt_id.filter(|p| !p.trim().is_empty()) {
        Some(p) => prompt_key("p", p),
        None => prompt_key("n", &Uuid::new_v4().to_string()),
    };
    // At most one retry: the first key may name a hold that already
    // finished (the prompt was re-sent after its turn ended), which is a
    // new turn and needs a new hold.
    let main_prompt = prompt_id
        .filter(|p| !p.trim().is_empty())
        .filter(|_| agent_id.is_none());
    for attempt in 0..2 {
        let outcome = ledger.reserve_shared_metered_for_prompt(
            MeteredReserveRequest {
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
            },
            main_prompt,
        );
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
    // Same precedence as before the split: identity, amount, key.
    if let Err(reason) = validate_identity(session_id, agent_id) {
        return Ok(refused(reason));
    }
    if amount_usd_cents == 0 || i64::try_from(amount_usd_cents).is_err() {
        return Ok(refused(SharedBudgetRefusal::InvalidAmount));
    }
    if idempotency_key.trim().is_empty() {
        return Ok(refused(SharedBudgetRefusal::InvalidIdempotencyKey));
    }
    let key = format!("charge:{}|{}", lp(session_id), lp(idempotency_key));
    book_charge(
        ledger,
        rt,
        &state,
        BookedCharge {
            session_id,
            agent_id,
            amount_usd_cents,
            key: &key,
            charge_source,
            event_time,
            usage_known: true,
            watermark: None,
        },
    )
}

/// One post-hoc charge, ready to book.
struct BookedCharge<'a> {
    session_id: &'a str,
    agent_id: Option<&'a str>,
    amount_usd_cents: u64,
    /// The complete ledger idempotency key.
    key: &'a str,
    charge_source: ChargeSource,
    event_time: Option<&'a str>,
    /// False for a conservative placeholder (an unmeasured subagent).
    usage_known: bool,
    /// Written in the same ledger transaction as the row (a tail charge).
    watermark: Option<&'a SettleWatermark>,
}

/// Validates and books `charge` into the right period's pool. The single
/// write path for `ChargeAgent` and `ChargeAgentTranscript`.
fn book_charge(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    state: &SharedBudgetFileState,
    charge: BookedCharge<'_>,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let BookedCharge {
        session_id,
        agent_id,
        amount_usd_cents,
        key,
        charge_source,
        event_time,
        usage_known,
        watermark,
    } = charge;
    if let Err(reason) = validate_identity(session_id, agent_id) {
        return Ok(refused(reason));
    }
    if amount_usd_cents == 0 || i64::try_from(amount_usd_cents).is_err() {
        return Ok(refused(SharedBudgetRefusal::InvalidAmount));
    }
    let event = match event_time {
        None => None,
        Some(raw) => match OffsetDateTime::parse(raw, &Rfc3339) {
            Ok(t) if EVENT_YEARS.contains(&t.year()) => Some(t),
            _ => return Ok(refused(SharedBudgetRefusal::InvalidEventTime)),
        },
    };
    let cfg = match configured(state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(outcome),
    };
    let cfg = &cfg;
    // One charge can never exceed ten months of budget.
    if amount_usd_cents > cfg.monthly_cap_usd_cents.saturating_mul(10) {
        return Ok(refused(SharedBudgetRefusal::AmountTooLarge));
    }
    let now = rt.now();
    let current = period_at(cfg, now)?;

    // A retry resolves to the pool the first attempt landed in, wherever
    // the fallback would point today: the ledger's replay lookup is
    // scoped to one pool, so without this a retry after a month change
    // would book the same spend twice.
    let existing = ledger.find_shared_reservation_by_key(&principal(), key, OWNER_POOL_PREFIX)?;
    let mut clamped = false;
    let pool_id = match (&existing, event) {
        (Some(found), _) => found.pool_id.clone(),
        (None, Some(t)) => {
            let target = period_at(cfg, t)?;
            if target.pool_id.0 > current.pool_id.0 {
                // Spend that already happened is never refused for a
                // plausible timestamp: book it now and say so.
                clamped = true;
                current.pool_id.clone()
            } else {
                // `prune` deletes whole ended pools and with them the
                // idempotency memory of the charges booked there, so a
                // charge for an event older than every retained pool cannot
                // be told from a replay of one already booked (HORO-2250):
                // a re-swept subagent file after a restart, say. Refuse it
                // rather than recreate the period and book it a second time.
                // A prune records the newest pool it removed (even when no
                // pool remains). Only a period at or before that is refused;
                // a period the budget simply never booked into is not.
                let pruned_through = ledger.shared_retention_horizon(OWNER_POOL_PREFIX)?;
                if pruned_through.is_some_and(|h| target.pool_id.0 <= h.0) {
                    return Ok(refused(SharedBudgetRefusal::EventBeforeRetention));
                }
                target.pool_id
            }
        }
        (None, None) => ledger
            .latest_shared_hold_pool_for_session(&principal(), session_id, OWNER_POOL_PREFIX)?
            .unwrap_or_else(|| current.pool_id.clone()),
    };
    ensure_period_pool(ledger, &pool_id, cfg, now)?;
    if existing.is_none() {
        // Keep every pool total far inside i64 so no later sum can overflow.
        let committed = ledger
            .pool_admission_for(&pool_id, now, source(cfg))?
            .map_or(0, |a| a.settled.saturating_add(a.active));
        if committed.saturating_add(amount_usd_cents) > (i64::MAX / 2) as u64 {
            return Ok(refused(SharedBudgetRefusal::AmountTooLarge));
        }
    }

    // An unknown agent on a subagent charge is explicitly UNKNOWN, not the
    // main executor; any other source without an agent is the main one.
    let attribution = match (agent_id, charge_source) {
        (None, ChargeSource::SubagentTranscript) => {
            ExecutorAttribution::unattributed(Some(charge_source))
        }
        _ => attribution_for(agent_id, charge_source),
    };
    let outcome = ledger.charge_shared_with_watermark(
        SharedPoolChargeRequest {
            pool_id: &pool_id,
            principal_id: &principal(),
            session_id,
            amount: amount_usd_cents,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: key,
            now,
            attribution,
            capacity_source: source(cfg),
            usage_known,
        },
        watermark,
    );
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
            event_time_clamped: clamped,
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
                event_time_clamped: false,
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

/// An opaque, stable display id for a session in log lines: the first eight
/// hex digits of its SHA-256. Log lines never carry the session or agent id
/// themselves, a path or a transcript excerpt.
pub fn opaque_id(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(session_id.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A structured log line for a flagged subagent transcript charge
/// (mismatch, unverified, unpriced or unmeasured), or `None` when the
/// charge was clean. Status counters are volatile; this line and the
/// ledger's `usage_known` flag are the durable record.
pub fn transcript_charge_log(session_id: &str, outcome: &SharedBudgetOutcome) -> Option<String> {
    let SharedBudgetOutcome::TranscriptCharged {
        amount_charged_usd_cents,
        cumulative_usd_cents,
        unmeasured,
        unpriced,
        transcript_unverified,
        binding_mismatch,
        ..
    } = outcome
    else {
        return None;
    };
    if !(*unmeasured || *unpriced || *transcript_unverified || *binding_mismatch) {
        return None;
    }
    Some(format!(
        "shared budget event=subagent_charge lane={} unmeasured={unmeasured} unpriced={unpriced} \
         unverified={transcript_unverified} binding_mismatch={binding_mismatch} \
         charged={amount_charged_usd_cents}c cumulative={cumulative_usd_cents}c",
        opaque_id(session_id)
    ))
}

/// The structured log line for a `SettleTail` outcome.
pub fn tail_log(session_id: &str, outcome: &SharedBudgetOutcome) -> String {
    match outcome {
        SharedBudgetOutcome::TailSettled {
            amount_charged_usd_cents,
            already_charged,
            unmeasured,
            unpriced,
            transcript_unverified,
            binding_mismatch,
            ..
        } => format!(
            "shared budget event=settle_tail lane={} charged={amount_charged_usd_cents}c \
             already={already_charged} unmeasured={unmeasured} unpriced={unpriced} \
             unverified={transcript_unverified} binding_mismatch={binding_mismatch}",
            opaque_id(session_id)
        ),
        _ => format!(
            "shared budget event=settle_tail lane={} not_applied",
            opaque_id(session_id)
        ),
    }
}

/// What settling a `Stop` did (HORO-2196). Content-free: it is logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSettlement {
    /// No shared-budget file: nothing was consulted or written.
    NotConfigured,
    /// The session id is empty or reserved; there is no lane to settle.
    NotApplicable,
    /// The lane has no unsettled hold (the turn was never admitted, or it
    /// was already settled -- a retried `Stop` lands here).
    NoHold,
    Settled {
        reservation_id: libra_governor_domain::SharedPoolReservationId,
        pool_id: String,
        hold_usd_cents: u64,
        settled_usd_cents: u64,
        /// False when the hold amount was settled because the window could
        /// not be measured.
        usage_known: bool,
        /// How far the settled amount exceeds the hold (never clamped).
        overrun_usd_cents: u64,
        /// Why the window was not measured; `None` when it was.
        unmeasured_reason: Option<&'static str>,
    },
    /// The hold was already settled by another call (lost a race).
    AlreadySettled,
}

impl TurnSettlement {
    /// A short, content-free label for the daemon log.
    pub fn label(&self) -> String {
        match self {
            Self::NotConfigured => "shared budget not configured".to_string(),
            Self::NotApplicable => "shared budget: session not applicable".to_string(),
            Self::NoHold => "shared budget: no unsettled hold".to_string(),
            Self::AlreadySettled => "shared budget: hold already settled".to_string(),
            Self::Settled {
                hold_usd_cents,
                settled_usd_cents,
                usage_known,
                overrun_usd_cents,
                unmeasured_reason,
                ..
            } => format!(
                "shared budget: settled {settled_usd_cents}c of {hold_usd_cents}c hold \
                 (usage_known={usage_known}, overrun={overrun_usd_cents}c, reason={})",
                unmeasured_reason.unwrap_or("measured")
            ),
        }
    }
}

/// Most `message.id`s a watermark remembers. A turn is a few dozen
/// messages; this keeps tail-block protection across several windows while
/// bounding the row.
const MAX_WATERMARK_IDS: usize = libra_governor_ledger::WATERMARK_ID_CAP;

/// Where the next window of a main-executor lane starts: the persisted
/// transcript high-water mark (newest record timestamp counted, plus the
/// message ids counted up to it), else -- legacy rows and lanes whose
/// windows never counted a record -- the settle time of the latest settled
/// hold, else the oldest hold's creation.
fn lane_window(
    ledger: &LedgerStore,
    session_id: &str,
    holds: &[SharedPoolReservation],
) -> Result<Option<(OffsetDateTime, Vec<String>)>, SharedBudgetError> {
    let mark = ledger.shared_lane_watermark(
        &principal(),
        session_id,
        None,
        ExecutorKind::AgentAbsent,
        OWNER_POOL_PREFIX,
    )?;
    // Ids are remembered even when no timestamp mark exists.
    let ids = mark
        .as_ref()
        .map(|m| m.message_ids.clone())
        .unwrap_or_default();
    if let Some(at) = mark.as_ref().and_then(|m| m.at) {
        return Ok(Some((at, ids)));
    }
    Ok(holds
        .iter()
        .filter(|h| h.state == ReservationState::Settled)
        .filter_map(|h| h.settled_at)
        .max()
        .or_else(|| holds.iter().map(|h| h.created_at).min())
        .map(|t| (t, ids)))
}

fn merged_ids(previous: &[String], counted: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(previous.len() + counted.len());
    let mut seen = HashSet::new();
    for id in previous.iter().chain(counted) {
        if seen.insert(id.as_str()) {
            out.push(id.clone());
        }
    }
    if out.len() > MAX_WATERMARK_IDS {
        out.drain(..out.len() - MAX_WATERMARK_IDS);
    }
    out
}

/// The hold a `Stop` settles: the lane's most recently created hold that is
/// still `active` (expired-but-unswept included) or `expired`.
fn pending_hold(holds: &[SharedPoolReservation]) -> Option<&SharedPoolReservation> {
    holds
        .iter()
        .filter(|h| {
            matches!(
                h.state,
                ReservationState::Active | ReservationState::Expired
            )
        })
        .max_by_key(|h| h.created_at)
}

/// Settles the session's main-executor turn hold at `Stop` (HORO-2196).
///
/// Called once per `Finalize`, before and independently of the legacy task
/// finalization: the shared budget needs no preflight task. With no file in
/// force it does nothing at all.
///
/// # Which hold, which window
///
/// The target is the lane's most recently created hold still `active`
/// (expired-but-unswept included) or `expired`; a late settle after expiry
/// is recorded in the hold's own row, in the hold's own period.
///
/// The window is every transcript record **strictly after** the lane's
/// watermark: the latest `settled_at` of the lane's settled holds, or, when
/// none has settled yet, the creation time of the lane's oldest hold. The
/// watermark is derived from existing ledger rows (no migration), and it
/// advances only when a hold actually settles, so:
///
/// - two consecutive Stops never count a record twice;
/// - a crashed turn (no Stop) is measured by the *next* Stop: its hold
///   expired unsettled, so the watermark did not move past its records.
///   The abandoned hold itself is reclaimed by TTL (`Admit`'s sweep).
///
/// A record the host flushes to the file *after* the Stop that should have
/// covered it, but stamped before that Stop, falls behind the watermark and
/// is not counted (Claude Code writes its transcript asynchronously). The
/// window it was missing from settled either measured-short or at the hold
/// amount; this is the known limit of a timestamp watermark.
///
/// # Outcomes
///
/// - measured and bound to the session: settle at the priced USD cents,
///   `usage_known=true`. The amount may exceed the hold; the overrun is
///   recorded and later `Admit`s are refused while `remaining < hold`.
/// - the transcript names another session: charge nothing, leave the hold
///   (it expires by TTL), count the mismatch.
/// - anything unmeasurable (no path, missing/rotated/truncated file, over
///   the cap, no usage in the window, session field absent, unpriced
///   model): settle at the **hold amount** with `usage_known=false`.
/// - the file is present but invalid: settle at the hold amount (overrides
///   from an untrusted file are not used).
/// - no hold: nothing.
/// - `prompt_id` names a prompt whose hold already settled (a replayed
///   `Stop`): nothing, even when a newer hold is pending. Without a
///   `prompt_id` a duplicate cannot be told from a real `Stop` whose
///   transcript has not flushed, and settles the pending hold.
pub fn settle_turn(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    transcript_path: Option<&str>,
    prompt_id: Option<&str>,
) -> Result<TurnSettlement, SharedBudgetError> {
    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(TurnSettlement::NotConfigured);
    }
    if validate_identity(session_id, None).is_err() {
        return Ok(TurnSettlement::NotApplicable);
    }
    let holds = ledger.shared_lane_holds(
        &principal(),
        session_id,
        None,
        ExecutorKind::AgentAbsent,
        OWNER_POOL_PREFIX,
    )?;
    // A `Stop` for a prompt whose own hold already settled is a replay (a
    // duplicate or late delivery). The lane's pending hold now belongs to a
    // LATER turn; settling it from this stale window would book a turn that
    // has not happened (HORO-2250).
    if let Some(p) = prompt_id.filter(|p| !p.trim().is_empty()) {
        // Any prompt a hold accepted (first or renewed, kept after a prune)
        // names its hold; a Stop for a hold that is not the pending one is a
        // replay. A prompt with no mapping falls back to the first-prompt
        // key (holds admitted before the mapping existed).
        let key = hold_key(session_id, None, "p", p);
        let replay = match ledger.hold_for_prompt(&principal(), session_id, p)? {
            Some(accepted) => pending_hold(&holds).is_none_or(|t| t.id != accepted),
            // No mapping (a hold admitted before migration 0024): the first
            // prompt's key is all there is.
            None => holds
                .iter()
                .any(|h| h.state == ReservationState::Settled && h.idempotency_key == key),
        };
        if replay {
            return Ok(TurnSettlement::AlreadySettled);
        }
    }
    let Some(target) = pending_hold(&holds) else {
        return Ok(TurnSettlement::NoHold);
    };
    let (after, previous_ids) =
        lane_window(ledger, session_id, &holds)?.unwrap_or((target.created_at, Vec::new()));
    let counted_ids: HashSet<String> = previous_ids.iter().cloned().collect();

    let now = rt.now();
    let (actual, reason, watermark): (Option<u64>, Option<&'static str>, Option<SettleWatermark>) =
        match &state {
            SharedBudgetFileState::Valid(cfg) => {
                let observation = match transcript_path {
                    None => PricedObservation::Unavailable(UsageUnavailable::NoTranscriptPath),
                    Some(path) => observe_priced(
                        std::path::Path::new(path),
                        ReadMode::ParentWindow {
                            after,
                            counted_ids: &counted_ids,
                            horizon: now + FUTURE_SKEW,
                        },
                        session_id,
                        &cfg.pricing_overrides.table(),
                        BUDGET_SCAN_CAP_BYTES,
                    ),
                };
                match observation {
                    PricedObservation::Priced(window) => {
                        rt.note(|t| {
                            t.id_less_records += window.id_less_records;
                            t.foreign_lines += window.foreign_lines;
                        });
                        (
                            Some(window.cents),
                            None,
                            Some(SettleWatermark {
                                // In bound by construction; the year check
                                // keeps a pathological stamp out of storage.
                                at: window
                                    .last_event_time
                                    .filter(|t| EVENT_YEARS.contains(&t.year())),
                                message_ids: merged_ids(&previous_ids, &window.message_ids),
                            }),
                        )
                    }
                    // Every usage line named another session: nothing here is
                    // this lane's spend, and none of it is imported. The turn
                    // is not free either, so the hold amount is booked,
                    // flagged. The watermark does not move: it only ever
                    // advances to the lane's OWN counted records.
                    PricedObservation::SessionMismatch => {
                        rt.note(|t| t.binding_mismatches += 1);
                        (None, Some("binding_mismatch"), None)
                    }
                    // A record stamped beyond now + skew cannot be trusted
                    // and must never become the watermark.
                    // The in-bound own records are still measured spend:
                    // the window books max(priced, hold). Above the hold the
                    // priced amount is recorded as measured; at or below it
                    // the hold stands, unmeasured.
                    PricedObservation::FutureTimestamp {
                        priced_cents,
                        newest_in_bound,
                        message_ids,
                    } => {
                        rt.note(|t| t.future_timestamps += 1);
                        (
                            priced_cents.filter(|c| *c > target.amount),
                            Some("future_timestamp"),
                            // The mark advances only when the in-bound records
                            // were priced, and only to the newest of them. The
                            // future-stamped lines stay after the mark and are
                            // re-evaluated every window: each is counted exactly
                            // once, when its stamp comes within the horizon.
                            priced_cents.map(|_| SettleWatermark {
                                at: newest_in_bound.filter(|t| EVENT_YEARS.contains(&t.year())),
                                message_ids: merged_ids(&previous_ids, &message_ids),
                            }),
                        )
                    }
                    PricedObservation::SessionUnverified => {
                        rt.note(|t| t.unverified += 1);
                        (None, Some("transcript_unverified"), None)
                    }
                    PricedObservation::MalformedUsage => (None, Some("usage_malformed"), None),
                    PricedObservation::UnpricedModels(models) => {
                        rt.note_unpriced(&models);
                        (None, Some("unpriced_model"), None)
                    }
                    PricedObservation::Unavailable(why) => (
                        None,
                        Some(match why {
                            UsageUnavailable::NoTranscriptPath => "no_transcript_path",
                            UsageUnavailable::Unreadable => "transcript_unreadable",
                            UsageUnavailable::ScanCapReached => "scan_cap_reached",
                            UsageUnavailable::NoUsageInWindow => "no_usage_in_window",
                            UsageUnavailable::TurnIncomplete | UsageUnavailable::Inconsistent => {
                                "transcript_inconsistent"
                            }
                        }),
                        None,
                    ),
                }
            }
            // An invalid file: the pricing it would supply is not trusted.
            SharedBudgetFileState::Invalid(_) => (None, Some("config_invalid"), None),
            SharedBudgetFileState::Absent => return Ok(TurnSettlement::NotConfigured),
        };
    if reason.is_some() {
        rt.note(|t| t.unmeasured_settlements += 1);
    }

    let outcome = ledger.settle_shared_with_watermark(
        target.id,
        actual,
        now,
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
        watermark.as_ref(),
    )?;
    Ok(match outcome {
        SharedPoolSettleOutcome::Settled {
            reservation,
            overrun,
            ..
        } => TurnSettlement::Settled {
            reservation_id: reservation.id,
            pool_id: reservation.pool_id.0.clone(),
            hold_usd_cents: reservation.amount,
            settled_usd_cents: reservation.settled_amount.unwrap_or(reservation.amount),
            usage_known: reservation.usage_known.unwrap_or(false),
            overrun_usd_cents: overrun.unwrap_or(0),
            unmeasured_reason: reason,
        },
        SharedPoolSettleOutcome::AlreadyFinal(_) | SharedPoolSettleOutcome::NotFound => {
            TurnSettlement::AlreadySettled
        }
    })
}

/// Handles `SharedBudgetRequest::ChargeAgentTranscript` (HORO-2196).
///
/// Reads the subagent's own transcript (bounded, session-bound), prices it
/// cumulatively and books only what is not yet charged for this
/// `(session, agent)` -- or, for an unattributed subagent, this transcript
/// path. See [`SharedBudgetRequest::ChargeAgentTranscript`].
///
/// # Idempotency
///
/// The ledger key is `subagent-transcript:<session>|<family>|c<cumulative>`
/// (a measured delta) or `...|u` (the unmeasured fallback), where
/// `<family>` is the agent id, or a hash of the path when there is none.
/// "Already charged" is the sum of every row under the
/// `subagent-transcript:<session>|<family>|` prefix, across periods, so:
///
/// - a repeated `SubagentStop` with the same transcript books nothing;
/// - a later, larger transcript books only the difference;
/// - an earlier unmeasured fallback counts as already charged, so a later
///   measurement larger than it books the rest, and a smaller one books
///   nothing. A charge is never refunded.
///
/// # Unmeasured fallback
///
/// A missing, unreadable, over-cap, empty, unpriced or unverifiable
/// transcript is never silently omitted: `subagent_unmeasured_charge_usd_cents`
/// (else `turn_hold_usd_cents`) is charged once for the family with
/// `usage_known=false`. A transcript naming another session is refused and
/// charges nothing at all.
pub fn charge_agent_transcript(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    agent_id: Option<&str>,
    agent_transcript_path: &str,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    Ok(charge_transcript_inner(
        ledger,
        rt,
        session_id,
        agent_id,
        agent_transcript_path,
        false,
    )?
    .unwrap_or(SharedBudgetOutcome::NotConfigured))
}

/// The shared body of `ChargeAgentTranscript` and the `Stop` sweep. With
/// `sweep` set, a file with nothing measurable yet (missing, unreadable,
/// no assistant record) returns `None` instead of charging the unmeasured
/// fallback: a sweep runs on every `Stop` and must not bill a subagent that
/// simply has not flushed its first record. Genuine problems (mismatch,
/// unverified, unpriced, malformed) still take the fallback.
fn charge_transcript_inner(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    agent_id: Option<&str>,
    agent_transcript_path: &str,
    sweep: bool,
) -> Result<Option<SharedBudgetOutcome>, SharedBudgetError> {
    use sha2::{Digest, Sha256};

    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(Some(SharedBudgetOutcome::NotConfigured));
    }
    let executor_kind = match validate_identity(session_id, agent_id) {
        // A subagent without a known id is explicitly UNKNOWN, never the
        // main executor.
        Ok(ExecutorKind::AgentAbsent) => ExecutorKind::Unattributed,
        Ok(kind) => kind,
        Err(reason) => return Ok(Some(refused(reason))),
    };
    let cfg = match configured(&state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(Some(outcome)),
    };
    let cfg = &cfg;

    let family = match agent_id {
        Some(agent) => format!("agent:{agent}"),
        None => {
            let digest = Sha256::digest(agent_transcript_path.as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            format!("path-sha256:{hex}")
        }
    };
    let prefix = format!("subagent-transcript:{}|{}|", lp(session_id), lp(&family));

    let path = std::path::Path::new(agent_transcript_path);
    let observation = if agent_transcript_path.trim().is_empty() || !path.is_absolute() {
        PricedObservation::Unavailable(UsageUnavailable::NoTranscriptPath)
    } else {
        observe_priced(
            path,
            ReadMode::WholeFile { agent_id },
            session_id,
            &cfg.pricing_overrides.table(),
            BUDGET_SCAN_CAP_BYTES,
        )
    };

    struct Flags {
        unpriced: bool,
        unverified: bool,
        mismatch: bool,
    }
    let (measured, flags): (Option<(u64, Option<OffsetDateTime>)>, Flags) = match observation {
        PricedObservation::Priced(w) => {
            rt.note(|t| t.foreign_lines += w.foreign_lines);
            (
                Some((w.cents, w.last_event_time)),
                Flags {
                    unpriced: false,
                    unverified: false,
                    mismatch: false,
                },
            )
        }
        // Foreign spend is never imported, but a subagent is not free
        // either: the unmeasured fallback is charged (once per family) and
        // the mismatch counted.
        PricedObservation::SessionMismatch => {
            rt.note(|t| t.binding_mismatches += 1);
            (
                None,
                Flags {
                    unpriced: false,
                    unverified: false,
                    mismatch: true,
                },
            )
        }
        PricedObservation::FutureTimestamp { .. } => {
            rt.note(|t| t.future_timestamps += 1);
            (
                None,
                Flags {
                    unpriced: false,
                    unverified: false,
                    mismatch: false,
                },
            )
        }
        PricedObservation::MalformedUsage => (
            None,
            Flags {
                unpriced: false,
                unverified: false,
                mismatch: false,
            },
        ),
        PricedObservation::SessionUnverified => {
            rt.note(|t| t.unverified += 1);
            (
                None,
                Flags {
                    unpriced: false,
                    unverified: true,
                    mismatch: false,
                },
            )
        }
        PricedObservation::UnpricedModels(models) => {
            rt.note_unpriced(&models);
            (
                None,
                Flags {
                    unpriced: true,
                    unverified: false,
                    mismatch: false,
                },
            )
        }
        // In a sweep, a file with nothing readable yet is skipped; a file the
        // read cap truncates is NOT (it would be skipped on every sweep):
        // it takes the flagged unmeasured fallback once.
        PricedObservation::Unavailable(
            UsageUnavailable::NoUsageInWindow
            | UsageUnavailable::Unreadable
            | UsageUnavailable::NoTranscriptPath,
        ) if sweep => return Ok(None),
        PricedObservation::Unavailable(_) => (
            None,
            Flags {
                unpriced: false,
                unverified: false,
                mismatch: false,
            },
        ),
    };

    let already =
        ledger.shared_charges_total_by_key_prefix(&principal(), &prefix, OWNER_POOL_PREFIX)?;
    let (amount, key, event_time, usage_known) = match measured {
        Some((cumulative, last_event)) => {
            let delta = cumulative.saturating_sub(already);
            let event_time = last_event
                .filter(|t| EVENT_YEARS.contains(&t.year()))
                .map(rfc);
            (delta, format!("{prefix}c{cumulative}"), event_time, true)
        }
        None => {
            let fallback = cfg
                .subagent_unmeasured_charge_usd_cents
                .unwrap_or(cfg.turn_hold_usd_cents);
            // Charged once per family; anything already booked stands.
            let amount = if already == 0 { fallback } else { 0 };
            (amount, format!("{prefix}u"), None, false)
        }
    };
    let unmeasured = !usage_known;

    let now = rt.now();
    if amount == 0 {
        // Nothing new to book. Report the posture without writing.
        let period = period_at(cfg, now)?;
        let admission = ledger.pool_admission_for(&period.pool_id, now, source(cfg))?;
        return Ok(Some(SharedBudgetOutcome::TranscriptCharged {
            reservation_id: None,
            pool_id: None,
            amount_charged_usd_cents: 0,
            cumulative_usd_cents: already,
            already_charged: true,
            executor_kind,
            remaining_usd_cents: admission.map_or_else(
                || i64::try_from(cfg.monthly_cap_usd_cents).unwrap_or(i64::MAX),
                |a| a.remaining(),
            ),
            overrun_usd_cents: admission.map_or(0, |a| a.overrun()),
            usage_known,
            unmeasured,
            unpriced: flags.unpriced,
            transcript_unverified: flags.unverified,
            binding_mismatch: flags.mismatch,
            event_time_clamped: false,
        }));
    }

    let outcome = book_charge(
        ledger,
        rt,
        &state,
        BookedCharge {
            session_id,
            agent_id,
            amount_usd_cents: amount,
            key: &key,
            charge_source: ChargeSource::SubagentTranscript,
            event_time: event_time.as_deref(),
            usage_known,
            watermark: None,
        },
    )?;
    Ok(Some(match outcome {
        SharedBudgetOutcome::Charged {
            reservation_id,
            pool_id,
            amount_usd_cents,
            already_charged,
            executor_kind,
            remaining_usd_cents,
            overrun_usd_cents,
            event_time_clamped,
            ..
        } => {
            if unmeasured && !already_charged {
                rt.note(|t| t.unmeasured_charges += 1);
            }
            SharedBudgetOutcome::TranscriptCharged {
                reservation_id: Some(reservation_id),
                pool_id: Some(pool_id),
                amount_charged_usd_cents: if already_charged { 0 } else { amount_usd_cents },
                cumulative_usd_cents: if already_charged {
                    already
                } else {
                    already.saturating_add(amount_usd_cents)
                },
                already_charged,
                executor_kind,
                remaining_usd_cents,
                overrun_usd_cents,
                usage_known,
                unmeasured,
                unpriced: flags.unpriced,
                transcript_unverified: flags.unverified,
                binding_mismatch: flags.mismatch,
                event_time_clamped,
            }
        }
        other => other,
    }))
}

/// Most subagent files one `Stop` sweep will look at.
const MAX_SWEEP_FILES: usize = 64;

/// Charges the not-yet-charged part of every native subagent transcript of
/// `session_id` (HORO-2196). The host writes a subagent's file only when
/// the subagent runs and lags behind its `SubagentStop`, so a subagent's
/// final records are often missing when its own `SubagentStop` charges it.
/// The parent's `Stop` therefore also visits
/// `<transcript_path minus .jsonl>/subagents/agent-<agent_id>.jsonl` and
/// charges each file's delta through the same cumulative-delta logic (same
/// binding: each usage line's `sessionId` must be the parent's and its
/// `agentId` this file's). A file with nothing measurable yet is skipped,
/// not billed the unmeasured fallback. Returns how many files were charged
/// something. Never fails the `Stop`.
pub fn sweep_subagent_transcripts(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    transcript_path: &str,
) -> usize {
    sweep_with_budget(ledger, rt, session_id, transcript_path, SWEEP_BYTE_BUDGET).charged
}

/// What one sweep did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Files charged something.
    pub charged: usize,
    /// Files left for the next `Stop` because the byte budget was spent.
    pub deferred: usize,
}

/// Bytes of subagent files one `Finalize` sweep may read in total. Files
/// past it are deferred to the next `Stop` (counted in
/// `Status.sweep_deferred_files`), so a session with many large subagent
/// transcripts cannot make every `Stop` read gigabytes.
pub const SWEEP_BYTE_BUDGET: u64 = 128 * 1024 * 1024;

/// [`sweep_subagent_transcripts`] with an explicit byte budget.
pub fn sweep_with_budget(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    transcript_path: &str,
    byte_budget: u64,
) -> SweepReport {
    use std::os::unix::fs::MetadataExt;

    let Some(stem) = transcript_path.strip_suffix(".jsonl") else {
        return SweepReport::default();
    };
    if !std::path::Path::new(transcript_path).is_absolute() {
        return SweepReport::default();
    }
    let dir = std::path::Path::new(stem).join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return SweepReport::default();
    };
    let mut files: Vec<(String, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let agent = name.strip_prefix("agent-")?.strip_suffix(".jsonl")?;
            let valid = !agent.is_empty()
                && agent
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            valid.then(|| (agent.to_string(), e.path()))
        })
        .collect();
    files.sort();
    files.truncate(MAX_SWEEP_FILES);
    let mut charged = 0;
    let mut spent: u64 = 0;
    let mut deferred: u64 = 0;
    let mut processed_any = false;
    for (agent, path) in files {
        let Some(path) = path.to_str() else { continue };
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let stamp = (
            meta.len(),
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        );
        let key = (session_id.to_string(), agent.clone());
        let unchanged = rt
            .sweep_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            == Some(&stamp);
        if unchanged {
            continue;
        }
        // A file is charged against the budget at no more than the most one
        // read can scan, and the first file of every sweep is always
        // processed: a file larger than the whole budget must not be
        // deferred forever.
        let cost = stamp.0.min(BUDGET_SCAN_CAP_BYTES);
        if processed_any && spent.saturating_add(cost) > byte_budget {
            deferred += 1;
            continue;
        }
        processed_any = true;
        spent = spent.saturating_add(cost);
        match charge_transcript_inner(ledger, rt, session_id, Some(&agent), path, true) {
            Ok(Some(SharedBudgetOutcome::TranscriptCharged {
                amount_charged_usd_cents,
                ..
            })) => {
                if amount_charged_usd_cents > 0 {
                    charged += 1;
                }
                remember_swept(rt, key, stamp);
            }
            Ok(None) => remember_swept(rt, key, stamp),
            // A denial (invalid config) or an error is retried next Stop.
            _ => {}
        }
    }
    if deferred > 0 {
        rt.note(|t| t.sweep_deferred_files += deferred);
    }
    SweepReport {
        charged,
        deferred: deferred as usize,
    }
}

fn remember_swept(rt: &SharedBudgetRuntime, key: (String, String), stamp: (u64, i128)) {
    let mut seen = rt.sweep_seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= 4096 {
        seen.clear();
    }
    seen.insert(key, stamp);
}

/// Handles `SharedBudgetRequest::SettleTail` (HORO-2196). See the request.
pub fn settle_tail(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    transcript_path: &str,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let state = rt.effective().state;
    if matches!(state, SharedBudgetFileState::Absent) {
        return Ok(SharedBudgetOutcome::NotConfigured);
    }
    if let Err(reason) = validate_identity(session_id, None) {
        return Ok(refused(reason));
    }
    let cfg = match configured(&state) {
        Ok(cfg) => cfg,
        Err(outcome) => return Ok(outcome),
    };
    let cfg = &cfg;
    let now = rt.now();
    let period = period_at(cfg, now)?;
    let admission = ledger.pool_admission_for(&period.pool_id, now, source(cfg))?;
    let posture = |tail: TailFlags,
                   row: Option<(libra_governor_domain::SharedPoolReservationId, String)>,
                   charged: u64,
                   already: bool| {
        let (id, pool) = row.map_or((None, None), |(i, p)| (Some(i), Some(p)));
        SharedBudgetOutcome::TailSettled {
            reservation_id: id,
            pool_id: pool,
            amount_charged_usd_cents: charged,
            already_charged: already,
            remaining_usd_cents: admission.map_or_else(
                || i64::try_from(cfg.monthly_cap_usd_cents).unwrap_or(i64::MAX),
                |a| a.remaining(),
            ),
            overrun_usd_cents: admission.map_or(0, |a| a.overrun()),
            unmeasured: tail.unmeasured,
            unpriced: tail.unpriced,
            transcript_unverified: tail.unverified,
            binding_mismatch: tail.mismatch,
        }
    };

    let holds = ledger.shared_lane_holds(
        &principal(),
        session_id,
        None,
        ExecutorKind::AgentAbsent,
        OWNER_POOL_PREFIX,
    )?;
    // A tail continues a lane that has settled under this budget. A session
    // that was never admitted, or whose holds never settled, has no
    // watermark to continue from, and charging "everything past nothing"
    // would bill its whole transcript history.
    let has_mark = ledger
        .shared_lane_watermark(
            &principal(),
            session_id,
            None,
            ExecutorKind::AgentAbsent,
            OWNER_POOL_PREFIX,
        )?
        .is_some_and(|m| m.at.is_some());
    let has_settled_hold = holds.iter().any(|h| h.state == ReservationState::Settled);
    if !has_mark && !has_settled_hold {
        return Ok(refused(SharedBudgetRefusal::NoSettledLane));
    }
    // The same lower bound a Stop uses (the mark, else the latest settle
    // time), never the lane's earliest hold: earlier unmeasured settlements
    // were already booked at the hold amount.
    let (after, previous_ids) = lane_window(ledger, session_id, &holds)?
        .ok_or(SharedBudgetError::Internal("a settled lane has no hold"))?;
    let path = std::path::Path::new(transcript_path);
    if !path.is_absolute() {
        return Ok(posture(
            TailFlags {
                unmeasured: true,
                ..TailFlags::default()
            },
            None,
            0,
            false,
        ));
    }
    let counted_ids: HashSet<String> = previous_ids.iter().cloned().collect();
    let observation = observe_priced(
        path,
        ReadMode::ParentWindow {
            after,
            counted_ids: &counted_ids,
            horizon: now + FUTURE_SKEW,
        },
        session_id,
        &cfg.pricing_overrides.table(),
        BUDGET_SCAN_CAP_BYTES,
    );
    let flags = match observation {
        PricedObservation::Priced(window) => {
            let Some(last) = window
                .last_event_time
                .filter(|t| EVENT_YEARS.contains(&t.year()))
            else {
                return Ok(posture(TailFlags::default(), None, 0, false));
            };
            if window.cents == 0 {
                return Ok(posture(TailFlags::default(), None, 0, false));
            }
            rt.note(|t| {
                t.id_less_records += window.id_less_records;
                t.foreign_lines += window.foreign_lines;
            });
            let key = format!("tail:{}|{}|{}", lp(session_id), rfc(after), rfc(last));
            let event_time = rfc(last);
            let mark = SettleWatermark {
                at: Some(last),
                message_ids: merged_ids(&previous_ids, &window.message_ids),
            };
            // The charge and its mark are one ledger transaction.
            let outcome = book_charge(
                ledger,
                rt,
                &state,
                BookedCharge {
                    session_id,
                    agent_id: None,
                    amount_usd_cents: window.cents,
                    key: &key,
                    charge_source: ChargeSource::TurnHold,
                    event_time: Some(&event_time),
                    usage_known: true,
                    watermark: Some(&mark),
                },
            )?;
            return Ok(match outcome {
                SharedBudgetOutcome::Charged {
                    reservation_id,
                    pool_id,
                    already_charged,
                    amount_usd_cents,
                    remaining_usd_cents,
                    overrun_usd_cents,
                    ..
                } => SharedBudgetOutcome::TailSettled {
                    reservation_id: Some(reservation_id),
                    pool_id: Some(pool_id),
                    amount_charged_usd_cents: if already_charged { 0 } else { amount_usd_cents },
                    already_charged,
                    remaining_usd_cents,
                    overrun_usd_cents,
                    unmeasured: false,
                    unpriced: false,
                    transcript_unverified: false,
                    binding_mismatch: false,
                },
                other => other,
            });
        }
        PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow) => TailFlags::default(),
        PricedObservation::SessionMismatch => {
            rt.note(|t| t.binding_mismatches += 1);
            TailFlags {
                mismatch: true,
                ..TailFlags::default()
            }
        }
        PricedObservation::SessionUnverified => {
            rt.note(|t| t.unverified += 1);
            TailFlags {
                unverified: true,
                unmeasured: true,
                ..TailFlags::default()
            }
        }
        PricedObservation::UnpricedModels(models) => {
            rt.note_unpriced(&models);
            TailFlags {
                unpriced: true,
                unmeasured: true,
                ..TailFlags::default()
            }
        }
        PricedObservation::FutureTimestamp { .. } => {
            rt.note(|t| t.future_timestamps += 1);
            TailFlags {
                unmeasured: true,
                ..TailFlags::default()
            }
        }
        PricedObservation::MalformedUsage | PricedObservation::Unavailable(_) => TailFlags {
            unmeasured: true,
            ..TailFlags::default()
        },
    };
    Ok(posture(flags, None, 0, false))
}

/// Handles `SharedBudgetRequest::TranscriptReady` (HORO-2246): would a
/// `Stop` settlement right now find this turn's usage in the transcript?
///
/// **Read-only by construction**: it takes `&LedgerStore`, writes nothing,
/// and touches no telemetry counter (a poll must not inflate
/// `unmeasured_settlements`, `unverified` and the rest).
///
/// The answer is `ready = false` only when waiting could change what
/// `Finalize` books, which is exactly the two outcomes a host that has not
/// written the transcript yet produces: `Unreadable` (the file does not
/// exist yet, as on the first turn of a session) and `NoUsageInWindow` (a
/// resumed session whose new lines are not appended yet). Every other
/// observation is `ready = true`, genuine problems included (binding
/// mismatch, unverified, unpriced, malformed, future-stamped, scan cap):
/// they settle the same way however long the hook waits, so burning the
/// deadline on them would only delay the hook. A lane with no pending hold
/// has nothing to settle and is ready; so is an invalid file, which settles
/// the hold at its own amount regardless.
pub fn transcript_ready(
    ledger: &LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
    transcript_path: &str,
) -> Result<SharedBudgetOutcome, SharedBudgetError> {
    let state = rt.effective().state;
    let cfg = match &state {
        SharedBudgetFileState::Absent => return Ok(SharedBudgetOutcome::NotConfigured),
        SharedBudgetFileState::Invalid(_) => {
            return Ok(SharedBudgetOutcome::TranscriptReadiness { ready: true })
        }
        SharedBudgetFileState::Valid(cfg) => cfg,
    };
    if let Err(reason) = validate_identity(session_id, None) {
        return Ok(refused(reason));
    }
    let holds = ledger.shared_lane_holds(
        &principal(),
        session_id,
        None,
        ExecutorKind::AgentAbsent,
        OWNER_POOL_PREFIX,
    )?;
    let Some(target) = pending_hold(&holds) else {
        return Ok(SharedBudgetOutcome::TranscriptReadiness { ready: true });
    };
    let (after, previous_ids) =
        lane_window(ledger, session_id, &holds)?.unwrap_or((target.created_at, Vec::new()));
    let counted_ids: HashSet<String> = previous_ids.into_iter().collect();
    let observation = observe_priced(
        std::path::Path::new(transcript_path),
        ReadMode::ParentWindow {
            after,
            counted_ids: &counted_ids,
            horizon: rt.now() + FUTURE_SKEW,
        },
        session_id,
        &cfg.pricing_overrides.table(),
        BUDGET_SCAN_CAP_BYTES,
    );
    let ready = !matches!(
        observation,
        PricedObservation::Unavailable(
            UsageUnavailable::Unreadable | UsageUnavailable::NoUsageInWindow
        )
    );
    Ok(SharedBudgetOutcome::TranscriptReadiness { ready })
}

#[derive(Default)]
struct TailFlags {
    unmeasured: bool,
    unpriced: bool,
    unverified: bool,
    mismatch: bool,
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
        future_pool_detected: None,
        period: None,
        pool_exists: false,
        capacity_known: false,
        used_usd_cents: 0,
        reserved_usd_cents: 0,
        remaining_usd_cents: None,
        executors: Vec::new(),
        warnings: Vec::new(),
        unpriced_models: Vec::new(),
        transcript_binding_mismatches: 0,
        transcript_unverified: 0,
        unmeasured_settlements: 0,
        unmeasured_charges: 0,
        id_less_records: 0,
        foreign_lines: 0,
        sweep_deferred_files: 0,
        future_timestamp_settlements: 0,
    };
    {
        let t = rt.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        report.unpriced_models = t.unpriced_models.iter().cloned().collect();
        report.transcript_binding_mismatches = t.binding_mismatches;
        report.transcript_unverified = t.unverified;
        report.unmeasured_settlements = t.unmeasured_settlements;
        report.unmeasured_charges = t.unmeasured_charges;
        report.id_less_records = t.id_less_records;
        report.foreign_lines = t.foreign_lines;
        report.sweep_deferred_files = t.sweep_deferred_files;
        report.future_timestamp_settlements = t.future_timestamps;
    }
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
        pricing_override_count: cfg.pricing_overrides.len() as u64,
    });
    let now = rt.now();
    let period = period_at(cfg, now)?;
    let later = later_pool(ledger, &period)?;
    report.clock_anomaly = later.is_some();
    report.future_pool_detected = later.map(|p| {
        p.0.strip_prefix(OWNER_POOL_PREFIX)
            .unwrap_or(&p.0)
            .to_string()
    });
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
        // Totals and per-executor rows come from one ledger snapshot, so the
        // table always sums to the figures reported beside it.
        let executors = ledger.shared_pool_executor_usage(&period.pool_id)?;
        report.used_usd_cents = executors.total_used;
        report.reserved_usd_cents = executors.total_reserved;
        let remaining = i128::from(cfg.monthly_cap_usd_cents)
            - i128::from(executors.total_used)
            - i128::from(executors.total_reserved);
        report.remaining_usd_cents =
            Some(remaining.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64);
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
/// of `session_id` in the current period's pool, whichever lane it belongs
/// to (a tool event carries no agent id, so the lane cannot be preferred). Returns how many were renewed. Does nothing -- not even a
/// ledger read -- unless a valid shared-budget file is loaded.
pub fn heartbeat(
    ledger: &mut LedgerStore,
    rt: &SharedBudgetRuntime,
    session_id: &str,
) -> Result<usize, SharedBudgetError> {
    let state = rt.effective().state;
    let SharedBudgetFileState::Valid(cfg) = &state else {
        return Ok(0);
    };
    if validate_identity(session_id, None).is_err() {
        return Ok(0);
    }
    let now = rt.now();
    let period = period_at(cfg, now)?;
    let mut renewed = 0;
    // Only the current period's holds: an earlier period's hold clears by
    // settling or by its own expiry, and a tool event in a later month is
    // not evidence that last month's turn is still running.
    for hold in
        ledger.active_shared_holds_for_session(&principal(), session_id, OWNER_POOL_PREFIX, now)?
    {
        if hold.pool_id != period.pool_id {
            continue;
        }
        if matches!(
            ledger.renew_shared(hold.id, &principal(), session_id, now, HOLD_TTL_SECS)?,
            SharedPoolRenewOutcome::Renewed(_)
        ) {
            renewed += 1;
        }
    }
    Ok(renewed)
}
