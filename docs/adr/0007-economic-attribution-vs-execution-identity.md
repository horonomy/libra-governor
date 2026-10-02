# ADR 0007: Economic Attribution Is Built on, and Never Duplicates, Execution Identity

- **Status:** Accepted
- **Date:** 2026-10-02
- **Ticket:** HORO-1666

## Context

HORO-1597/1598/1599 give Libra a shared, vendor-neutral answer to WHO/WHERE/
WHICH EXECUTION: the [`ExecutionIdentity`] envelope, captured for real
Claude Code and Codex sessions. Libra's own ledger still needs an answer to
a different question — WHAT RESOURCE / HOW MUCH / WHO OWNS IT — and every
estimate, admission decision, and receipt depends on that answer being
attributed to the right task, session, agent, or person. This ADR records
the boundary between the two and the arithmetic built on top of it.

## Decision

### Boundary statement

The shared [`ExecutionIdentity`] envelope (`envelope_version: 1`) is the
sole source of host, tool/provider, provider session, agent, turn, and
parent-agent lineage identity in Libra. Libra never re-declares, re-derives,
reformats, or backfills any of those dimensions; its economic attribution
envelope ([`EconomicAttribution`]) embeds the shared envelope verbatim and
reads those dimensions through accessors. Libra owns, and the shared
contract does not: economic-event identity ([`EconomicEvent`]), resource-
fact provenance and truth-strength ([`ResourceBasis`]), reservation/
settlement ownership, exclusive-vs-inclusive aggregation semantics, and the
task/plan/model-request dimensions.

### Why embed rather than mirror fields

A mirrored field (e.g. a second `agent_id: Option<String>` on
`EconomicAttribution`) is a second identity system with a sync bug waiting
in it — the two copies can drift the moment either side updates
independently, and HORO-1597 exists precisely to prevent that pattern. An
embedded envelope cannot drift: there is only one copy of each field, and
`EconomicAttribution` reads it through accessors (`dimension_key`,
`proven_parent`, `narrowest_proven_execution_scope`) rather than storing a
derived value that could disagree with it.

### Amendment to HORO-1666's ticket text

The ticket states the shared envelope "owns org/principal context when
authoritatively available." This is not accurate: envelope v1 has exactly
13 fields (`envelope_version`, `observed_at`, `host_id`, `tool_provider`,
`tool_instance_id`, `provider_session_id`, `agent_id`, `turn_id`,
`lineage_status`, `parent_agent_id`, `session_lineage_id`, `event_id`,
`repo_id`, `worktree_id`), none of them organization/principal/tenant.
`organization`/`principal` in [`EconomicAttribution`] are Libra's own
operator-configured dimensions instead — structurally present, and always
[`UnknownReason::NotConfigured`] today, since v0.0.3 has no organization or
principal configuration surface (Team SaaS/control plane is an explicit
non-goal). This satisfies the acceptance criterion ("task/session/agent/
principal/org are distinct optional ownership dimensions") without
inventing an identity system or a config surface that does not exist yet.

Separately, [`ResourceBasis::ImportedAllocationSnapshot`] has no producer
in this ticket — nothing imports an external allocation snapshot yet. It
is typed now, produced later: a future importer has a truthful place to
put its data without the contract needing to widen.

### Relationship to the shared contract's `Scope`

[`EconomicScope`] is related to, but not interchangeable with, the shared
contract's [`Scope`] (itself already distinct from the statusline
contract's `scope` field — see the execution identity contract's own
"Relationship to the statusline contract's scope" section). `EconomicScope`
adds [`EconomicScope::ModelRequest`], [`EconomicScope::Task`],
[`EconomicScope::Principal`], and [`EconomicScope::Organization`] —
dimensions `Scope` has no concept of — and drops `ProjectWorktree`: a
repo/worktree filters a query, it never owns spend, exactly as the shared
contract itself says. [`EconomicScope::Unknown`] is kept as a permanent
legal member, for the same reason the shared contract keeps `Scope::Unknown`
permanent: pre-attribution legacy data is a real, permanent case, not a
migration target.

