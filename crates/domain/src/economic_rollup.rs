//! Exclusive/inclusive spend aggregation over the proven agent-lineage
//! forest, and dimension projections over canonical leaf events.
//!
//! Two different theorems, two different signatures:
//!
//! - **Exclusive/inclusive** ([`exclusive_spend`]/[`inclusive_spend`]) is
//!   defined only over the proven agent-lineage forest built by
//!   [`AgentLineage::from_events`]. The recursive identity
//!   `inclusive(node) == exclusive(node) + Σ inclusive(children)` holds
//!   over that forest.
//! - **[`project`]** is a partition over canonical leaf events for any
//!   [`crate::EconomicDimension`] — every event lands in exactly one group
//!   or in `unattributed`, never both, never twice.
//!
//! Every function here de-duplicates by [`EconomicEventId`] before summing,
//! so replaying the same event twice can never double count.

use crate::economic_attribution::{DimensionKey, EconomicDimension, ProvenParent};
#[cfg(test)]
use crate::economic_event::EconomicEventId;
use crate::economic_event::{EconomicEvent, FactRole, ResourceFact, TruthStrength};
use crate::ResourceAmount;
use crate::ResourceKind;
use std::collections::{BTreeMap, BTreeSet};

/// Why building an [`AgentLineage`] or computing [`inclusive_spend`]
/// failed. Both are real paths over caller-supplied event data, not
/// hypotheticals: a provider could (incorrectly) report conflicting
/// parents for the same agent across two events, or — defensively, should
/// future code ever construct an [`AgentLineage`] by hand — a lineage
/// graph could contain a cycle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RollupError {
    #[error("lineage cycle detected at agent {0:?}")]
    LineageCycle(DimensionKey),
    #[error("conflicting parent for agent {agent:?}: {first:?} vs {second:?}")]
    ConflictingParent {
        agent: DimensionKey,
        first: DimensionKey,
        second: DimensionKey,
    },
}

/// The proven agent-lineage forest, built from events. Edges exist **only**
/// where [`ProvenParent::Child`] is proven by the embedded
/// [`crate::ExecutionIdentity`] — never reconstructed from timing or
/// process ancestry. An agent with [`ProvenParent::Unknown`] (or
/// [`ProvenParent::Root`]) lineage becomes its own forest root, which is
/// **not** an assertion that it is a session's top-level agent — it only
/// means no proven parent exists in this event set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLineage {
    children: BTreeMap<DimensionKey, Vec<DimensionKey>>,
    parents: BTreeMap<DimensionKey, DimensionKey>,
    roots: Vec<DimensionKey>,
}

impl AgentLineage {
    /// Builds the lineage forest from a set of events, de-duplicating by
    /// [`EconomicEventId`] first.
    pub fn from_events<'a>(
        events: impl IntoIterator<Item = &'a EconomicEvent>,
    ) -> Result<Self, RollupError> {
        let mut seen_ids = BTreeSet::new();
        let mut parents: BTreeMap<DimensionKey, DimensionKey> = BTreeMap::new();
        let mut known_agents: BTreeSet<DimensionKey> = BTreeSet::new();

        for event in events {
            if !seen_ids.insert(event.id()) {
                continue;
            }
            let attribution = event.attribution();
            let Some(agent_key) = attribution.dimension_key(EconomicDimension::Agent) else {
                continue;
            };
            known_agents.insert(agent_key.clone());
            if let ProvenParent::Child(parent_key) = attribution.proven_parent() {
                known_agents.insert(parent_key.clone());
                match parents.get(&agent_key) {
                    Some(existing) if existing != &parent_key => {
                        return Err(RollupError::ConflictingParent {
                            agent: agent_key,
                            first: existing.clone(),
                            second: parent_key,
                        });
                    }
                    Some(_) => {}
                    None => {
                        parents.insert(agent_key, parent_key);
                    }
                }
            }
        }

        // Cycle detection: walk each known agent's parent chain; revisiting
        // a node proves a cycle.
        for agent in &known_agents {
            let mut visited = BTreeSet::new();
            let mut current = agent.clone();
            loop {
                if !visited.insert(current.clone()) {
                    return Err(RollupError::LineageCycle(current));
                }
                match parents.get(&current) {
                    Some(parent) => current = parent.clone(),
                    None => break,
                }
            }
        }

        let mut children: BTreeMap<DimensionKey, Vec<DimensionKey>> = BTreeMap::new();
        for (child, parent) in &parents {
            children
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
        }

        let roots: Vec<DimensionKey> = known_agents
            .iter()
            .filter(|agent| !parents.contains_key(*agent))
            .cloned()
            .collect();

        Ok(Self {
            children,
            parents,
            roots,
        })
    }

    pub fn children(&self, agent: &DimensionKey) -> &[DimensionKey] {
        self.children.get(agent).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn parent(&self, agent: &DimensionKey) -> Option<&DimensionKey> {
        self.parents.get(agent)
    }

    pub fn roots(&self) -> &[DimensionKey] {
        &self.roots
    }
}

