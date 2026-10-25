//! Read-only presentation of the local monthly shared budget (HORO-2217,
//! ADR-0026 S8): the model behind `shared-budget status`, the statusline
//! segment and the `doctor` checks.
//!
//! One rule governs everything here: **it observes, never changes**. It
//! never spawns a daemon, never writes a file, never reserves or charges,
//! and with no `shared-budget.json` it does not even connect. The daemon's
//! `Status` answer is the single source of the figures; this module only
//! formats them and cross-checks that they add up.
//!
//! Wording is fixed on purpose and must not be strengthened: the limit is
//! *admission-bounded (cooperative)*, and the unit is Libra-accounted
//! list-price-equivalent USD, never a provider bill or quota.

use std::path::{Path, PathBuf};
use std::time::Duration;

use libra_governor_daemon::shared_budget_file::{LoadedSharedBudget, SharedBudgetFileState};
use libra_governor_domain::{format_local_rfc3339, IanaTimeZone};
use libra_governor_protocol::{
    Request, Response, SharedBudgetConfigDrift, SharedBudgetConfigState, SharedBudgetOutcome,
    SharedBudgetRequest, SharedBudgetStatus, SoftLimitScope,
};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::agent::budget_gate;
use crate::client;

pub const STATUS_SCHEMA: &str = "libra.shared-budget-status.v1";

/// The limit's kind, as ADR-0026 words it.
pub const GUARANTEE_LABEL: &str = "admission-bounded (cooperative)";

/// How to turn the budget on; shown wherever "not configured" is reported.
pub const ENABLE_HINT: &str = "To enable it: libra-governor shared-budget init --monthly-usd <n> \
    --time-zone <IANA> (for example --monthly-usd 200 --time-zone America/New_York)";

/// What a dollar figure means.
pub const USD_MEANING: &str = "Libra-accounted list-price-equivalent USD at the pinned \
     PRICING_VERSION; never a provider bill or quota";

/// What the budget does not cover. Stated by `status` and `doctor` alike.
pub const NOT_COVERED: [&str; 3] = [
    "in-turn overshoot in hooks-only mode: spend inside a turn is visible only after the fact \
     (at Stop), so a turn can cost more than its hold",
    "the Codex route has no budget gate: Codex sessions are not admitted or charged by this budget",
    "there is no gateway hard cap: the per-request hard cap (gateway governor_held mode) is not \
     built",
];

/// How long the statusline segment may wait for the daemon. The shared
/// statusline host kills a provider after 250 ms.
pub const SEGMENT_BUDGET: Duration = Duration::from_millis(200);

/// How long `status` and `doctor` wait for the daemon.
pub const REPORT_BUDGET: Duration = Duration::from_secs(2);

/// What a read-only look at the budget found.
#[derive(Debug)]
pub enum Posture {
    /// No budget file: nothing was consulted, connected to or written.
    NotConfigured,
    /// The daemon answered `Status`.
    Reported(Box<SharedBudgetStatus>),
    /// A budget file exists but the daemon did not answer (it is never
    /// started from here). Only the file itself can be described.
    DaemonUnreachable {
        file: Box<LoadedSharedBudget>,
        reason: String,
    },
}

pub fn pricing_version() -> &'static str {
    libra_governor_gateway::pricing::PRICING_VERSION
}