`EconomicScope` deliberately has no `Ord` implementation: a task spans
multiple sessions while a session maps to one task (overlapping, not
nested), so there is no total order across every variant. [`EXECUTION_CHAIN`]
gives the only ordering that exists — the proven-containment chain
(`TurnTask → Agent → Session → Host`) — used solely by
[`EconomicAttribution::narrowest_proven_execution_scope`].

### Exclusive vs. inclusive

Two different theorems, two different signatures:

- **Exclusive/self** ([`exclusive_spend`]): resource directly consumed by
  one agent node — spend-role, additive facts whose
  `EconomicDimension::Agent` key equals that node, exactly.
- **Inclusive/subtree** ([`inclusive_spend`]): self plus all proven
  descendants, defined *only* over the proven agent-lineage forest built by
  [`AgentLineage::from_events`]. Task/session/principal/organization totals
  are *partition projections* over canonical leaf events ([`project`]),
  never a recursive sum — a session has no proven children, so the
  recursive identity does not apply to it.

A billable/spend event is persisted exactly once, at its narrowest
truthful owning scope. Every rollup function de-duplicates by
[`EconomicEventId`] before summing, so replaying the same event twice can
never double count — "canonical spend events are single-recorded" is
therefore a property of the rollup functions themselves, not an assumption
placed on the caller.

The required invariant — `inclusive(node) == exclusive(node) +
Σ_{c ∈ children(node)} inclusive(c)` — is verified by
`economic_rollup::tests::exhaustive_forest_shapes_satisfy_the_inclusive_identity`
across four representative forest shapes (chain, star, balanced, and
disconnected two-root), each carrying a mix of additive and non-additive
facts across all four truth strengths, checked at every node and against
the grand total.

### Unknown lineage

An agent with [`LineageStatus::Unknown`] (or `Root`) becomes its own forest
root in [`AgentLineage`]. This is **not** an assertion that it is a
session's top-level agent — it only means no proven parent exists in the
given event set. Unknown lineage degrades the *agent-tree* dimension only:
such an agent's spend is excluded from every ancestor's inclusive total
(there is no proven ancestor to attach it to), but it still counts fully in
the session/task/grand-total projections, since those projections do not
depend on agent lineage at all. See
`unknown_lineage_agent_is_a_root_and_excluded_from_any_parent_inclusive`
and `unknown_lineage_agent_still_counts_in_session_projection`.

### Provenance is never collapsed

[`ResourceBasis`] is one closed enum carrying both meaning and source —
not a (meaning × source) matrix needing cross-field validation. Each
variant has a const `role()` (`Spend`/`Hold`/`Projection`/`Reference`), a
const `truth_strength()` (`Claimed` < `Estimated` < `Reported` < `Metered`),
and a const `is_additive()`. Additivity is a property of the *basis*, not
of the [`ResourceKind`] unit: `QuotaSnapshot` and
`ImportedAllocationSnapshot` are point-in-time levels (a gauge reading, a
configured ceiling), never deltas, so summing two of them would produce a
meaningless number regardless of whether the unit is dollars, tokens, or a
percentage. This is the structural reason estimate/actual/limit/quota can
never collapse into one summed "cost" field.

### Backward compatibility

[`EconomicAttribution::pre_attribution_record`] marks every dimension —
including execution identity itself — as
[`UnknownReason::PreAttributionRecord`]. Such a record only ever
constructs at [`EconomicScope::Unknown`] (a narrower scope claim would be a
guess `EconomicEvent::validated` refuses); it still counts in the grand
total (it is a real historical spend fact) and lands in `unattributed` for
every execution-backed projection. This state is permanent: a mixed table
of pre-attribution and fully-attributed rows is the expected, ongoing
shape, never a migration target to "fix."

### Schema evolution

One contract version, carried on the event only:
`ECONOMIC_ATTRIBUTION_CONTRACT_VERSION = 1`, stored as
`EconomicEvent::contract_version`. `EconomicAttribution` itself carries no
separate version field — a nested value with its own version that must
agree with the event's is a disagreement surface with no benefit — while
the embedded `ExecutionIdentity` keeps its own `envelope_version`, since it
genuinely evolves independently of Libra's attribution contract. Reader
behavior mirrors the shared contract's own version-evolution table: an
unknown top-level field is ignored; an unrecognized `EconomicScope` /
`ResourceBasis` / `UnknownReason` value is refused, not guessed; an
unknown `contract_version` is refused.

