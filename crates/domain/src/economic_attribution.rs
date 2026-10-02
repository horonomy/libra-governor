//! [`EconomicAttribution`] — Libra's own "who owns this economic fact"
//! contract, built on top of (never duplicating) the shared
//! [`crate::ExecutionIdentity`] envelope (HORO-1597/1598/1599).
//!
//! # Boundary with [`crate::ExecutionIdentity`]
//!
//! The shared envelope is the sole source of host, tool/provider, provider
//! session, agent, turn, and parent-agent lineage identity. This module
//! never re-declares, re-derives, reformats, or backfills any of those
//! dimensions — [`EconomicAttribution`] embeds the envelope verbatim and
//! reads those dimensions through accessors. Libra owns, and the shared
//! contract does not: economic-event identity (see
//! [`crate::economic_event`]), resource-fact provenance, exclusive/inclusive
//! aggregation semantics (see [`crate::economic_rollup`]), and the
//! task/plan/model-request/organization/principal dimensions below. See
//! `docs/adr/0007-economic-attribution-vs-execution-identity.md`.
//!
//! # Amendment to HORO-1666's ticket text
//!
//! The ticket states the shared envelope "owns org/principal context when
//! authoritatively available." It does not: envelope v1 has exactly 13
//! fields, none of them org/principal/tenant. `organization`/`principal`
//! below are Libra's own operator-configured dimensions instead, and are
//! always [`UnknownReason::NotConfigured`] today — v0.0.3 has no
//! organization/principal config surface (Team SaaS/control plane is an
//! explicit non-goal). See the ADR for the full amendment.

use crate::{ExecutionIdentity, LineageStatus, PlanId, Scope, TaskId};

/// Traceability tag for this contract's shape, mirroring
/// [`crate::EXECUTION_IDENTITY_ENVELOPE_VERSION`]'s convention.
pub const ECONOMIC_ATTRIBUTION_CONTRACT_VERSION: i64 = 1;

pub(crate) fn is_supported_contract_version(version: i64) -> bool {
    version == ECONOMIC_ATTRIBUTION_CONTRACT_VERSION
}

/// Why a dimension is absent on a particular attribution.
///
/// Never collapsed into a bare `None`: "the provider doesn't expose this",
/// "nobody configured this", and "this row predates attribution" are three
/// different truths a reader needs to tell apart, and conflating any two of
/// them is exactly the kind of guessed precision this contract forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    /// The execution's provider has no concept of this dimension.
    NotExposedByProvider,
    /// Libra has the concept, but no operator has configured a value for
    /// it. The permanent state of `organization`/`principal` in v0.0.3.
    NotConfigured,
    /// This record was written before this attribution contract existed.
    /// Permanent — never guessed-backfilled.
    PreAttributionRecord,
}

/// An availability-explicit dimension value.
///
/// Deliberately not `Option<T>`: `None` cannot distinguish "the provider
/// doesn't expose it" from "nobody configured it" from "this row predates
/// attribution" — see [`UnknownReason`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum Attributed<T> {
    Known { value: T },
    Unknown { reason: UnknownReason },
}

impl<T> Attributed<T> {
    pub fn known(value: T) -> Self {
        Self::Known { value }
    }

    pub fn unknown(reason: UnknownReason) -> Self {
        Self::Unknown { reason }
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Known { value } => Some(value),
            Self::Unknown { .. } => None,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }
}

/// An opaque, operator-configured organization identifier.
///
/// Libra's own dimension — not part of the shared execution identity
/// envelope. Always [`UnknownReason::NotConfigured`] in v0.0.3: there is no
/// organization config surface (Team SaaS/control plane is a non-goal).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct OrganizationId(pub String);

/// An opaque, operator-configured principal (person) identifier. See
/// [`OrganizationId`] — same status, same reason.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(pub String);

/// A reference to the gateway request this economic fact concerns.
///
/// Opaque newtype over the gateway's own request id (`gateway_requests.id`
/// in the ledger schema) plus an optional model name — kept here rather
/// than in the ledger crate so attribution types have no persistence
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct GatewayRequestId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelRequest {
    pub request: GatewayRequestId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
}

