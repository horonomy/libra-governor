//! Read-only projection from Libra Governor's native ledger
//! ([`libra_governor_ledger::LedgerStore`]) into ADR-0012 §3 evidence
//! events (HORO-1376).
//!
//! # Structural constraint (ADR-0012 §11.4)
//!
//! Libra Governor is `local_only` in v1. This module refuses at
//! config-load time to construct any transport other than `local_only`
//! ([`AdapterConfig::validate`]) — `manual_window`/`monthly_window` are
//! not offered (DFC-ADAPT-08). This module has no dependency edge to
//! `libra-governor-gateway` or any networked crate, and it never touches
//! `crates/cli/src/evidence_report_cmd.rs`.
//!
//! # `actual_action` vs `would_action` (a deliberate deviation from a
//! naive "Deny maps to actual_action=deny" reading)
//!
//! ADR-0012 §3 states plainly: *"A record with `decision_mode=observe`
//! and `actual_action=deny` is malformed by construction."* Since this
//! adapter only ever emits `decision_mode=observe` (Libra Governor's
//! personal-profile v1 scope never enforces), an admission verdict of
//! `Deny`/`ApprovalRequired` can only ever describe what enforcement
//! *would* have done — never what actually happened. So:
//!
//! - `Admission::Admit` → `would_action=allow`, `actual_action=allow`.
//! - `Admission::Deny(_)` → `would_action=deny`; `actual_action` is
//!   `allow` if the plan went on to produce a receipt (the operation
//!   ran — Libra never blocked it), else `no_op`.
//! - `Admission::ApprovalRequired(_)` → `would_action=warn`; same
//!   `actual_action` derivation as `Deny`.
//! - No admission recorded at all (a pre-HORO-1146 row, or a genuinely
//!   unparseable value) → `would_action=None`, `actual_action=no_op`,
//!   `coverage=partial`, `gap_reason=source_unavailable` — this reuses
//!   the same "unrecorded" bucket `EvidenceAggregates::admission_unrecorded_count`
//!   already tracks (see `libra-governor-ledger::query::evidence_aggregates`),
//!   rather than inventing a second one.
//!
//! `actual_action` is never fabricated as `deny` here — no code path in
//! this module can produce that combination, which is exactly what
//! [`crate::event::DogfoodEvent::validate`] checks.

use libra_governor_domain::Admission;
use libra_governor_ledger::{DogfoodPlanRecord, DogfoodReceiptRecord, LedgerStore};
use time::format_description::well_known::Rfc3339;

use crate::canon::seal;
use crate::event::{
    Action, Coverage, DecisionMode, DogfoodEvent, Eligibility, GapReason, PayloadClassification,
    Product, Profile,
};

/// ADR-0012 §2's `transport_policy` axis. Only `LocalOnly` is
/// constructible by [`AdapterConfig::new`] for Libra Governor in v1 —
/// the other two variants exist so [`AdapterConfig::validate`] has a
/// real value to reject rather than an ad-hoc string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPolicy {
    LocalOnly,
    ManualWindow,
    MonthlyWindow,
}

/// Configuration error raised at load time, never at send time — per
/// ADR-0012 §2 ("every illegal combination... is rejected at
/// configuration load, not at send time").
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AdapterConfigError {
    /// DFC-ADAPT-08: Libra Governor does not offer `manual_window` or
    /// `monthly_window` in v1 (ADR-0012 §11.4/§12).
    #[error(
        "transport policy {0:?} is not offered by the Libra Governor DogFood adapter in v1 \
         (ADR-0012 §11.4) — only local_only is supported"
    )]
    WindowTransportNotOfferedInV1(TransportPolicy),
}

/// Adapter configuration. The only field that can vary is
/// `transport_policy`, and [`AdapterConfig::validate`] refuses every
/// value except [`TransportPolicy::LocalOnly`] for this product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    pub transport_policy: TransportPolicy,
}

impl AdapterConfig {
    /// Constructs and validates in one step — there is no way to obtain
    /// an `AdapterConfig` that has not passed [`Self::validate`], so a
    /// caller cannot accidentally skip the config-load-time rejection.
    pub fn new(transport_policy: TransportPolicy) -> Result<Self, AdapterConfigError> {
        let config = Self { transport_policy };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), AdapterConfigError> {
        match self.transport_policy {
            TransportPolicy::LocalOnly => Ok(()),
            other => Err(AdapterConfigError::WindowTransportNotOfferedInV1(other)),
        }
    }
}

/// Errors this crate's read path can raise. Wraps the ledger's own error
/// type rather than re-deriving one — this crate adds no new storage
/// layer of its own.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("ledger error: {0}")]
    Ledger(#[from] libra_governor_ledger::LedgerError),
    #[error("config error: {0}")]
    Config(#[from] AdapterConfigError),
}

fn parse_origin_profile(raw: Option<&str>) -> Profile {
    match raw {
        Some("corporate") => Profile::Corporate,
        _ => Profile::Personal,
    }
}

/// The live profile setting, read the same way
/// `libra_governor_ledger`'s capture path reads it — but this is a
/// *separate* read for the schema's separate `profile` field (ADR-0012
/// distinguishes `profile`, the current live setting, from
/// `origin_profile`, the immutable capture-time value).
pub fn current_profile() -> Profile {
    match std::env::var(libra_governor_ledger::DOGFOOD_PROFILE_ENV_VAR) {
        Ok(v) if v == "corporate" => Profile::Corporate,
        _ => Profile::Personal,
    }
}

fn format_time(t: Option<time::OffsetDateTime>) -> String {
    t.and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|_| "unknown".to_string())
}