/// Asks a running daemon for the budget status, bounded by one overall
/// deadline around connect + write + read (a daemon that trickles bytes
/// defeats per-read socket timeouts). Never spawns a daemon.
pub fn fetch_status(socket: &Path, budget: Duration) -> Result<SharedBudgetStatus, String> {
    let socket = socket.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = client::connect_only_with_budget(&socket, budget).and_then(|stream| {
            client::roundtrip(
                &stream,
                Request::SharedBudget {
                    shared_budget: SharedBudgetRequest::Status {},
                },
            )
        });
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(budget) {
        Ok(Ok(Response::SharedBudget(outcome))) => match *outcome {
            SharedBudgetOutcome::Status(status) => Ok(*status),
            other => Err(format!("unexpected shared-budget answer: {other:?}")),
        },
        Ok(Ok(_)) => Err("unexpected response kind for a shared-budget status".to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the daemon did not answer in time".to_string()),
    }
}

/// Looks at the budget without changing anything.
pub fn collect(budget: Duration) -> Posture {
    if !budget_gate::budget_file_present() {
        return Posture::NotConfigured;
    }
    let reason = match libra_governor_daemon::paths::socket_path() {
        Ok(socket) => match fetch_status(&socket, budget) {
            // The file vanished between the stat and the daemon's read.
            Ok(status) if status.config_state == SharedBudgetConfigState::Absent => {
                return Posture::NotConfigured;
            }
            Ok(status) => return Posture::Reported(Box::new(status)),
            Err(reason) => reason,
        },
        Err(e) => format!("could not resolve the daemon socket: {e}"),
    };
    let file = match libra_governor_daemon::paths::state_dir() {
        Ok(dir) => LoadedSharedBudget::load(&dir),
        Err(_) => LoadedSharedBudget::absent(),
    };
    Posture::DaemonUnreachable {
        file: Box::new(file),
        reason,
    }
}

// ---------------------------------------------------------------------
// Figures
// ---------------------------------------------------------------------

pub fn usd(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

pub fn usd_u(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// `$500` for a whole-dollar figure, `$500.25` otherwise.
fn usd_short(cents: u64) -> String {
    if cents.is_multiple_of(100) {
        format!("${}", cents / 100)
    } else {
        usd_u(cents)
    }
}

/// Flattens daemon- or file-supplied text to one bounded printable line.
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() > 120 {
        let cut: String = flat.chars().take(120).collect();
        format!("{cut}...")
    } else {
        flat.to_string()
    }
}

/// Whether the per-executor rows add up to the pool figures beside them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    pub executors_used: u128,
    pub executors_reserved: u128,
    pub pool_used: u64,
    pub pool_reserved: u64,
    /// `cap - used - reserved` as the reported remaining figure claims.
    pub remaining_consistent: bool,
}

impl Reconciliation {
    pub fn sums_match(&self) -> bool {
        self.executors_used == u128::from(self.pool_used)
            && self.executors_reserved == u128::from(self.pool_reserved)
    }

    pub fn ok(&self) -> bool {
        self.sums_match() && self.remaining_consistent
    }

    /// One line naming every disagreement, or `None` when all agree.
    pub fn discrepancy(&self) -> Option<String> {
        if self.ok() {
            return None;
        }
        let mut parts = Vec::new();
        if self.executors_used != u128::from(self.pool_used) {
            parts.push(format!(
                "executors settled {} != pool settled {}",
                usd_u(clamp_u64(self.executors_used)),
                usd_u(self.pool_used)
            ));
        }
        if self.executors_reserved != u128::from(self.pool_reserved) {
            parts.push(format!(
                "executors held {} != pool held {}",
                usd_u(clamp_u64(self.executors_reserved)),
                usd_u(self.pool_reserved)
            ));
        }
        if !self.remaining_consistent {
            parts.push("remaining != cap - settled - held".to_string());
        }
        Some(parts.join("; "))
    }
}

fn clamp_u64(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

pub fn reconcile(status: &SharedBudgetStatus) -> Reconciliation {
    let executors_used = status
        .executors
        .iter()
        .map(|e| u128::from(e.used_usd_cents))
        .sum();
    let executors_reserved = status
        .executors
        .iter()
        .map(|e| u128::from(e.reserved_usd_cents))
        .sum();
    let remaining_consistent = match (&status.config, status.remaining_usd_cents) {
        (Some(cfg), Some(remaining)) => {
            let expected = i128::from(cfg.monthly_cap_usd_cents)
                - i128::from(status.used_usd_cents)
                - i128::from(status.reserved_usd_cents);
            expected.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) == i128::from(remaining)
        }
        _ => true,
    };
    Reconciliation {
        executors_used,
        executors_reserved,
        pool_used: status.used_usd_cents,
        pool_reserved: status.reserved_usd_cents,
        remaining_consistent,
    }
}

/// Settlement counts that mean "this spend was estimated or skipped, not
/// measured". They count since the daemon started.
pub fn flagged_counts(status: &SharedBudgetStatus) -> [(&'static str, u64); 8] {
    [
        ("unmeasured_settlements", status.unmeasured_settlements),
        ("unmeasured_charges", status.unmeasured_charges),
        (
            "future_timestamp_settlements",
            status.future_timestamp_settlements,
        ),
        ("binding_mismatch", status.transcript_binding_mismatches),
        ("transcript_unverified", status.transcript_unverified),
        ("foreign_lines", status.foreign_lines),
        ("sweep_deferred_files", status.sweep_deferred_files),
        ("id_less_records", status.id_less_records),
    ]
}

pub fn flagged_total(status: &SharedBudgetStatus) -> u64 {
    flagged_counts(status)
        .iter()
        .fold(0u64, |acc, (_, n)| acc.saturating_add(*n))
        .saturating_add(status.unpriced_models.len() as u64)
}

/// Settled plus held: what the pool has committed against the cap.
fn committed(status: &SharedBudgetStatus) -> u64 {
    status
        .used_usd_cents
        .saturating_add(status.reserved_usd_cents)
}

// ---------------------------------------------------------------------
// Statusline segment
// ---------------------------------------------------------------------

/// The short statusline segment, e.g. `budget $12.30/$500 (2%)` with a
/// ` WARN` marker when a soft limit is crossed or the pool is overrun, and
/// a ` · budget low: new sessions blocked` marker once less than a turn hold
/// remains. `None` for a state that has nothing useful to say in a few characters.
pub fn statusline_segment(status: &SharedBudgetStatus) -> Option<String> {
    match status.config_state {
        SharedBudgetConfigState::Absent => return None,
        SharedBudgetConfigState::Invalid => return Some("budget: config invalid".to_string()),
        SharedBudgetConfigState::Valid => {}
    }
    if status.clock_anomaly {
        return Some("budget: clock anomaly".to_string());
    }
    let cap = status.config.as_ref()?.monthly_cap_usd_cents;
    let spent = committed(status);
    // Rounded up for any spend, so 0.4% never reads 0% and 99.9% never 99%.
    let percent = if cap == 0 {
        0
    } else {
        (u128::from(spent) * 100)
            .div_ceil(u128::from(cap))
            .min(9_999)
    };
    // At or below zero remaining, admission is closed (fully held or overrun).
    let closed = status.remaining_usd_cents.is_some_and(|r| r <= 0);
    let warn = !status.warnings.is_empty() || closed;
    // Less than one turn hold left: a lane without a hold is denied. The
    // statusline is per session and a lane that already holds budget is
    // renewed, so this says "new sessions" rather than "your next prompt".
    let blocked = if admission_closed(status) {
        " · budget low: new sessions blocked"
    } else {
        ""
    };
    Some(format!(
        "budget {}/{} ({percent}%){}{blocked}",
        usd_u(spent),
        usd_short(cap),
        if warn { " WARN" } else { "" }
    ))
}

/// The segment for the legacy statusline line, or `None` (printing
/// nothing extra) when no budget file exists or anything goes wrong.
pub fn statusline_segment_if_configured() -> Option<String> {
    if !budget_gate::budget_file_present() {
        return None;
    }
    let socket = libra_governor_daemon::paths::socket_path().ok()?;
    let status = fetch_status(&socket, SEGMENT_BUDGET).ok()?;
    statusline_segment(&status)
}

// ---------------------------------------------------------------------
// `shared-budget status`
// ---------------------------------------------------------------------

fn local(instant: &str, tz: Option<&IanaTimeZone>) -> Option<String> {
    let parsed = OffsetDateTime::parse(instant, &Rfc3339).ok()?;
    Some(match tz {
        Some(tz) => format_local_rfc3339(parsed, tz),
        None => parsed.format(&Rfc3339).ok()?,
    })
}

fn period_label(pool_id: &str) -> &str {
    pool_id.rsplit('/').next().unwrap_or(pool_id)
}

fn drift_text(drift: SharedBudgetConfigDrift) -> Option<&'static str> {
    match drift {
        SharedBudgetConfigDrift::None => None,
        SharedBudgetConfigDrift::Changed => Some("file edited since the daemon started"),
        SharedBudgetConfigDrift::Appeared => Some("file created since the daemon started"),
        SharedBudgetConfigDrift::Removed => Some("file removed since the daemon started"),
        SharedBudgetConfigDrift::Unreadable => Some("file unreadable now"),
    }
}

fn scope_word(scope: SoftLimitScope) -> &'static str {
    match scope {
        SoftLimitScope::PerSession => "session",
        SoftLimitScope::PerAgent => "agent",
    }
}

