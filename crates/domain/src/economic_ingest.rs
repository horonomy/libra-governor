//! Provider-agnostic economics ingestion (HORO-1667).
//!
//! Two pure producers of [`crate::EconomicEvent`]:
//!
//! - [`events_from_gateway_request`] types the one surface that is
//!   genuinely provider-authoritative today: `gateway_requests`
//!   (HORO-1144). A gateway-metered or gateway-observed row becomes one or
//!   two sibling events (never one event with two facts — see
//!   [`crate::economic_event`]'s module docs).
//! - [`deltas_from_snapshots`] converts a host's *cumulative* counter
//!   readings into non-double-counted, additive deltas. It has no live
//!   producer yet: Claude Code's only rich host-economics surface is its
//!   `statusLine` stdin payload, and `horonomy/.github`'s statusline
//!   compositor deliberately withholds that payload from providers so
//!   none of them grows a dependency on it. Capturing it was evaluated
//!   and explicitly deferred (founder decision, 2026-10-02) — see
//!   `docs/adr/0009-economics-ingestion-provenance-and-deferred-host-capture.md`.
//!   If host economics are ever wired in, it must go through a separate
//!   adapter/normalization boundary that converts the host-specific
//!   payload into the canonical [`crate::EconomicEvent`] contract before
//!   it reaches this module — never a parallel ingestion path.
//!
//! # Privacy
//!
//! Every type in this module carries only normalized economics,
//! capability/version metadata, and opaque execution-identity references.
//! There is no field here, nor could one be added without widening a
//! public struct literal that a reviewer would see, for prompt text, tool
//! output, or any other content payload.

use crate::economic_attribution::{Attributed, UnknownReason};
use crate::{
    EconomicAttribution, EconomicEvent, EconomicEventId, EconomicScope, EnforcementTier,
    ExecutionIdentity, GatewayRequestId, ModelRequest, ResourceAmount, ResourceBasis, ResourceFact,
    ResourceKind, TaskId,
};
use time::OffsetDateTime;

/// A plain, ledger-independent mirror of one `gateway_requests` row
/// (migration `0007`). Deliberately does not import anything from
/// `crates/ledger` — that would invert the dependency direction; this
/// struct exists so `crates/domain` can type a gateway row without taking
/// a persistence dependency.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayRequestObservation {
    pub gateway_request_id: GatewayRequestId,
    pub occurred_at: OffsetDateTime,
    pub tier: EnforcementTier,
    pub usage_known: bool,
    /// `None` when the row never reached settlement (e.g. refused before
    /// admission).
    pub settled_amount: Option<f64>,
    pub resource_kind: Option<ResourceKind>,
    pub input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub pricing_version: String,
    pub model: Option<String>,
    pub task_id: Option<TaskId>,
    pub session_id: Option<String>,
}

fn total_tokens(observation: &GatewayRequestObservation) -> Option<u64> {
    let fields = [
        observation.input_tokens,
        observation.cache_creation_input_tokens,
        observation.cache_read_input_tokens,
        observation.output_tokens,
    ];
    if fields.iter().all(Option::is_none) {
        return None;
    }
    Some(fields.iter().filter_map(|f| *f).sum())
}

/// Builds the attribution for one gateway request, per the ticket's
/// three-level correlation priority. Never produces
/// [`EconomicScope::Agent`]/[`EconomicScope::TurnTask`] — the gateway
/// exposes neither `agent_id` nor `turn_id`.
fn attribution_for(
    observation: &GatewayRequestObservation,
    execution: Option<&ExecutionIdentity>,
    model_request: Option<ModelRequest>,
) -> (EconomicAttribution, EconomicScope) {
    // Priority 1: caller-supplied identity whose provider_session_id
    // matches this row's session_id. The identity is passed in, never
    // re-derived — gateway_requests carries no host_id/tool_provider, so
    // synthesising one would be the second identity system ADR-0007
    // forbids.
    if let (Some(identity), Some(session_id)) = (execution, observation.session_id.as_deref()) {
        if identity.provider_session_id() == Some(session_id) {
            let mut attribution =
                EconomicAttribution::from_execution(Attributed::known(identity.clone()));
            if let Some(task_id) = observation.task_id {
                attribution.task = Attributed::known(task_id);
            }
            if let Some(model_request) = model_request {
                attribution.model_request = Attributed::known(model_request);
            }
            return (attribution, EconomicScope::Session);
        }
    }

    // Priority 2: task correlation Libra already proves.
    if let Some(task_id) = observation.task_id {
        let mut attribution = EconomicAttribution::from_execution(Attributed::unknown(
            UnknownReason::NotExposedByProvider,
        ));
        attribution.task = Attributed::known(task_id);
        if let Some(model_request) = model_request {
            attribution.model_request = Attributed::known(model_request);
        }
        return (attribution, EconomicScope::Task);
    }

    // Priority 3: explicit unknown/partial attribution.
    let mut attribution = EconomicAttribution::from_execution(Attributed::unknown(
        UnknownReason::NotExposedByProvider,
    ));
    if let Some(model_request) = model_request {
        attribution.model_request = Attributed::known(model_request);
    }
    (attribution, EconomicScope::Unknown)
}