/// One dimension of economic ownership. Used to select which projection
/// [`crate::economic_rollup::project`] groups by, and to look up a
/// dimension's static [`DimensionSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EconomicDimension {
    Organization,
    Principal,
    Task,
    Session,
    Agent,
    TurnTask,
    Plan,
    ModelRequest,
}

impl EconomicDimension {
    /// Which system is authoritative for this dimension's identity. A
    /// static property of *which dimension this is* — a session id can
    /// only ever be provider-native in Libra — never a per-row field.
    pub const fn source(self) -> DimensionSource {
        match self {
            Self::Session | Self::Agent | Self::TurnTask => DimensionSource::ProviderNative,
            Self::Task | Self::Plan | Self::ModelRequest => DimensionSource::LibraInternal,
            Self::Organization | Self::Principal => DimensionSource::OperatorConfigured,
        }
    }
}

/// Which system is authoritative for a dimension's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionSource {
    /// Carried verbatim from the shared [`ExecutionIdentity`] envelope.
    ProviderNative,
    /// Correlated by Horonom session-lineage, not providers themselves.
    HoronomCorrelation,
    /// Minted and owned entirely inside Libra (e.g. [`TaskId`], [`PlanId`]).
    LibraInternal,
    /// An operator configures this value; no provider or Horonom
    /// correlation produces it.
    OperatorConfigured,
}

/// The minimal identity tuple a dimension's grouping key is keyed on —
/// what [`ExecutionIdentity::cache_key`] returns, wrapped so it can key a
/// `BTreeMap` and never be confused with a raw provider id.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct DimensionKey(pub Vec<String>);

/// Whether an agent's parent-agent relationship is proven.
///
/// Mirrors [`LineageStatus`]'s tri-state discipline at the attribution
/// layer: [`ProvenParent::Child`] is returned only when the embedded
/// envelope proves it, never reconstructed from timing or process
/// ancestry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenParent {
    Root,
    Child(DimensionKey),
    Unknown,
}

/// Libra's full economic-ownership envelope for one [`crate::EconomicEvent`].
///
/// Embeds [`ExecutionIdentity`] verbatim (see module docs) and adds
/// exactly the dimensions Libra itself owns: task, plan, model/provider
/// request, and the operator-configured organization/principal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EconomicAttribution {
    pub execution: Attributed<ExecutionIdentity>,
    pub task: Attributed<TaskId>,
    pub plan: Attributed<PlanId>,
    pub model_request: Attributed<ModelRequest>,
    pub organization: Attributed<OrganizationId>,
    pub principal: Attributed<PrincipalId>,
}

impl EconomicAttribution {
    /// Constructs an attribution with no task/plan/model-request known yet,
    /// and `organization`/`principal` set to their v0.0.3 permanent status.
    /// Callers that do know task/plan/model-request should set those
    /// fields via the public struct literal (all fields are public) rather
    /// than mutate this helper's output in place.
    pub fn from_execution(execution: Attributed<ExecutionIdentity>) -> Self {
        Self {
            execution,
            task: Attributed::unknown(UnknownReason::NotExposedByProvider),
            plan: Attributed::unknown(UnknownReason::NotExposedByProvider),
            model_request: Attributed::unknown(UnknownReason::NotExposedByProvider),
            organization: Attributed::unknown(UnknownReason::NotConfigured),
            principal: Attributed::unknown(UnknownReason::NotConfigured),
        }
    }

    /// A pre-attribution record: no dimension is known, including
    /// execution identity itself. Used by callers importing rows that
    /// predate this contract. Never guessed-backfilled from any other
    /// field.
    pub fn pre_attribution_record() -> Self {
        Self {
            execution: Attributed::unknown(UnknownReason::PreAttributionRecord),
            task: Attributed::unknown(UnknownReason::PreAttributionRecord),
            plan: Attributed::unknown(UnknownReason::PreAttributionRecord),
            model_request: Attributed::unknown(UnknownReason::PreAttributionRecord),
            organization: Attributed::unknown(UnknownReason::PreAttributionRecord),
            principal: Attributed::unknown(UnknownReason::PreAttributionRecord),
        }
    }