pub fn render_human(posture: &Posture, state_dir: Option<&Path>) -> String {
    let mut out = String::new();
    match posture {
        Posture::NotConfigured => {
            out.push_str("Shared budget: not configured\n");
            match state_dir {
                Some(dir) => out.push_str(&format!(
                    "  No shared-budget.json in {}; nothing was consulted or written.\n",
                    dir.display()
                )),
                None => {
                    out.push_str("  No shared-budget.json; nothing was consulted or written.\n")
                }
            }
            out.push_str(&format!("  {ENABLE_HINT}\n"));
        }
        Posture::DaemonUnreachable { file, reason } => {
            out.push_str("Shared budget (local, this host and OS user)\n");
            push_labels(&mut out);
            match &file.state {
                SharedBudgetFileState::Absent => out.push_str("Config: absent\n"),
                SharedBudgetFileState::Invalid(why) => out.push_str(&format!(
                    "Config: INVALID ({}); prompt admission will deny until it is fixed\n",
                    one_line(why)
                )),
                SharedBudgetFileState::Valid(cfg) => out.push_str(&format!(
                    "Config: valid; monthly cap {} in {}\n",
                    usd_u(cfg.monthly_cap_usd_cents),
                    cfg.time_zone.as_str()
                )),
            }
            out.push_str(&format!(
                "Daemon: not reachable ({}); it was not started. Spend, holds and per-executor \
                 figures are unavailable until it runs.\n",
                one_line(reason)
            ));
        }
        Posture::Reported(status) => render_reported(&mut out, status),
    }
    out
}

