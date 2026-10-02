//! [`EconomicEvent`] — one immutable, canonical economic fact, recorded
//! exactly once at its narrowest truthful owning scope.
//!
//! An event carries exactly one [`ResourceFact`] (not a `Vec`): a gateway
//! request that yields both a USD cost and a token count emits two sibling
//! events sharing one [`crate::ModelRequest`] dimension, rather than one
//! event with two facts. This is deliberate — it makes "recorded exactly
//! once" literally the event-id dedup in
//! [`crate::economic_rollup`], and makes every total trivially per-
//! [`crate::ResourceKind`] so cross-unit mixing is unrepresentable. It is
//! not double counting: the two sibling events measure two different
//! resources of the same request, not the same resource twice.

use crate::economic_attribution::is_supported_contract_version;
use crate::{EconomicAttribution, ResourceAmount, ECONOMIC_ATTRIBUTION_CONTRACT_VERSION};
use time::OffsetDateTime;
use uuid::Uuid;

/// The idempotency key for one [`EconomicEvent`]. Every rollup function in
/// [`crate::economic_rollup`] de-duplicates by this id before summing, so
/// replaying the same event twice can never double count.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct EconomicEventId(pub Uuid);

impl EconomicEventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EconomicEventId {
    fn default() -> Self {
        Self::new()
    }
}

/// What a resource value means, and where it came from — one closed enum,
/// not a (meaning × source) matrix needing cross-field validation. These
/// are the six classes HORO-1666 enumerates, plus the gateway/provider
/// actual split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceBasis {
    /// Libra's own gateway metered this request directly (HORO-1144) —
    /// the strongest truth strength available.
    GatewayMeteredActual,
    /// The provider's own usage/billing response reported this value.
    ProviderReportedActual,
    /// The agent host itself reported an estimated cost figure.
    HostEstimatedCost,
    /// A point-in-time level of a subscription period quota consumed.
    /// Never additive — a quota reading is a gauge, not a delta.
    QuotaSnapshot,
    /// Capacity Libra has reserved (claimed), not yet spent.
    LibraReservationHold,
    /// Libra's own estimator output.
    LibraForecast,
    /// A limit or allocation imported from outside Libra. Typed now,
    /// produced later: v0.0.3 has no importer for this basis yet, but the
    /// acceptance criteria require the dimension to exist so a future
    /// importer has a truthful place to put its data without widening the
    /// contract.
    ImportedAllocationSnapshot,
}

/// What role a [`ResourceBasis`] plays in a spend/budget computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactRole {
    /// Resource actually consumed.
    Spend,
    /// Capacity claimed but not (yet) consumed.
    Hold,
    /// A forward-looking estimate, never itself spend.
    Projection,
    /// An external limit/allocation value, neither spend nor a Libra hold.
    Reference,
}

/// Relative confidence in a [`ResourceBasis`]'s truthfulness. Ordered so
/// rollups can report the single weakest basis contributing to a total
/// (`Metered` is strongest).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TruthStrength {
    Claimed,
    Estimated,
    Reported,
    Metered,
}

impl ResourceBasis {
    pub const fn role(self) -> FactRole {
        match self {
            Self::GatewayMeteredActual | Self::ProviderReportedActual => FactRole::Spend,
            Self::HostEstimatedCost | Self::LibraForecast => FactRole::Projection,
            Self::QuotaSnapshot | Self::ImportedAllocationSnapshot => FactRole::Reference,
            Self::LibraReservationHold => FactRole::Hold,
        }
    }

    pub const fn truth_strength(self) -> TruthStrength {
        match self {
            Self::GatewayMeteredActual => TruthStrength::Metered,
            Self::ProviderReportedActual => TruthStrength::Reported,
            Self::HostEstimatedCost | Self::LibraForecast => TruthStrength::Estimated,
            Self::QuotaSnapshot | Self::LibraReservationHold | Self::ImportedAllocationSnapshot => {
                TruthStrength::Claimed
            }
        }
    }