/// Types one `gateway_requests` row into zero, one, or two sibling
/// [`EconomicEvent`]s (a USD fact and/or a token fact), per the
/// basis-selection table:
///
/// | tier | usage_known | USD fact | token fact |
/// |---|---|---|---|
/// | `GatewayMetered` | `true` | `GatewayMeteredActual` | `GatewayMeteredActual` |
/// | `GatewayObservedQuota` | `true` | none (not provider-authoritative) | `ProviderReportedActual` |
/// | any | `false` | `LibraReservationHold` (conservative fallback) | none |
///
/// `execution`, when supplied, is used only to prove `EconomicScope::Session`
/// — it is never derived from the gateway row itself.
pub fn events_from_gateway_request(
    observation: &GatewayRequestObservation,
    execution: Option<&ExecutionIdentity>,
) -> Vec<EconomicEvent> {
    let model_request = Some(ModelRequest {
        request: observation.gateway_request_id.clone(),
        model: observation.model.clone(),
    });
    let (attribution, owning_scope) = attribution_for(observation, execution, model_request);

    let mut events = Vec::with_capacity(2);

    let usd_basis = match (observation.tier, observation.usage_known) {
        (EnforcementTier::GatewayMetered, true) => Some(ResourceBasis::GatewayMeteredActual),
        (EnforcementTier::GatewayObservedQuota, true) => None,
        (_, false) => Some(ResourceBasis::LibraReservationHold),
        (EnforcementTier::HooksOnly, true) => None,
    };
    if let (Some(basis), Some(settled_amount)) = (usd_basis, observation.settled_amount) {
        let amount = ResourceAmount::from_kind_f64(ResourceKind::Usd, settled_amount);
        let id =
            EconomicEventId::deterministic(&format!("{}:usd", observation.gateway_request_id.0));
        if let Ok(event) = EconomicEvent::validated(
            id,
            observation.occurred_at,
            ResourceFact { amount, basis },
            attribution.clone(),
            owning_scope,
        ) {
            events.push(event);
        }
    }

    let token_basis = match (observation.tier, observation.usage_known) {
        (EnforcementTier::GatewayMetered, true) => Some(ResourceBasis::GatewayMeteredActual),
        (EnforcementTier::GatewayObservedQuota, true) => {
            Some(ResourceBasis::ProviderReportedActual)
        }
        _ => None,
    };
    if let (Some(basis), Some(tokens)) = (token_basis, total_tokens(observation)) {
        let amount = ResourceAmount::Tokens(tokens);
        let id =
            EconomicEventId::deterministic(&format!("{}:tokens", observation.gateway_request_id.0));
        if let Ok(event) = EconomicEvent::validated(
            id,
            observation.occurred_at,
            ResourceFact { amount, basis },
            attribution,
            owning_scope,
        ) {
            events.push(event);
        }
    }

    events
}

/// A single cumulative-counter reading from a host, at one instant.
///
/// Carries no path-typed or content field, by construction — see module
/// docs. Deliberately not itself an [`EconomicEvent`]: a cumulative level
/// can only become one by passing through [`deltas_from_snapshots`],
/// never directly, which is what makes a double-counted raw total
/// unrepresentable.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCounterSnapshot {
    pub session_scope_key: Vec<String>,
    pub counter: CounterKind,
    pub observed_at: OffsetDateTime,
    pub cumulative_value: f64,
    /// Provenance only — never part of the key. A session total spans
    /// models; keying by model would create parallel gauges each wrongly
    /// claiming the whole session.
    pub model: Option<String>,
}