fn push_labels(out: &mut String) {
    out.push_str(&format!("Guarantee: {GUARANTEE_LABEL}\n"));
    out.push_str(&format!("Units: {USD_MEANING} ({})\n", pricing_version()));
}

fn render_reported(out: &mut String, s: &SharedBudgetStatus) {
    out.push_str("Shared budget (local, this host and OS user)\n");
    push_labels(out);
    match s.config_state {
        SharedBudgetConfigState::Absent => out.push_str("Config: absent\n"),
        SharedBudgetConfigState::Invalid => out.push_str(&format!(
            "Config: INVALID ({}); prompt admission denies until it is fixed\n",
            one_line(
                s.config_invalid_reason
                    .as_deref()
                    .unwrap_or("unknown reason")
            )
        )),
        SharedBudgetConfigState::Valid => out.push_str("Config: valid\n"),
    }
    if let Some(path) = &s.config_path {
        out.push_str(&format!("  file: {}\n", one_line(path)));
    }
    if let Some(note) = &s.config_permissions_note {
        out.push_str(&format!("  note: {}\n", one_line(note)));
    }
    if let Some(text) = drift_text(s.config_drift) {
        out.push_str(&format!(
            "  note: {text}; the daemon re-reads the file on every request, so the figures \
             below already use the file in force\n"
        ));
    }
    let Some(cfg) = &s.config else {
        out.push_str("No capacity is known, so no spend figures are authoritative.\n");
        return;
    };
    if !s.capacity_known {
        out.push_str(
            "CAPACITY UNKNOWN: the pool exists but its capacity could not be established, so \
             the figures below are not authoritative\n",
        );
    }
    let tz = IanaTimeZone::new(&cfg.time_zone).ok();
    if let Some(period) = &s.period {
        out.push_str(&format!(
            "Period: {} ({})\n  start: {}\n  end:   {}\n",
            period_label(&period.pool_id),
            cfg.time_zone,
            local(&period.start, tz.as_ref()).unwrap_or_else(|| period.start.clone()),
            local(&period.end, tz.as_ref()).unwrap_or_else(|| period.end.clone()),
        ));
    }
    if s.clock_anomaly {
        out.push_str(&format!(
            "CLOCK ANOMALY: a pool of a later period ({}) exists than the clock's; admission \
             denies until this is resolved\n",
            s.future_pool_detected.as_deref().unwrap_or("unknown")
        ));
    } else {
        out.push_str("Clock: no anomaly\n");
    }
    out.push_str(&format!(
        "Monthly cap: {}   Turn hold: {}\n",
        usd_u(cfg.monthly_cap_usd_cents),
        usd_u(cfg.turn_hold_usd_cents)
    ));
    if !s.pool_exists {
        out.push_str("Pool: not created yet (the first admission of the period creates it)\n");
    }
    out.push_str(&format!(
        "Settled: {}   Held: {}\n",
        usd_u(s.used_usd_cents),
        usd_u(s.reserved_usd_cents)
    ));
    match s.remaining_usd_cents {
        Some(r) if r < 0 => out.push_str(&format!(
            "Remaining: {} (OVERRUN: spend exceeded the cap; admission is closed)\n",
            usd(r)
        )),
        Some(r) => out.push_str(&format!("Remaining: {}\n", usd(r))),
        None => out.push_str("Remaining: unknown\n"),
    }

    out.push_str(&format!("Executors ({}):\n", s.executors.len()));
    if s.executors.is_empty() {
        out.push_str("  (none)\n");
    } else {
        out.push_str(&format!(
            "  {:<28} {:<24} {:<13} {:>10} {:>10}\n",
            "SESSION", "AGENT", "KIND", "SETTLED", "HELD"
        ));
        for e in &s.executors {
            out.push_str(&format!(
                "  {:<28} {:<24} {:<13} {:>10} {:>10}\n",
                clip(&one_line(&e.session_id), 28),
                clip(&one_line(e.agent_id.as_deref().unwrap_or("-")), 24),
                e.executor_kind.as_str(),
                usd_u(e.used_usd_cents),
                usd_u(e.reserved_usd_cents),
            ));
        }
    }
    let rec = reconcile(s);
    match rec.discrepancy() {
        None => out.push_str("Reconciliation: OK (executor rows sum to the pool totals)\n"),
        Some(why) => out.push_str(&format!("Reconciliation: DISCREPANCY ({why})\n")),
    }

    let limits = match (
        cfg.soft_limit_per_session_usd_cents,
        cfg.soft_limit_per_agent_usd_cents,
    ) {
        (None, None) => "none configured".to_string(),
        (a, b) => format!(
            "per session {}, per agent {}",
            a.map_or("-".to_string(), usd_u),
            b.map_or("-".to_string(), usd_u)
        ),
    };
    out.push_str(&format!(
        "Soft limits (warn-only; they never deny admission): {limits}\n"
    ));
    for w in &s.warnings {
        out.push_str(&format!(
            "  WARN {} {}{}: {} settled+held over the {} limit\n",
            scope_word(w.scope),
            clip(&one_line(&w.session_id), 28),
            w.agent_id
                .as_deref()
                .map(|a| format!("/{}", clip(&one_line(a), 24)))
                .unwrap_or_default(),
            usd_u(w.used_plus_reserved_usd_cents),
            usd_u(w.threshold_usd_cents),
        ));
    }

    let flagged = flagged_total(s);
    if flagged == 0 {
        out.push_str("Flagged settlements (since daemon start): none\n");
    } else {
        out.push_str(
            "Flagged settlements (since daemon start; estimated or skipped, not measured):\n",
        );
        for (name, n) in flagged_counts(s) {
            if n > 0 {
                out.push_str(&format!("  {name}: {n}\n"));
            }
        }
        if !s.unpriced_models.is_empty() {
            let models: Vec<String> = s.unpriced_models.iter().map(|m| one_line(m)).collect();
            out.push_str(&format!("  unpriced_models: {}\n", models.join(", ")));
        }
    }
    out.push_str("Does NOT cover:\n");
    for item in NOT_COVERED {
        out.push_str(&format!("  - {item}\n"));
    }
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let cut: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}~")
    }
}