/// Builds one evidence event from a plan/admission record (ADR-0012 §7:
/// policy-decision evidence is `replayable_evidence`).
fn build_plan_event(
    rec: &DogfoodPlanRecord,
    product_version: &str,
    adapter_version: &str,
) -> DogfoodEvent {
    let origin_profile = parse_origin_profile(rec.origin_profile.as_deref());
    let profile = current_profile();

    let (would_action, actual_action, coverage, gap_reason) = match &rec.admission {
        None => (
            None,
            Action::NoOp,
            Coverage::Partial,
            Some(GapReason::SourceUnavailable),
        ),
        Some(Admission::Admit) => (Some(Action::Allow), Action::Allow, Coverage::Full, None),
        Some(Admission::Deny(_)) => {
            let actual = if rec.has_receipt {
                Action::Allow
            } else {
                Action::NoOp
            };
            (Some(Action::Deny), actual, Coverage::Full, None)
        }
        Some(Admission::ApprovalRequired(_)) => {
            let actual = if rec.has_receipt {
                Action::Allow
            } else {
                Action::NoOp
            };
            (Some(Action::Warn), actual, Coverage::Full, None)
        }
    };

    let event = DogfoodEvent {
        event_id: rec.event_id.clone(),
        schema_version: 1,
        product: Product::LibraGovernor,
        product_version: product_version.to_string(),
        adapter_version: adapter_version.to_string(),
        occurred_at: rfc3339(rec.occurred_at),
        ingested_at: format_time(rec.ingested_at),
        profile,
        origin_profile,
        decision_mode: DecisionMode::Observe,
        scope_id: None,
        actual_action,
        would_action,
        coverage,
        gap_reason,
        dropped_count: 0,
        payload_classification: PayloadClassification::MetadataOnly,
        integrity: crate::canon::placeholder_integrity(),
        destination: Some("local_only".to_string()),
        tenant_id: None,
        transport_state: crate::event::TransportState::Pending,
        eligibility: Eligibility::ReplayableEvidence,
        permanently_ineligible: origin_profile == Profile::Corporate,
        imported: false,
    };
    seal(event)
}

/// Builds one evidence event from an execution receipt (ADR-0012 §7.1:
/// tool/command execution records are `non_replayable_operation`, never
/// transferred — a fact this adapter can state truthfully since it never
/// transfers anything at all).
fn build_receipt_event(
    rec: &DogfoodReceiptRecord,
    product_version: &str,
    adapter_version: &str,
) -> DogfoodEvent {
    let origin_profile = parse_origin_profile(rec.origin_profile.as_deref());
    let profile = current_profile();

    let event = DogfoodEvent {
        event_id: rec.event_id.clone(),
        schema_version: 1,
        product: Product::LibraGovernor,
        product_version: product_version.to_string(),
        adapter_version: adapter_version.to_string(),
        occurred_at: rfc3339(rec.occurred_at),
        ingested_at: format_time(rec.ingested_at),
        profile,
        origin_profile,
        decision_mode: DecisionMode::Observe,
        scope_id: None,
        // A receipt is, by definition, an operation that actually ran.
        actual_action: Action::Allow,
        would_action: Some(Action::Allow),
        coverage: Coverage::Full,
        gap_reason: None,
        dropped_count: 0,
        payload_classification: PayloadClassification::MetadataOnly,
        integrity: crate::canon::placeholder_integrity(),
        destination: Some("local_only".to_string()),
        tenant_id: None,
        transport_state: crate::event::TransportState::Pending,
        eligibility: Eligibility::NonReplayableOperation,
        permanently_ineligible: origin_profile == Profile::Corporate,
        imported: false,
    };
    seal(event)
}

/// Projects every plan/admission record and every receipt in `store`
/// into evidence events, validated against ADR-0012 §3's structural
/// invariants before being returned. `config` is accepted (and
/// validated again defensively) even though `local_only` changes nothing
/// about *which* events are built — it exists so a caller cannot get an
/// events list without having gone through [`AdapterConfig::validate`]
/// first.
pub fn build_events(
    store: &LedgerStore,
    config: &AdapterConfig,
    product_version: &str,
    adapter_version: &str,
) -> Result<Vec<DogfoodEvent>, AdapterError> {
    config.validate()?;

    let mut events = Vec::new();
    for rec in store.dogfood_plan_records()? {
        events.push(build_plan_event(&rec, product_version, adapter_version));
    }
    for rec in store.dogfood_receipt_records()? {
        events.push(build_receipt_event(&rec, product_version, adapter_version));
    }
    for event in &events {
        event
            .validate()
            .unwrap_or_else(|e| panic!("adapter produced a malformed event: {e}"));
    }
    Ok(events)
}

/// Coarse, queryable overflow/gap summary (DFC-RETN-02: "operator/
/// dashboard-facing status reports gap count, `dropped_count`... these
/// must be real queryable values, not hardcoded zero"). Since this
/// adapter has no row-cap/eviction path anywhere in the ledger it reads
/// from (no `DELETE FROM` exists in `crates/ledger/src/**` or its
/// migrations — see the adapter fixture test that greps for this), a
/// real run's `dropped_total` is always genuinely `0`, and this function
/// computes that from the actual events rather than hardcoding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GapSummary {
    pub gap_count: usize,
    pub dropped_total: u64,
}

pub fn summarize(events: &[DogfoodEvent]) -> GapSummary {
    let mut summary = GapSummary::default();
    for event in events {
        if event.coverage != Coverage::Full {
            summary.gap_count += 1;
        }
        summary.dropped_total += event.dropped_count;
    }
    summary
}