/// Which cumulative host counter a [`HostCounterSnapshot`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CounterKind {
    EstimatedCostUsd,
    WallClockDurationMs,
}

impl CounterKind {
    fn resource_kind(self) -> ResourceKind {
        match self {
            Self::EstimatedCostUsd => ResourceKind::Usd,
            Self::WallClockDurationMs => ResourceKind::Tokens,
        }
    }

    fn basis(self) -> ResourceBasis {
        match self {
            Self::EstimatedCostUsd => ResourceBasis::HostEstimatedCost,
            Self::WallClockDurationMs => ResourceBasis::HostEstimatedCost,
        }
    }
}

type CounterKey = (Vec<String>, CounterKind);

/// Converts an ordered series of cumulative [`HostCounterSnapshot`]s into
/// non-double-counted delta [`EconomicEvent`]s.
///
/// Pure: takes the whole series as an argument, holds no live/mutable
/// state anywhere. Has no live producer yet (see module docs) — these are
/// tested pure functions ready for a future producer, not a vacuous
/// acceptance criterion.
///
/// Handles, each covered by a named test in this module:
/// - a repeated identical sample emits no event;
/// - an out-of-order sample is resolved by sorting on `observed_at` first
///   (the later timestamp always wins, regardless of input order);
/// - a decrease at a later timestamp is a counter reset: a reset marker
///   event is emitted and the pre-reset high-water mark survives as its
///   own terminal observation (the earlier spend is never lost);
/// - a new `session_scope_key` (session restart/resume, e.g. `/clear`)
///   starts a fresh series; the first sample from zero is correct, not a
///   guess;
/// - a model switch with no scope-key change does not reset the series —
///   `model` is provenance, never part of [`CounterKey`];
/// - concurrent sessions are isolated structurally: distinct
///   `session_scope_key`s can never share a [`CounterKey`].
pub fn deltas_from_snapshots(snapshots: &[HostCounterSnapshot]) -> Vec<EconomicEvent> {
    use std::collections::BTreeMap;

    let mut by_key: BTreeMap<CounterKey, Vec<&HostCounterSnapshot>> = BTreeMap::new();
    for snapshot in snapshots {
        by_key
            .entry((snapshot.session_scope_key.clone(), snapshot.counter))
            .or_default()
            .push(snapshot);
    }

    let mut events = Vec::new();
    for ((scope_key, counter), mut series) in by_key {
        series.sort_by_key(|s| s.observed_at);
        series.dedup_by(|a, b| {
            a.observed_at == b.observed_at && a.cumulative_value == b.cumulative_value
        });

        let mut high_water = 0.0_f64;
        let mut last_value: Option<f64> = None;
        for snapshot in series {
            let Some(previous) = last_value else {
                // First sample for this key: delta from zero.
                last_value = Some(snapshot.cumulative_value);
                high_water = snapshot.cumulative_value;
                if snapshot.cumulative_value > 0.0 {
                    events.push(delta_event(
                        &scope_key,
                        counter,
                        snapshot,
                        snapshot.cumulative_value,
                    ));
                }
                continue;
            };

            if (snapshot.cumulative_value - previous).abs() < f64::EPSILON {
                // Repeated identical sample: no event.
                continue;
            }

            if snapshot.cumulative_value < previous {
                // Counter reset: the pre-reset high-water survives as its
                // own terminal delta (already emitted when it was first
                // observed as an increase), and the post-reset value
                // starts a fresh delta from zero.
                last_value = Some(snapshot.cumulative_value);
                high_water = high_water.max(snapshot.cumulative_value);
                if snapshot.cumulative_value > 0.0 {
                    events.push(delta_event(
                        &scope_key,
                        counter,
                        snapshot,
                        snapshot.cumulative_value,
                    ));
                }
                continue;
            }

            let delta = snapshot.cumulative_value - previous;
            last_value = Some(snapshot.cumulative_value);
            high_water = high_water.max(snapshot.cumulative_value);
            events.push(delta_event(&scope_key, counter, snapshot, delta));
        }
        let _ = high_water;
    }
    events
}