    /// The grouping key for `dim`, or `None` when this attribution is not
    /// attributed on that dimension. For [`EconomicDimension::Session`],
    /// [`EconomicDimension::Agent`], and [`EconomicDimension::TurnTask`]
    /// this delegates to [`ExecutionIdentity::cache_key`] — it never
    /// builds an independent `(host_id, tool_provider, agent_id)` tuple of
    /// its own, since a provider-native `agent_id` is opaque and unique
    /// only within its session, and re-deriving the key would be exactly
    /// the second identity system this contract forbids.
    pub fn dimension_key(&self, dim: EconomicDimension) -> Option<DimensionKey> {
        match dim {
            EconomicDimension::Organization => self
                .organization
                .value()
                .map(|v| DimensionKey(vec![v.0.clone()])),
            EconomicDimension::Principal => self
                .principal
                .value()
                .map(|v| DimensionKey(vec![v.0.clone()])),
            EconomicDimension::Task => self.task.value().map(|v| DimensionKey(vec![v.to_string()])),
            EconomicDimension::Plan => self
                .plan
                .value()
                .map(|v| DimensionKey(vec![v.0.to_string()])),
            EconomicDimension::ModelRequest => self.model_request.value().map(|v| {
                DimensionKey(vec![
                    v.request.0.clone(),
                    v.model.clone().unwrap_or_default(),
                ])
            }),
            EconomicDimension::Session => self
                .execution
                .value()
                .and_then(|id| id.cache_key(Scope::Session).ok())
                .map(DimensionKey),
            EconomicDimension::Agent => self
                .execution
                .value()
                .and_then(|id| id.cache_key(Scope::Agent).ok())
                .map(DimensionKey),
            EconomicDimension::TurnTask => self
                .execution
                .value()
                .and_then(|id| id.cache_key(Scope::TurnTask).ok())
                .map(DimensionKey),
        }
    }

    /// Proven parent-agent lineage, read verbatim from the embedded
    /// envelope. Returns [`ProvenParent::Child`] only when
    /// [`LineageStatus::Child`] is proven *and* the agent-scope key is
    /// itself available; otherwise [`ProvenParent::Unknown`] — never
    /// synthesized from any other field.
    pub fn proven_parent(&self) -> ProvenParent {
        let Some(identity) = self.execution.value() else {
            return ProvenParent::Unknown;
        };
        match identity.lineage_status() {
            LineageStatus::Root => ProvenParent::Root,
            LineageStatus::Unknown => ProvenParent::Unknown,
            LineageStatus::Child => {
                let Some(parent_agent_id) = identity.parent_agent_id() else {
                    return ProvenParent::Unknown;
                };
                let Ok(session_key) = identity.cache_key(Scope::Session) else {
                    return ProvenParent::Unknown;
                };
                let mut key = session_key;
                key.push(parent_agent_id.to_string());
                ProvenParent::Child(DimensionKey(key))
            }
        }
    }