### Deferred to later tickets

No migration, no ledger change, no persistence of any kind in this ticket
— everything above is pure domain types and pure functions over in-memory
event slices. HORO-1667 is expected to persist `EconomicEvent` rows (one
row per event, an `attribution` JSON blob plus extracted indexable columns
for `owning_scope`/resource kind/basis/task id/session and agent cache
keys, following the precedent `crates/ledger/migrations/0006`/`0007` set),
and should carry a cross-check test asserting its SQL aggregation agrees
with `exclusive_spend`/`inclusive_spend`/`project` over the same synthetic
event set — these pure functions are the *definition* of the arithmetic,
not the production query path, and the two must not be allowed to diverge.

## Consequences

- A future second coding-agent provider needs no change to
  `EconomicAttribution`'s shape — it only needs an `ExecutionIdentity`
  capturer, exactly as HORO-1598/1599 already designed for.
- `organization`/`principal` are ready, typed dimensions the moment v0.0.3
  or a later milestone adds operator configuration for them, with zero
  further type changes required.
- HORO-1667/1668 build the persistence and the hierarchical budget-lease
  mechanics on top of these types without redesigning the attribution
  model itself.

## Alternatives considered

- **Mirroring session/agent/turn fields directly onto
  `EconomicAttribution`**: rejected — a second identity system with its own
  sync bugs, exactly what HORO-1597 exists to prevent.
- **A single `Option<T>` per dimension instead of `Attributed<T>`**:
  rejected — `None` cannot distinguish "the provider doesn't expose this"
  from "nobody configured this" from "this row predates attribution," and
  conflating any two of those is exactly the guessed-precision failure
  this contract forbids.
- **A generic `node` parameter for inclusive spend, covering sessions and
  agents alike**: rejected — sessions and tasks have no proven children;
  a generic signature would eventually get the recursive identity asserted
  somewhere it does not hold. Task/session/principal/organization use the
  separate `project` partition instead.
- **Persisting `economic_events` in this ticket**: rejected as premature —
  HORO-1666's acceptance criteria are fully satisfiable with types, pure
  functions, tests, and this ADR; a SQL migration belongs in HORO-1667,
  where the real indexing and query-shape decisions can be made with an
  actual access pattern in view.

[`ExecutionIdentity`]: ../../crates/domain/src/execution_identity.rs
[`Scope`]: ../../crates/domain/src/execution_identity.rs
[`LineageStatus`]: ../../crates/domain/src/execution_identity.rs
[`EconomicAttribution`]: ../../crates/domain/src/economic_attribution.rs
[`UnknownReason::NotConfigured`]: ../../crates/domain/src/economic_attribution.rs
[`EconomicAttribution::pre_attribution_record`]: ../../crates/domain/src/economic_attribution.rs
[`EconomicAttribution::narrowest_proven_execution_scope`]: ../../crates/domain/src/economic_attribution.rs
[`EconomicEvent`]: ../../crates/domain/src/economic_event.rs
[`EconomicEventId`]: ../../crates/domain/src/economic_event.rs
[`EconomicScope`]: ../../crates/domain/src/economic_event.rs
[`EXECUTION_CHAIN`]: ../../crates/domain/src/economic_event.rs
[`ResourceBasis`]: ../../crates/domain/src/economic_event.rs
[`ResourceBasis::ImportedAllocationSnapshot`]: ../../crates/domain/src/economic_event.rs
[`ResourceKind`]: ../../crates/domain/src/resource_amount.rs
[`exclusive_spend`]: ../../crates/domain/src/economic_rollup.rs
[`inclusive_spend`]: ../../crates/domain/src/economic_rollup.rs
[`project`]: ../../crates/domain/src/economic_rollup.rs
[`AgentLineage::from_events`]: ../../crates/domain/src/economic_rollup.rs