    /// Whether two facts on this basis may be summed. `false` for
    /// [`Self::QuotaSnapshot`] and [`Self::ImportedAllocationSnapshot`]:
    /// both are *levels* (a gauge reading, a configured ceiling), never
    /// deltas — summing two quota-percent readings or two allocation
    /// ceilings produces a meaningless number. This is the structural
    /// reason estimate/actual/limit/quota can never collapse into one
    /// summed "cost" field: additivity is a property of the *basis*, not
    /// of the [`crate::ResourceKind`] unit it happens to be measured in.
    pub const fn is_additive(self) -> bool {
        !matches!(self, Self::QuotaSnapshot | Self::ImportedAllocationSnapshot)
    }
}

/// A resource measurement paired with its [`ResourceBasis`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResourceFact {
    pub amount: ResourceAmount,
    pub basis: ResourceBasis,
}

/// The execution containment chain an economic fact's scope may be fixed
/// at, narrowest first, plus the Libra-only dimensions that sit outside
/// that chain.
///
/// # Relationship to [`crate::Scope`]
///
/// Related, not interchangeable — see
/// `docs/adr/0007-economic-attribution-vs-execution-identity.md`,
/// "Relationship to the shared contract's `Scope`". This enum adds
/// [`Self::ModelRequest`], [`Self::Task`], [`Self::Principal`], and
/// [`Self::Organization`] (dimensions [`crate::Scope`] has no concept of);
/// it drops `ProjectWorktree` (a repo/worktree filters a query, it never
/// owns spend — the shared contract says so explicitly). [`Self::Unknown`]
/// is a permanent legal member here too, for the same reason: pre-
/// attribution legacy data is real and permanent.
///
/// Deliberately has **no** `Ord` impl: a task spans sessions while a
/// session maps to one task — overlapping, not nested — so there is no
/// total order across every variant. [`EXECUTION_CHAIN`] gives the only
/// ordering that exists (the proven-containment chain), used solely by
/// [`crate::EconomicAttribution::narrowest_proven_execution_scope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EconomicScope {
    ModelRequest,
    TurnTask,
    Agent,
    Session,
    Host,
    Task,
    Principal,
    Organization,
    Unknown,
}

/// The execution-identity containment chain, narrowest to broadest.
/// [`EconomicScope::Host`] is a legal owning scope for a spend fact (a
/// real cost with no session proven) — it is never "tidied" into
/// reference-only, which would silently drop real spend.
pub const EXECUTION_CHAIN: [EconomicScope; 4] = [
    EconomicScope::TurnTask,
    EconomicScope::Agent,
    EconomicScope::Session,
    EconomicScope::Host,
];

impl EconomicScope {
    /// Whether `attribution` actually proves this scope: for the four
    /// execution-chain members, delegates to
    /// [`crate::ExecutionIdentity::cache_key`] (fail-closed, never
    /// widened); [`Self::Task`]/[`Self::Plan`-less `ModelRequest`]/
    /// [`Self::Principal`]/[`Self::Organization`] check the corresponding
    /// [`crate::EconomicAttribution`] dimension directly.
    pub fn is_proven_by(self, attribution: &EconomicAttribution) -> bool {
        use crate::economic_attribution::EconomicDimension as Dim;
        match self {
            Self::TurnTask => attribution.dimension_key(Dim::TurnTask).is_some(),
            Self::Agent => attribution.dimension_key(Dim::Agent).is_some(),
            Self::Session => attribution.dimension_key(Dim::Session).is_some(),
            Self::Host => attribution.execution.value().is_some(),
            Self::ModelRequest => attribution.dimension_key(Dim::ModelRequest).is_some(),
            Self::Task => attribution.dimension_key(Dim::Task).is_some(),
            Self::Principal => attribution.dimension_key(Dim::Principal).is_some(),
            Self::Organization => attribution.dimension_key(Dim::Organization).is_some(),
            Self::Unknown => true,
        }
    }
}

/// Why constructing an [`EconomicEvent`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EconomicEventError {
    #[error("unsupported contract_version: {0}")]
    UnsupportedContractVersion(i64),
    #[error("occurred_at must be UTC, got offset {0}")]
    OccurredAtNotUtc(time::UtcOffset),
    #[error("owning_scope {scope:?} is not proven by the given attribution")]
    ScopeNotProven { scope: EconomicScope },
    #[error("resource amount must be non-negative")]
    NegativeAmount,
}

fn is_negative(amount: &ResourceAmount) -> bool {
    amount.as_f64() < 0.0
}