/// Per-[`ResourceKind`] spend subtotal.
///
/// `value` is `f64`, not [`ResourceAmount`], deliberately: `ResourceAmount`
/// rounds and clamps (e.g. `QuotaPercent` saturates at 100.0), which would
/// break `inclusive = exclusive + Σ children` associativity mid-sum.
/// Accumulate in `f64`; clamp only at the display boundary via
/// [`Subtotal::as_resource_amount_clamped`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Subtotal {
    pub kind: ResourceKind,
    pub value: f64,
    pub weakest_truth: TruthStrength,
    pub event_count: u32,
}

impl Subtotal {
    pub fn as_resource_amount_clamped(&self) -> ResourceAmount {
        ResourceAmount::from_kind_f64(self.kind, self.value)
    }

    fn seed(kind: ResourceKind) -> Self {
        Self {
            kind,
            value: 0.0,
            // `Metered` is the strongest member; the first real fact
            // folded in always lowers (or keeps) it via `min`, so a seed
            // subtotal's truth is never reported unless a fact is added.
            weakest_truth: TruthStrength::Metered,
            event_count: 0,
        }
    }

    fn fold(&mut self, amount: f64, basis_truth: TruthStrength) {
        self.value += amount;
        self.weakest_truth = self.weakest_truth.min(basis_truth);
        self.event_count += 1;
    }
}

/// Per-[`ResourceKind`] spend totals for one node or group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpendTotals(pub BTreeMap<ResourceKind, Subtotal>);

impl SpendTotals {
    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }

    /// Folds one [`ResourceFact`] in, if and only if its basis is
    /// spend-role and additive — never a hold, projection, reference, or
    /// non-additive level.
    fn add_fact(&mut self, fact: ResourceFact) {
        if fact.basis.role() != FactRole::Spend || !fact.basis.is_additive() {
            return;
        }
        let kind = fact.amount.kind();
        self.0
            .entry(kind)
            .or_insert_with(|| Subtotal::seed(kind))
            .fold(fact.amount.as_f64(), fact.basis.truth_strength());
    }

    fn merge(mut self, other: &Self) -> Self {
        for (kind, sub) in &other.0 {
            self.0
                .entry(*kind)
                .or_insert_with(|| Subtotal::seed(*kind))
                .fold_subtotal(sub);
        }
        self
    }
}

impl Subtotal {
    fn fold_subtotal(&mut self, other: &Subtotal) {
        self.value += other.value;
        self.weakest_truth = self.weakest_truth.min(other.weakest_truth);
        self.event_count += other.event_count;
    }
}

/// Resource directly consumed by this agent node — spend-role, additive
/// facts whose [`EconomicDimension::Agent`] key equals `agent`, exactly.
/// De-duplicates by [`EconomicEventId`] internally.
pub fn exclusive_spend(events: &[EconomicEvent], agent: &DimensionKey) -> SpendTotals {
    let mut seen = BTreeSet::new();
    let mut totals = SpendTotals::empty();
    for event in events {
        if !seen.insert(event.id()) {
            continue;
        }
        if event
            .attribution()
            .dimension_key(EconomicDimension::Agent)
            .as_ref()
            != Some(agent)
        {
            continue;
        }
        totals.add_fact(event.fact());
    }
    totals
}