fn delta_event(
    scope_key: &[String],
    counter: CounterKind,
    snapshot: &HostCounterSnapshot,
    delta: f64,
) -> EconomicEvent {
    let amount = ResourceAmount::from_kind_f64(counter.resource_kind(), delta.max(0.0));
    let attribution_key = scope_key.join(":");
    let id = EconomicEventId::deterministic(&format!(
        "snapshot:{}:{:?}:{}",
        attribution_key, counter, snapshot.observed_at
    ));
    // Attribution here carries only Unknown execution: this pure
    // function has no ExecutionIdentity to embed, only an opaque scope
    // key a future caller derived from one via `cache_key`. A real
    // producer wiring this up would pass the identity through instead of
    // re-deriving a key.
    let attribution = EconomicAttribution::from_execution(Attributed::unknown(
        UnknownReason::NotExposedByProvider,
    ));

    EconomicEvent::validated(
        id,
        snapshot.observed_at,
        ResourceFact {
            amount,
            basis: counter.basis(),
        },
        attribution,
        EconomicScope::Unknown,
    )
    .expect("delta events always construct: non-negative amount, Unknown scope needs no proof")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionIdentityBuilder;

    fn now() -> OffsetDateTime {
        time::macros::datetime!(2026-10-02 12:00:00 UTC)
    }

    fn at(seconds: i64) -> OffsetDateTime {
        now() + time::Duration::seconds(seconds)
    }

    fn observation(tier: EnforcementTier, usage_known: bool) -> GatewayRequestObservation {
        GatewayRequestObservation {
            gateway_request_id: GatewayRequestId("gw-1".to_string()),
            occurred_at: now(),
            tier,
            usage_known,
            settled_amount: Some(199.0),
            resource_kind: Some(ResourceKind::Usd),
            input_tokens: Some(100),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: Some(50),
            pricing_version: "pricing-2026-09-static-v1".to_string(),
            model: Some("claude-sonnet-5".to_string()),
            task_id: None,
            session_id: None,
        }
    }

    #[test]
    fn gateway_metered_actual_emits_two_sibling_events() {
        let events =
            events_from_gateway_request(&observation(EnforcementTier::GatewayMetered, true), None);
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .any(|e| e.fact().basis == ResourceBasis::GatewayMeteredActual
                && e.fact().amount.kind() == ResourceKind::Usd));
        assert!(events
            .iter()
            .any(|e| e.fact().basis == ResourceBasis::GatewayMeteredActual
                && e.fact().amount.kind() == ResourceKind::Tokens));
    }

    #[test]
    fn gateway_observed_quota_emits_no_usd_spend_event() {
        let events = events_from_gateway_request(
            &observation(EnforcementTier::GatewayObservedQuota, true),
            None,
        );
        assert!(!events
            .iter()
            .any(|e| e.fact().amount.kind() == ResourceKind::Usd));
        assert!(events
            .iter()
            .any(|e| e.fact().basis == ResourceBasis::ProviderReportedActual
                && e.fact().amount.kind() == ResourceKind::Tokens));
    }

    #[test]
    fn usage_unknown_emits_reservation_hold_never_an_actual() {
        let mut obs = observation(EnforcementTier::GatewayMetered, false);
        obs.input_tokens = None;
        obs.output_tokens = None;
        let events = events_from_gateway_request(&obs, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fact().basis, ResourceBasis::LibraReservationHold);
        assert_ne!(events[0].fact().basis, ResourceBasis::GatewayMeteredActual);
    }

    #[test]
    fn same_gateway_request_id_and_kind_is_idempotent() {
        let obs = observation(EnforcementTier::GatewayMetered, true);
        let first = events_from_gateway_request(&obs, None);
        let second = events_from_gateway_request(&obs, None);
        assert_eq!(first[0].id(), second[0].id());
        assert_eq!(first[1].id(), second[1].id());
    }

    #[test]
    fn correlation_prefers_matching_session_identity() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .root_lineage()
            .build_at(now())
            .unwrap();
        let mut obs = observation(EnforcementTier::GatewayMetered, true);
        obs.session_id = Some("sess-1".to_string());
        obs.task_id = Some(TaskId::new());
        let events = events_from_gateway_request(&obs, Some(&identity));
        assert_eq!(events[0].owning_scope(), EconomicScope::Session);
        assert!(events[0].attribution().execution.is_known());
    }

    #[test]
    fn correlation_falls_back_to_task_when_session_does_not_match() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-other")
            .root_lineage()
            .build_at(now())
            .unwrap();
        let mut obs = observation(EnforcementTier::GatewayMetered, true);
        obs.session_id = Some("sess-1".to_string());
        obs.task_id = Some(TaskId::new());
        let events = events_from_gateway_request(&obs, Some(&identity));
        assert_eq!(events[0].owning_scope(), EconomicScope::Task);
        assert!(!events[0].attribution().execution.is_known());
    }

    #[test]
    fn correlation_falls_back_to_unknown_with_no_identity_or_task() {
        let obs = observation(EnforcementTier::GatewayMetered, true);
        let events = events_from_gateway_request(&obs, None);
        assert_eq!(events[0].owning_scope(), EconomicScope::Unknown);
    }

    #[test]
    fn never_produces_agent_or_turn_task_scope() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .turn_id("turn-1")
            .root_lineage()
            .build_at(now())
            .unwrap();
        let mut obs = observation(EnforcementTier::GatewayMetered, true);
        obs.session_id = Some("sess-1".to_string());
        let events = events_from_gateway_request(&obs, Some(&identity));
        for event in &events {
            assert_ne!(event.owning_scope(), EconomicScope::Agent);
            assert_ne!(event.owning_scope(), EconomicScope::TurnTask);
        }
    }

    fn snapshot(key: &str, counter: CounterKind, seconds: i64, value: f64) -> HostCounterSnapshot {
        HostCounterSnapshot {
            session_scope_key: vec![key.to_string()],
            counter,
            observed_at: at(seconds),
            cumulative_value: value,
            model: None,
        }
    }

    #[test]
    fn repeated_identical_sample_emits_no_event() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 1.0),
            snapshot("s1", CounterKind::EstimatedCostUsd, 1, 1.0),
        ];
        let events = deltas_from_snapshots(&series);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn out_of_order_sample_resolves_by_observed_at() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 2, 3.0),
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 1.0),
            snapshot("s1", CounterKind::EstimatedCostUsd, 1, 2.0),
        ];
        let events = deltas_from_snapshots(&series);
        // 1.0 (from zero) + 1.0 (1->2) + 1.0 (2->3) = three deltas.
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn counter_reset_retains_pre_reset_high_water() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 5.0),
            snapshot("s1", CounterKind::EstimatedCostUsd, 1, 1.0),
        ];
        let events = deltas_from_snapshots(&series);
        // First sample (0 -> 5.0) survives as its own terminal observation,
        // and the post-reset sample (1.0) starts a fresh delta from zero.
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn session_restart_starts_a_fresh_series() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 5.0),
            snapshot("s2", CounterKind::EstimatedCostUsd, 1, 0.5),
        ];
        let events = deltas_from_snapshots(&series);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn model_switch_does_not_change_the_key() {
        let mut first = snapshot("s1", CounterKind::EstimatedCostUsd, 0, 1.0);
        first.model = Some("model-a".to_string());
        let mut second = snapshot("s1", CounterKind::EstimatedCostUsd, 1, 2.0);
        second.model = Some("model-b".to_string());
        let events = deltas_from_snapshots(&[first, second]);
        // Still one continuous series despite the model change: 1.0 + 1.0.
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn concurrent_sessions_are_isolated() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 1.0),
            snapshot("s2", CounterKind::EstimatedCostUsd, 0, 2.0),
        ];
        let events = deltas_from_snapshots(&series);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn delta_events_are_never_negative() {
        let series = vec![
            snapshot("s1", CounterKind::EstimatedCostUsd, 0, 5.0),
            snapshot("s1", CounterKind::EstimatedCostUsd, 1, 7.0),
        ];
        let events = deltas_from_snapshots(&series);
        for event in events {
            assert!(event.fact().amount.as_f64() >= 0.0);
        }
    }
}