/// One immutable economic fact, attributed to its narrowest truthful
/// owning scope. Construct only via [`EconomicEvent::validated`] so the
/// version/UTC/scope-proof/non-negative checks can never be bypassed by a
/// struct literal — the same discipline [`crate::ExecutionIdentity`]
/// applies to itself.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EconomicEvent {
    contract_version: i64,
    id: EconomicEventId,
    #[serde(with = "occurred_at_wire")]
    occurred_at: OffsetDateTime,
    fact: ResourceFact,
    attribution: EconomicAttribution,
    owning_scope: EconomicScope,
}

impl EconomicEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn validated(
        id: EconomicEventId,
        occurred_at: OffsetDateTime,
        fact: ResourceFact,
        attribution: EconomicAttribution,
        owning_scope: EconomicScope,
    ) -> Result<Self, EconomicEventError> {
        if !is_supported_contract_version(ECONOMIC_ATTRIBUTION_CONTRACT_VERSION) {
            return Err(EconomicEventError::UnsupportedContractVersion(
                ECONOMIC_ATTRIBUTION_CONTRACT_VERSION,
            ));
        }
        if occurred_at.offset() != time::UtcOffset::UTC {
            return Err(EconomicEventError::OccurredAtNotUtc(occurred_at.offset()));
        }
        if is_negative(&fact.amount) {
            return Err(EconomicEventError::NegativeAmount);
        }
        // A pre-attribution record proves nothing but Unknown; any other
        // scope must be genuinely proven by the attribution. A *broader*
        // than proven scope is legal (e.g. a real host-wide quota fact);
        // a narrower one is refused.
        if owning_scope != EconomicScope::Unknown && !owning_scope.is_proven_by(&attribution) {
            return Err(EconomicEventError::ScopeNotProven {
                scope: owning_scope,
            });
        }

        Ok(Self {
            contract_version: ECONOMIC_ATTRIBUTION_CONTRACT_VERSION,
            id,
            occurred_at,
            fact,
            attribution,
            owning_scope,
        })
    }

    pub fn id(&self) -> EconomicEventId {
        self.id
    }

    pub fn occurred_at(&self) -> OffsetDateTime {
        self.occurred_at
    }

    pub fn fact(&self) -> ResourceFact {
        self.fact
    }

    pub fn attribution(&self) -> &EconomicAttribution {
        &self.attribution
    }

    pub fn owning_scope(&self) -> EconomicScope {
        self.owning_scope
    }

    pub fn contract_version(&self) -> i64 {
        self.contract_version
    }
}

/// `occurred_at`'s wire format, matching
/// [`crate::ExecutionIdentity`]'s own `observed_at` wire format exactly
/// (`YYYY-MM-DDTHH:MM:SS.fffZ`) so both timestamps round-trip through the
/// same JSON shape.
mod occurred_at_wire {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(
        value: &OffsetDateTime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let millis = value.millisecond();
        let truncated = value
            .replace_millisecond(millis)
            .map_err(serde::ser::Error::custom)?;
        let formatted = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            truncated.year(),
            u8::from(truncated.month()),
            truncated.day(),
            truncated.hour(),
            truncated.minute(),
            truncated.second(),
            millis,
        );
        serializer.serialize_str(&formatted)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let rfc3339 = raw
            .strip_suffix('Z')
            .map(|s| format!("{s}+00:00"))
            .unwrap_or(raw);
        OffsetDateTime::parse(&rfc3339, &Rfc3339).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economic_attribution::Attributed;
    use crate::{ExecutionIdentityBuilder, ResourceKind};

    fn now() -> OffsetDateTime {
        time::macros::datetime!(2026-10-02 12:00:00 UTC)
    }

    fn attribution_with_session() -> EconomicAttribution {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .root_lineage()
            .build_at(now())
            .unwrap();
        EconomicAttribution::from_execution(Attributed::known(identity))
    }

    fn fact(amount: ResourceAmount, basis: ResourceBasis) -> ResourceFact {
        ResourceFact { amount, basis }
    }