/// Self plus all proven descendants, over `lineage`. Only defined for an
/// agent node — sessions, tasks, principals, and organizations have no
/// proven children, so a generic `node` parameter would eventually get
/// this identity asserted somewhere it does not hold.
///
/// Invariant under test: `inclusive(node) == exclusive(node) +
/// Σ_{c ∈ children(node)} inclusive(c)`.
pub fn inclusive_spend(
    events: &[EconomicEvent],
    lineage: &AgentLineage,
    agent: &DimensionKey,
) -> Result<SpendTotals, RollupError> {
    inclusive_spend_rec(events, lineage, agent, &mut BTreeSet::new())
}

fn inclusive_spend_rec(
    events: &[EconomicEvent],
    lineage: &AgentLineage,
    agent: &DimensionKey,
    visiting: &mut BTreeSet<DimensionKey>,
) -> Result<SpendTotals, RollupError> {
    if !visiting.insert(agent.clone()) {
        return Err(RollupError::LineageCycle(agent.clone()));
    }
    let mut totals = exclusive_spend(events, agent);
    for child in lineage.children(agent) {
        let child_totals = inclusive_spend_rec(events, lineage, child, visiting)?;
        totals = totals.merge(&child_totals);
    }
    visiting.remove(agent);
    Ok(totals)
}

/// A partition of canonical leaf events over one [`EconomicDimension`].
/// Every event lands in exactly one `groups` entry, or in `unattributed`
/// — never both, never neither.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub groups: BTreeMap<DimensionKey, SpendTotals>,
    pub unattributed: SpendTotals,
}