    /// The narrowest [`crate::EconomicScope`] this attribution actually
    /// proves, walking the execution-identity containment chain from
    /// `TurnTask` to `Host`. Returns [`crate::EconomicScope::Unknown`] when
    /// nothing is proven.
    pub fn narrowest_proven_execution_scope(&self) -> crate::EconomicScope {
        use crate::EconomicScope;
        for scope in [Scope::TurnTask, Scope::Agent, Scope::Session, Scope::Host] {
            if let Some(identity) = self.execution.value() {
                if identity.cache_key(scope).is_ok() {
                    return match scope {
                        Scope::TurnTask => EconomicScope::TurnTask,
                        Scope::Agent => EconomicScope::Agent,
                        Scope::Session => EconomicScope::Session,
                        Scope::Host => EconomicScope::Host,
                        _ => unreachable!("loop only yields the four scopes listed above"),
                    };
                }
            }
        }
        EconomicScope::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionIdentityBuilder;

    fn now() -> time::OffsetDateTime {
        time::macros::datetime!(2026-10-02 12:00:00 UTC)
    }

    fn identity_root() -> ExecutionIdentity {
        ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .root_lineage()
            .build_at(now())
            .unwrap()
    }

    fn identity_child(parent: &str) -> ExecutionIdentity {
        ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-2")
            .child_lineage(parent)
            .build_at(now())
            .unwrap()
    }

    #[test]
    fn declared_version_is_supported() {
        assert!(is_supported_contract_version(
            ECONOMIC_ATTRIBUTION_CONTRACT_VERSION
        ));
    }

    #[test]
    fn organization_and_principal_are_always_not_configured_today() {
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity_root()));
        assert_eq!(
            attribution.organization,
            Attributed::unknown(UnknownReason::NotConfigured)
        );
        assert_eq!(
            attribution.principal,
            Attributed::unknown(UnknownReason::NotConfigured)
        );
    }

    #[test]
    fn pre_attribution_record_is_unknown_on_every_dimension() {
        let record = EconomicAttribution::pre_attribution_record();
        assert!(!record.execution.is_known());
        assert!(!record.task.is_known());
        assert!(!record.plan.is_known());
        assert!(!record.model_request.is_known());
        assert!(!record.organization.is_known());
        assert!(!record.principal.is_known());
        assert_eq!(
            record.execution,
            Attributed::unknown(UnknownReason::PreAttributionRecord)
        );
    }

    #[test]
    fn dimension_key_for_agent_is_none_without_agent_id() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .build_at(now())
            .unwrap();
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity));
        assert_eq!(attribution.dimension_key(EconomicDimension::Agent), None);
        // Never falls back to the session key.
        assert_ne!(
            attribution.dimension_key(EconomicDimension::Session),
            attribution.dimension_key(EconomicDimension::Agent)
        );
    }

    #[test]
    fn dimension_key_for_session_is_present() {
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity_root()));
        assert!(attribution
            .dimension_key(EconomicDimension::Session)
            .is_some());
    }

    #[test]
    fn organization_dimension_key_is_none_when_not_configured() {
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity_root()));
        assert_eq!(
            attribution.dimension_key(EconomicDimension::Organization),
            None
        );
    }

    #[test]
    fn proven_parent_is_root_when_envelope_proves_root() {
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity_root()));
        assert_eq!(attribution.proven_parent(), ProvenParent::Root);
    }

    #[test]
    fn proven_parent_is_child_with_key_when_envelope_proves_child() {
        let attribution =
            EconomicAttribution::from_execution(Attributed::known(identity_child("agent-1")));
        match attribution.proven_parent() {
            ProvenParent::Child(key) => assert!(key.0.contains(&"agent-1".to_string())),
            other => panic!("expected Child, got {other:?}"),
        }
    }

    #[test]
    fn proven_parent_is_unknown_when_lineage_unknown() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .build_at(now())
            .unwrap();
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity));
        assert_eq!(attribution.proven_parent(), ProvenParent::Unknown);
    }

    #[test]
    fn proven_parent_is_unknown_when_execution_itself_is_unknown() {
        let attribution = EconomicAttribution::from_execution(Attributed::unknown(
            UnknownReason::NotExposedByProvider,
        ));
        assert_eq!(attribution.proven_parent(), ProvenParent::Unknown);
    }

    #[test]
    fn narrowest_proven_scope_is_turn_task_when_turn_id_present() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .turn_id("turn-1")
            .root_lineage()
            .build_at(now())
            .unwrap();
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity));
        assert_eq!(
            attribution.narrowest_proven_execution_scope(),
            crate::EconomicScope::TurnTask
        );
    }

    #[test]
    fn narrowest_proven_scope_is_unknown_when_execution_unknown() {
        let attribution = EconomicAttribution::from_execution(Attributed::unknown(
            UnknownReason::NotExposedByProvider,
        ));
        assert_eq!(
            attribution.narrowest_proven_execution_scope(),
            crate::EconomicScope::Unknown
        );
    }

    #[test]
    fn attributed_round_trips_through_json() {
        let attribution = EconomicAttribution::from_execution(Attributed::known(identity_root()));
        let json = serde_json::to_string(&attribution).unwrap();
        let restored: EconomicAttribution = serde_json::from_str(&json).unwrap();
        assert_eq!(attribution, restored);
    }

    #[test]
    fn dimension_source_is_static_per_dimension() {
        assert_eq!(
            EconomicDimension::Session.source(),
            DimensionSource::ProviderNative
        );
        assert_eq!(
            EconomicDimension::Task.source(),
            DimensionSource::LibraInternal
        );
        assert_eq!(
            EconomicDimension::Organization.source(),
            DimensionSource::OperatorConfigured
        );
    }
}