pub fn render_json(posture: &Posture, state_dir: Option<&Path>) -> Value {
    let base = |state: &str| {
        json!({
            "schema": STATUS_SCHEMA,
            "state": state,
            "guarantee": GUARANTEE_LABEL,
            "usd_meaning": USD_MEANING,
            "pricing_version": pricing_version(),
            "not_covered": NOT_COVERED,
            "state_dir": state_dir.map(|d| d.display().to_string()),
        })
    };
    match posture {
        Posture::NotConfigured => {
            let mut v = base("not_configured");
            v["how_to_enable"] = json!(ENABLE_HINT);
            v
        }
        Posture::DaemonUnreachable { file, reason } => {
            let mut v = base("daemon_unreachable");
            v["daemon_reason"] = json!(one_line(reason));
            v["config"] = match &file.state {
                SharedBudgetFileState::Absent => json!({ "state": "absent" }),
                SharedBudgetFileState::Invalid(why) => {
                    json!({ "state": "invalid", "invalid_reason": one_line(why) })
                }
                SharedBudgetFileState::Valid(cfg) => json!({
                    "state": "valid",
                    "monthly_cap_usd_cents": cfg.monthly_cap_usd_cents,
                    "time_zone": cfg.time_zone.as_str(),
                    "turn_hold_usd_cents": cfg.turn_hold_usd_cents,
                }),
            };
            v
        }
        Posture::Reported(s) => {
            let mut v = base("reported");
            let tz = s
                .config
                .as_ref()
                .and_then(|c| IanaTimeZone::new(&c.time_zone).ok());
            let rec = reconcile(s);
            v["config"] = json!({
                "state": s.config_state,
                "invalid_reason": s.config_invalid_reason,
                "path": s.config_path,
                "permissions_note": s.config_permissions_note,
                "drift": s.config_drift,
                "changed_since_start": s.config_changed_since_start,
                "values": s.config,
            });
            v["period"] = match &s.period {
                Some(p) => json!({
                    "label": period_label(&p.pool_id),
                    "pool_id": p.pool_id,
                    "time_zone": s.config.as_ref().map(|c| c.time_zone.clone()),
                    "start_utc": p.start,
                    "end_utc": p.end,
                    "start_local": local(&p.start, tz.as_ref()),
                    "end_local": local(&p.end, tz.as_ref()),
                }),
                None => Value::Null,
            };
            v["pool_exists"] = json!(s.pool_exists);
            v["capacity_known"] = json!(s.capacity_known);
            v["amounts_usd_cents"] = json!({
                "monthly_cap": s.config.as_ref().map(|c| c.monthly_cap_usd_cents),
                "settled": s.used_usd_cents,
                "held": s.reserved_usd_cents,
                "remaining": s.remaining_usd_cents,
                "overrun": s.remaining_usd_cents.is_some_and(|r| r < 0),
            });
            v["executors"] = json!(s
                .executors
                .iter()
                .map(|e| json!({
                    "session_id": e.session_id,
                    "agent_id": e.agent_id,
                    "kind": e.executor_kind.as_str(),
                    "settled_usd_cents": e.used_usd_cents,
                    "held_usd_cents": e.reserved_usd_cents,
                }))
                .collect::<Vec<_>>());
            v["reconciliation"] = json!({
                "ok": rec.ok(),
                "executors_settled_usd_cents": clamp_u64(rec.executors_used),
                "executors_held_usd_cents": clamp_u64(rec.executors_reserved),
                "pool_settled_usd_cents": rec.pool_used,
                "pool_held_usd_cents": rec.pool_reserved,
                "remaining_consistent": rec.remaining_consistent,
                "discrepancy": rec.discrepancy(),
            });
            v["soft_limits"] = json!({
                "behaviour": "warn-only; never denies admission",
                "warnings": s.warnings.iter().map(|w| json!({
                    "scope": w.scope,
                    "session_id": w.session_id,
                    "agent_id": w.agent_id,
                    "settled_plus_held_usd_cents": w.used_plus_reserved_usd_cents,
                    "threshold_usd_cents": w.threshold_usd_cents,
                })).collect::<Vec<_>>(),
            });
            let mut flagged = serde_json::Map::new();
            for (name, n) in flagged_counts(s) {
                flagged.insert(name.to_string(), json!(n));
            }
            flagged.insert("unpriced_models".to_string(), json!(s.unpriced_models));
            flagged.insert("window".to_string(), json!("since daemon start"));
            v["flagged_settlements"] = Value::Object(flagged);
            v["clock_anomaly"] = json!(s.clock_anomaly);
            v["future_pool_detected"] = json!(s.future_pool_detected);
            v
        }
    }
}