    #[test]
    fn valid_event_constructs() {
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::UsdCents(100),
                ResourceBasis::GatewayMeteredActual,
            ),
            attribution_with_session(),
            EconomicScope::Session,
        );
        assert!(event.is_ok());
    }

    #[test]
    fn scope_not_proven_is_refused() {
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::UsdCents(100),
                ResourceBasis::GatewayMeteredActual,
            ),
            attribution_with_session(),
            EconomicScope::Agent,
        );
        assert_eq!(
            event.unwrap_err(),
            EconomicEventError::ScopeNotProven {
                scope: EconomicScope::Agent
            }
        );
    }

    #[test]
    fn broader_than_proven_scope_is_legal() {
        // A real host-wide fact with only session proven is legal: Host is
        // broader than Session in the execution chain, and is_proven_by
        // for Host only requires execution identity to exist at all.
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::QuotaPercent(50.0),
                ResourceBasis::QuotaSnapshot,
            ),
            attribution_with_session(),
            EconomicScope::Host,
        );
        assert!(event.is_ok());
    }

    #[test]
    fn negative_amount_is_refused() {
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::UsdCents(-1),
                ResourceBasis::GatewayMeteredActual,
            ),
            attribution_with_session(),
            EconomicScope::Session,
        );
        assert_eq!(event.unwrap_err(), EconomicEventError::NegativeAmount);
    }

    #[test]
    fn non_utc_occurred_at_is_refused() {
        let non_utc = now().to_offset(time::macros::offset!(+8));
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            non_utc,
            fact(
                ResourceAmount::UsdCents(1),
                ResourceBasis::GatewayMeteredActual,
            ),
            attribution_with_session(),
            EconomicScope::Session,
        );
        assert!(matches!(
            event,
            Err(EconomicEventError::OccurredAtNotUtc(_))
        ));
    }

    #[test]
    fn pre_attribution_record_is_only_legal_at_unknown_scope() {
        let record = EconomicAttribution::pre_attribution_record();
        let legal = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::UsdCents(1),
                ResourceBasis::GatewayMeteredActual,
            ),
            record.clone(),
            EconomicScope::Unknown,
        );
        assert!(legal.is_ok());

        let illegal = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::UsdCents(1),
                ResourceBasis::GatewayMeteredActual,
            ),
            record,
            EconomicScope::Session,
        );
        assert!(illegal.is_err());
    }

    #[test]
    fn quota_snapshot_and_imported_allocation_are_not_additive() {
        assert!(!ResourceBasis::QuotaSnapshot.is_additive());
        assert!(!ResourceBasis::ImportedAllocationSnapshot.is_additive());
        assert!(ResourceBasis::GatewayMeteredActual.is_additive());
    }

    #[test]
    fn reservation_hold_and_forecast_are_never_spend_role() {
        assert_ne!(ResourceBasis::LibraReservationHold.role(), FactRole::Spend);
        assert_ne!(ResourceBasis::LibraForecast.role(), FactRole::Spend);
        assert_eq!(ResourceBasis::GatewayMeteredActual.role(), FactRole::Spend);
        assert_eq!(
            ResourceBasis::ProviderReportedActual.role(),
            FactRole::Spend
        );
    }

    #[test]
    fn truth_strength_orders_metered_above_claimed() {
        assert!(
            ResourceBasis::GatewayMeteredActual.truth_strength()
                > ResourceBasis::ImportedAllocationSnapshot.truth_strength()
        );
    }

    #[test]
    fn serde_round_trip_preserves_event() {
        let event = EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            fact(
                ResourceAmount::Tokens(500),
                ResourceBasis::ProviderReportedActual,
            ),
            attribution_with_session(),
            EconomicScope::Session,
        )
        .unwrap();
        let json = serde_json::to_string(&event).unwrap();
        let restored: EconomicEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, restored);
    }

    #[test]
    fn unrecognized_scope_is_refused_not_guessed() {
        let result: Result<EconomicScope, _> = serde_json::from_value(serde_json::json!("galaxy"));
        assert!(result.is_err());
    }

    #[test]
    fn unrecognized_basis_is_refused_not_guessed() {
        let result: Result<ResourceBasis, _> = serde_json::from_value(serde_json::json!("made_up"));
        assert!(result.is_err());
    }

    #[test]
    fn fact_amount_kind_is_tokens() {
        assert_eq!(
            fact(ResourceAmount::Tokens(1), ResourceBasis::LibraForecast)
                .amount
                .kind(),
            ResourceKind::Tokens
        );
    }
}