/// Groups canonical leaf events by `dim`, de-duplicating by
/// [`EconomicEventId`] internally.
pub fn project(events: &[EconomicEvent], dim: EconomicDimension) -> Projection {
    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<DimensionKey, SpendTotals> = BTreeMap::new();
    let mut unattributed = SpendTotals::empty();
    for event in events {
        if !seen.insert(event.id()) {
            continue;
        }
        match event.attribution().dimension_key(dim) {
            Some(key) => groups
                .entry(key)
                .or_insert_with(SpendTotals::empty)
                .add_fact(event.fact()),
            None => unattributed.add_fact(event.fact()),
        }
    }
    Projection {
        groups,
        unattributed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economic_attribution::{Attributed, EconomicAttribution};
    use crate::economic_event::{EconomicScope, ResourceBasis};
    use crate::{ExecutionIdentityBuilder, ResourceAmount};

    fn now() -> time::OffsetDateTime {
        time::macros::datetime!(2026-10-02 12:00:00 UTC)
    }

    fn attribution_for(
        session: &str,
        agent: &str,
        lineage: crate::LineageStatus,
        parent: Option<&str>,
    ) -> EconomicAttribution {
        let mut builder = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id(session)
            .agent_id(agent);
        builder = match (lineage, parent) {
            (crate::LineageStatus::Root, _) => builder.root_lineage(),
            (crate::LineageStatus::Child, Some(p)) => builder.child_lineage(p),
            _ => builder,
        };
        EconomicAttribution::from_execution(Attributed::known(builder.build_at(now()).unwrap()))
    }

    fn event(
        attribution: EconomicAttribution,
        amount: ResourceAmount,
        basis: ResourceBasis,
        scope: EconomicScope,
    ) -> EconomicEvent {
        EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            ResourceFact { amount, basis },
            attribution,
            scope,
        )
        .unwrap()
    }

    fn agent_key(session: &str, agent: &str) -> DimensionKey {
        attribution_for(session, agent, crate::LineageStatus::Root, None)
            .dimension_key(EconomicDimension::Agent)
            .unwrap()
    }

    #[test]
    fn exclusive_spend_sums_only_this_agents_spend_role_additive_facts() {
        let root = agent_key("sess-1", "agent-root");
        let events = vec![
            event(
                attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(100),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
            event(
                attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None),
                ResourceAmount::QuotaPercent(50.0),
                ResourceBasis::QuotaSnapshot,
                EconomicScope::Agent,
            ),
        ];
        let totals = exclusive_spend(&events, &root);
        assert_eq!(totals.0.get(&ResourceKind::Usd).unwrap().value, 100.0);
        // QuotaSnapshot is non-additive reference, excluded from spend sums.
        assert!(!totals.0.contains_key(&ResourceKind::QuotaPercent));
    }

    #[test]
    fn duplicate_event_id_does_not_double_count() {
        let root = agent_key("sess-1", "agent-root");
        let attribution = attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None);
        let event_id = EconomicEventId::new();
        let fact = ResourceFact {
            amount: ResourceAmount::UsdCents(100),
            basis: ResourceBasis::GatewayMeteredActual,
        };
        let one = EconomicEvent::validated(
            event_id,
            now(),
            fact,
            attribution.clone(),
            EconomicScope::Agent,
        )
        .unwrap();
        let duplicate =
            EconomicEvent::validated(event_id, now(), fact, attribution, EconomicScope::Agent)
                .unwrap();
        let totals = exclusive_spend(&[one, duplicate], &root);
        assert_eq!(totals.0.get(&ResourceKind::Usd).unwrap().value, 100.0);
        assert_eq!(totals.0.get(&ResourceKind::Usd).unwrap().event_count, 1);
    }

    #[test]
    fn inclusive_equals_exclusive_plus_children_for_a_two_level_tree() {
        let root = agent_key("sess-1", "agent-root");
        let child = agent_key("sess-1", "agent-child");
        let events = vec![
            event(
                attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(100),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
            event(
                attribution_for(
                    "sess-1",
                    "agent-child",
                    crate::LineageStatus::Child,
                    Some("agent-root"),
                ),
                ResourceAmount::UsdCents(50),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
        ];
        let lineage = AgentLineage::from_events(&events).unwrap();
        let root_inclusive = inclusive_spend(&events, &lineage, &root).unwrap();
        let root_exclusive = exclusive_spend(&events, &root);
        let child_inclusive = inclusive_spend(&events, &lineage, &child).unwrap();

        assert_eq!(
            root_inclusive.0.get(&ResourceKind::Usd).unwrap().value,
            root_exclusive.0.get(&ResourceKind::Usd).unwrap().value
                + child_inclusive.0.get(&ResourceKind::Usd).unwrap().value
        );
        assert_eq!(
            root_inclusive.0.get(&ResourceKind::Usd).unwrap().value,
            150.0
        );
    }

    #[test]
    fn three_level_tree_arithmetic_and_weakest_truth_propagate() {
        // root -> mid -> leaf, each with a different truth strength.
        let root = agent_key("sess-1", "agent-root");
        let mid = agent_key("sess-1", "agent-mid");
        let leaf = agent_key("sess-1", "agent-leaf");
        let events = vec![
            event(
                attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(10),
                ResourceBasis::GatewayMeteredActual, // Metered
                EconomicScope::Agent,
            ),
            event(
                attribution_for(
                    "sess-1",
                    "agent-mid",
                    crate::LineageStatus::Child,
                    Some("agent-root"),
                ),
                ResourceAmount::UsdCents(20),
                ResourceBasis::ProviderReportedActual, // Reported
                EconomicScope::Agent,
            ),
            event(
                attribution_for(
                    "sess-1",
                    "agent-leaf",
                    crate::LineageStatus::Child,
                    Some("agent-mid"),
                ),
                ResourceAmount::UsdCents(30),
                ResourceBasis::GatewayMeteredActual, // Metered
                EconomicScope::Agent,
            ),
        ];
        let lineage = AgentLineage::from_events(&events).unwrap();
        let root_inclusive = inclusive_spend(&events, &lineage, &root).unwrap();
        let usd = root_inclusive.0.get(&ResourceKind::Usd).unwrap();
        assert_eq!(usd.value, 60.0);
        // Weakest truth across the subtree is Reported (from agent-mid).
        assert_eq!(usd.weakest_truth, TruthStrength::Reported);
        assert_eq!(usd.event_count, 3);

        let _ = mid;
        let _ = leaf;
    }

    #[test]
    fn unknown_lineage_agent_is_a_root_and_excluded_from_any_parent_inclusive() {
        let root = agent_key("sess-1", "agent-root");
        let unknown_agent = attribution_for(
            "sess-1",
            "agent-unknown",
            crate::LineageStatus::Unknown,
            None,
        )
        .dimension_key(EconomicDimension::Agent)
        .unwrap();
        let events = vec![
            event(
                attribution_for("sess-1", "agent-root", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(100),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
            event(
                attribution_for(
                    "sess-1",
                    "agent-unknown",
                    crate::LineageStatus::Unknown,
                    None,
                ),
                ResourceAmount::UsdCents(500),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
        ];
        let lineage = AgentLineage::from_events(&events).unwrap();
        assert!(lineage.roots().contains(&unknown_agent));
        assert!(lineage.parent(&unknown_agent).is_none());

        let root_inclusive = inclusive_spend(&events, &lineage, &root).unwrap();
        // The unknown-lineage agent's spend never attaches to agent-root's
        // inclusive total, even though both are forest roots.
        assert_eq!(
            root_inclusive.0.get(&ResourceKind::Usd).unwrap().value,
            100.0
        );
    }

    #[test]
    fn unknown_lineage_agent_still_counts_in_session_projection() {
        let events = vec![event(
            attribution_for(
                "sess-1",
                "agent-unknown",
                crate::LineageStatus::Unknown,
                None,
            ),
            ResourceAmount::UsdCents(500),
            ResourceBasis::GatewayMeteredActual,
            EconomicScope::Agent,
        )];
        let projection = project(&events, EconomicDimension::Session);
        assert_eq!(
            projection
                .groups
                .values()
                .next()
                .unwrap()
                .0
                .get(&ResourceKind::Usd)
                .unwrap()
                .value,
            500.0
        );
        assert_eq!(projection.unattributed, SpendTotals::empty());
    }

    #[test]
    fn conflicting_parent_is_an_error_not_a_panic() {
        let events = vec![
            event(
                attribution_for(
                    "sess-1",
                    "agent-child",
                    crate::LineageStatus::Child,
                    Some("agent-root-a"),
                ),
                ResourceAmount::UsdCents(1),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
            event(
                attribution_for(
                    "sess-1",
                    "agent-child",
                    crate::LineageStatus::Child,
                    Some("agent-root-b"),
                ),
                ResourceAmount::UsdCents(1),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
        ];
        let result = AgentLineage::from_events(&events);
        assert!(matches!(result, Err(RollupError::ConflictingParent { .. })));
    }

    #[test]
    fn backward_compat_pre_attribution_event_counts_in_grand_total_but_unattributed_everywhere() {
        let record = EconomicAttribution::pre_attribution_record();
        let events = vec![EconomicEvent::validated(
            EconomicEventId::new(),
            now(),
            ResourceFact {
                amount: ResourceAmount::UsdCents(42),
                basis: ResourceBasis::GatewayMeteredActual,
            },
            record,
            EconomicScope::Unknown,
        )
        .unwrap()];

        for dim in [
            EconomicDimension::Session,
            EconomicDimension::Agent,
            EconomicDimension::Task,
            EconomicDimension::Organization,
        ] {
            let projection = project(&events, dim);
            assert!(projection.groups.is_empty());
            assert_eq!(
                projection
                    .unattributed
                    .0
                    .get(&ResourceKind::Usd)
                    .unwrap()
                    .value,
                42.0
            );
        }
    }

    #[test]
    fn projection_partitions_every_event_exactly_once() {
        let events = vec![
            event(
                attribution_for("sess-1", "agent-1", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(10),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
            event(
                attribution_for("sess-2", "agent-2", crate::LineageStatus::Root, None),
                ResourceAmount::UsdCents(20),
                ResourceBasis::GatewayMeteredActual,
                EconomicScope::Agent,
            ),
        ];
        let projection = project(&events, EconomicDimension::Session);
        let grand_total: f64 = projection
            .groups
            .values()
            .chain(std::iter::once(&projection.unattributed))
            .filter_map(|t| t.0.get(&ResourceKind::Usd))
            .map(|s| s.value)
            .sum();
        assert_eq!(grand_total, 30.0);
        assert_eq!(projection.groups.len(), 2);
    }

    /// Exhaustive-ish check: every rooted forest shape over 4 agents (a
    /// chain, a star, and a balanced binary shape), each with a mix of
    /// additive/non-additive and varying-truth-strength facts, satisfies
    /// `inclusive(node) == exclusive(node) + Σ inclusive(children)` and
    /// `Σ_roots inclusive(root) == grand total of spend-role additive
    /// facts`. Deterministic (no RNG dependency; the workspace has none),
    /// reproducible by construction rather than by seed.
    #[test]
    fn exhaustive_forest_shapes_satisfy_the_inclusive_identity() {
        struct Shape {
            name: &'static str,
            // (agent, parent) pairs; parent None => root.
            edges: &'static [(&'static str, Option<&'static str>)],
        }
        const SHAPES: &[Shape] = &[
            Shape {
                name: "chain",
                edges: &[
                    ("a", None),
                    ("b", Some("a")),
                    ("c", Some("b")),
                    ("d", Some("c")),
                ],
            },
            Shape {
                name: "star",
                edges: &[
                    ("a", None),
                    ("b", Some("a")),
                    ("c", Some("a")),
                    ("d", Some("a")),
                ],
            },
            Shape {
                name: "balanced",
                edges: &[
                    ("a", None),
                    ("b", Some("a")),
                    ("c", Some("a")),
                    ("d", Some("b")),
                ],
            },
            Shape {
                name: "two_roots",
                edges: &[("a", None), ("b", Some("a")), ("c", None), ("d", Some("c"))],
            },
        ];

        let bases: [(ResourceBasis, i64); 4] = [
            (ResourceBasis::GatewayMeteredActual, 10),
            (ResourceBasis::ProviderReportedActual, 7),
            (ResourceBasis::QuotaSnapshot, 999), // excluded from sums
            (ResourceBasis::LibraReservationHold, 999), // excluded (role=Hold)
        ];

        for shape in SHAPES {
            let mut events = Vec::new();
            for (i, (agent, parent)) in shape.edges.iter().enumerate() {
                let lineage_status = if parent.is_some() {
                    crate::LineageStatus::Child
                } else {
                    crate::LineageStatus::Root
                };
                for (basis, amount) in bases {
                    events.push(event(
                        attribution_for("sess-1", agent, lineage_status, *parent),
                        ResourceAmount::UsdCents(amount + i as i64),
                        basis,
                        EconomicScope::Agent,
                    ));
                }
            }

            let lineage = AgentLineage::from_events(&events).unwrap();

            // Recursive identity at every node.
            let all_agents: Vec<DimensionKey> = shape
                .edges
                .iter()
                .map(|(agent, _)| agent_key("sess-1", agent))
                .collect();
            for agent in &all_agents {
                let inclusive = inclusive_spend(&events, &lineage, agent).unwrap();
                let exclusive = exclusive_spend(&events, agent);
                let children_sum = lineage
                    .children(agent)
                    .iter()
                    .map(|c| inclusive_spend(&events, &lineage, c).unwrap())
                    .fold(SpendTotals::empty(), |acc, t| acc.merge(&t));
                let expected = exclusive.merge(&children_sum);
                assert_eq!(
                    inclusive.0.get(&ResourceKind::Usd).map(|s| s.value),
                    expected.0.get(&ResourceKind::Usd).map(|s| s.value),
                    "shape {} failed the inclusive identity at an agent",
                    shape.name
                );
            }

            // Grand total over roots equals the sum of all spend-role
            // additive facts (QuotaSnapshot/LibraReservationHold excluded).
            let roots_total: f64 = lineage
                .roots()
                .iter()
                .map(|r| inclusive_spend(&events, &lineage, r).unwrap())
                .fold(SpendTotals::empty(), |acc, t| acc.merge(&t))
                .0
                .get(&ResourceKind::Usd)
                .map(|s| s.value)
                .unwrap_or(0.0);
            let direct_total: f64 = events
                .iter()
                .filter(|e| {
                    e.fact().basis.role() == FactRole::Spend && e.fact().basis.is_additive()
                })
                .map(|e| e.fact().amount.as_f64())
                .sum();
            assert_eq!(
                roots_total, direct_total,
                "shape {} failed the grand total",
                shape.name
            );
        }
    }
}