/// Whether a new prompt would be refused for lack of budget: less than one
/// turn hold remains (which includes fully held and overrun pools).
pub fn admission_closed(s: &SharedBudgetStatus) -> bool {
    match (&s.config, s.remaining_usd_cents) {
        (Some(cfg), Some(remaining)) => {
            remaining < i64::try_from(cfg.turn_hold_usd_cents).unwrap_or(i64::MAX)
        }
        _ => false,
    }
}

pub fn state_dir() -> Option<PathBuf> {
    libra_governor_daemon::paths::state_dir().ok()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use libra_governor_domain::ExecutorKind;
    use libra_governor_protocol::{
        SharedBudgetConfigView, SharedBudgetExecutorRow, SharedBudgetPeriod, SHARED_BUDGET_BASIS,
        SHARED_BUDGET_GUARANTEE, SHARED_BUDGET_SCOPE,
    };

    pub(crate) fn row(
        session: &str,
        agent: Option<&str>,
        kind: ExecutorKind,
        used: u64,
        held: u64,
    ) -> SharedBudgetExecutorRow {
        SharedBudgetExecutorRow {
            session_id: session.into(),
            agent_id: agent.map(str::to_owned),
            executor_kind: kind,
            used_usd_cents: used,
            reserved_usd_cents: held,
        }
    }

    pub(crate) fn sample_status() -> SharedBudgetStatus {
        SharedBudgetStatus {
            basis: SHARED_BUDGET_BASIS.into(),
            guarantee: SHARED_BUDGET_GUARANTEE.into(),
            scope: SHARED_BUDGET_SCOPE.into(),
            config_state: SharedBudgetConfigState::Valid,
            config_invalid_reason: None,
            config_drift: SharedBudgetConfigDrift::None,
            config_changed_since_start: false,
            config_path: Some("/s/shared-budget.json".into()),
            config_permissions_note: None,
            config: Some(SharedBudgetConfigView {
                monthly_cap_usd_cents: 50_000,
                time_zone: "America/New_York".into(),
                turn_hold_usd_cents: 1_000,
                soft_limit_per_session_usd_cents: Some(500),
                soft_limit_per_agent_usd_cents: None,
                subagent_unmeasured_charge_usd_cents: None,
                pricing_override_count: 0,
            }),
            clock_anomaly: false,
            future_pool_detected: None,
            period: Some(SharedBudgetPeriod {
                pool_id: "metered:local-budget/monthly-2026-10".into(),
                start: "2026-10-01T04:00:00Z".into(),
                end: "2026-11-01T04:00:00Z".into(),
            }),
            pool_exists: true,
            capacity_known: true,
            used_usd_cents: 230,
            reserved_usd_cents: 1_000,
            remaining_usd_cents: Some(48_770),
            executors: vec![
                row("s1", None, ExecutorKind::AgentAbsent, 100, 1_000),
                row("s1", Some("a1"), ExecutorKind::Agent, 100, 0),
                row("s1", None, ExecutorKind::Unattributed, 30, 0),
            ],
            warnings: vec![],
            unpriced_models: vec![],
            transcript_binding_mismatches: 0,
            transcript_unverified: 0,
            unmeasured_settlements: 0,
            unmeasured_charges: 0,
            id_less_records: 0,
            foreign_lines: 0,
            sweep_deferred_files: 0,
            future_timestamp_settlements: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::sample_status as status;
    use super::*;
    use libra_governor_protocol::SoftLimitWarning;

    #[test]
    fn usd_renders_negative_remaining_honestly() {
        assert_eq!(usd(-1_250), "-$12.50");
        assert_eq!(usd(5), "$0.05");
        assert_eq!(usd_short(50_000), "$500");
        assert_eq!(usd_short(50_025), "$500.25");
    }

    #[test]
    fn reconcile_accepts_matching_rows_and_names_each_discrepancy() {
        assert!(reconcile(&status()).ok());

        let mut bad = status();
        bad.executors[0].used_usd_cents += 1;
        bad.executors[2].reserved_usd_cents += 7;
        let rec = reconcile(&bad);
        assert!(!rec.ok());
        let why = rec.discrepancy().unwrap();
        assert!(why.contains("settled") && why.contains("held"), "{why}");

        let mut bad_remaining = status();
        bad_remaining.remaining_usd_cents = Some(1);
        let rec = reconcile(&bad_remaining);
        assert!(rec.sums_match() && !rec.ok());
        assert!(rec.discrepancy().unwrap().contains("remaining"));
    }

    #[test]
    fn the_segment_shows_committed_spend_cap_and_percent() {
        assert_eq!(
            statusline_segment(&status()).as_deref(),
            Some("budget $12.30/$500 (3%)")
        );
    }

    #[test]
    fn the_segment_carries_warn_for_a_soft_limit_or_an_overrun() {
        let mut warned = status();
        warned.warnings.push(SoftLimitWarning {
            scope: SoftLimitScope::PerSession,
            session_id: "s1".into(),
            agent_id: None,
            used_plus_reserved_usd_cents: 600,
            threshold_usd_cents: 500,
        });
        assert!(statusline_segment(&warned).unwrap().ends_with(" WARN"));

        let mut over = status();
        over.remaining_usd_cents = Some(-1);
        let over_segment = statusline_segment(&over).unwrap();
        assert!(over_segment.contains(" WARN"), "{over_segment}");
    }

    #[test]
    fn the_segment_says_when_new_sessions_would_be_denied() {
        // The healthy fixture ($487.70 left, $10 hold) carries no marker.
        assert!(!statusline_segment(&status()).unwrap().contains("blocked"));

        // $9.99 left is below the $10 hold: not zero, but a prompt is denied.
        let mut short = status();
        short.remaining_usd_cents = Some(999);
        assert_eq!(
            statusline_segment(&short).as_deref(),
            Some("budget $12.30/$500 (3%) · budget low: new sessions blocked")
        );

        // Exactly one hold left still admits.
        let mut exact = status();
        exact.remaining_usd_cents = Some(1_000);
        assert!(!statusline_segment(&exact).unwrap().contains("blocked"));

        // Unusable states keep their terse text.
        let mut anomaly = status();
        anomaly.clock_anomaly = true;
        anomaly.remaining_usd_cents = Some(0);
        assert_eq!(
            statusline_segment(&anomaly).as_deref(),
            Some("budget: clock anomaly")
        );
    }

    #[test]
    fn the_segment_warns_when_nothing_remains_and_rounds_the_percent_up() {
        let mut full = status();
        full.remaining_usd_cents = Some(0);
        let full_segment = statusline_segment(&full).unwrap();
        assert!(full_segment.contains(" WARN"), "{full_segment}");
        assert!(full_segment.ends_with(" · budget low: new sessions blocked"));

        // 1 cent of a $500 cap is 0.002%: shown as 1%, not 0%.
        let mut tiny = status();
        tiny.used_usd_cents = 1;
        tiny.reserved_usd_cents = 0;
        assert!(statusline_segment(&tiny).unwrap().contains("(1%)"));
        // 49,951 of 50,000 is 99.9%: shown as 100%, not 99%.
        let mut almost = status();
        almost.used_usd_cents = 49_951;
        almost.reserved_usd_cents = 0;
        assert!(statusline_segment(&almost).unwrap().contains("(100%)"));
        // No spend stays 0%.
        let mut none = status();
        none.used_usd_cents = 0;
        none.reserved_usd_cents = 0;
        assert!(statusline_segment(&none).unwrap().contains("(0%)"));
    }

    #[test]
    fn the_segment_is_silent_without_a_budget_and_terse_when_unusable() {
        let mut absent = status();
        absent.config_state = SharedBudgetConfigState::Absent;
        assert_eq!(statusline_segment(&absent), None);

        let mut invalid = status();
        invalid.config_state = SharedBudgetConfigState::Invalid;
        assert_eq!(
            statusline_segment(&invalid).as_deref(),
            Some("budget: config invalid")
        );

        let mut anomaly = status();
        anomaly.clock_anomaly = true;
        assert_eq!(
            statusline_segment(&anomaly).as_deref(),
            Some("budget: clock anomaly")
        );
    }

    #[test]
    fn human_status_labels_the_guarantee_units_and_what_is_not_covered() {
        let text = render_human(&Posture::Reported(Box::new(status())), None);
        assert!(text.contains("admission-bounded (cooperative)"), "{text}");
        assert!(text.contains("never a provider bill or quota"), "{text}");
        assert!(text.contains("pricing-"), "{text}");
        assert!(text.contains("Reconciliation: OK"), "{text}");
        assert!(text.contains("unattributed"), "{text}");
        assert!(
            text.contains("2026-10-01T00:00:00-04:00"),
            "zone-local start: {text}"
        );
        for item in NOT_COVERED {
            assert!(text.contains(item), "{item}");
        }
    }

    #[test]
    fn human_status_shows_an_overrun_and_a_reconciliation_discrepancy() {
        let mut s = status();
        s.remaining_usd_cents = Some(-250);
        s.executors[0].used_usd_cents += 5;
        let text = render_human(&Posture::Reported(Box::new(s)), None);
        assert!(text.contains("-$2.50"), "{text}");
        assert!(text.contains("OVERRUN"), "{text}");
        assert!(text.contains("DISCREPANCY"), "{text}");
    }

    #[test]
    fn unknown_capacity_is_surfaced_in_human_and_json_status() {
        let mut s = status();
        s.capacity_known = false;
        let text = render_human(&Posture::Reported(Box::new(s.clone())), None);
        assert!(text.contains("CAPACITY UNKNOWN"), "{text}");
        let v = render_json(&Posture::Reported(Box::new(s)), None);
        assert_eq!(v["capacity_known"], false);
        let known = render_json(&Posture::Reported(Box::new(status())), None);
        assert_eq!(known["capacity_known"], true);
    }

    #[test]
    fn flagged_counts_are_listed_only_when_non_zero() {
        let quiet = render_human(&Posture::Reported(Box::new(status())), None);
        assert!(quiet.contains("Flagged settlements (since daemon start): none"));

        let mut s = status();
        s.future_timestamp_settlements = 2;
        s.foreign_lines = 3;
        s.unpriced_models = vec!["mystery-model".into()];
        let text = render_human(&Posture::Reported(Box::new(s)), None);
        assert!(text.contains("future_timestamp_settlements: 2"), "{text}");
        assert!(text.contains("foreign_lines: 3"), "{text}");
        assert!(text.contains("mystery-model"), "{text}");
        assert!(!text.contains("binding_mismatch:"), "{text}");
    }

    #[test]
    fn not_configured_says_so_and_json_is_marked() {
        let text = render_human(&Posture::NotConfigured, Some(Path::new("/s")));
        assert!(text.starts_with("Shared budget: not configured"), "{text}");
        assert!(
            text.contains("shared-budget init --monthly-usd <n> --time-zone <IANA>"),
            "the how-to-enable line is shown: {text}"
        );
        let v = render_json(&Posture::NotConfigured, None);
        assert_eq!(v["state"], "not_configured");
        assert_eq!(v["how_to_enable"], ENABLE_HINT);
        assert_eq!(v["schema"], STATUS_SCHEMA);
        assert_eq!(v["guarantee"], GUARANTEE_LABEL);
    }

    #[test]
    fn json_reports_reconciliation_and_zone_local_period() {
        let v = render_json(&Posture::Reported(Box::new(status())), None);
        assert_eq!(v["reconciliation"]["ok"], true);
        assert_eq!(v["period"]["start_local"], "2026-10-01T00:00:00-04:00");
        assert_eq!(v["period"]["label"], "monthly-2026-10");
        assert_eq!(v["amounts_usd_cents"]["remaining"], 48_770);
        assert_eq!(v["executors"][2]["kind"], "unattributed");
    }
}
