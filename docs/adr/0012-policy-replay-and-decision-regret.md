# ADR-0012: Counterfactual Policy Replay and Decision-Regret Metrics

> **Numbering note**: this ADR's local number, `0012`, coincides with an
> **unrelated** cross-repo ADR series referenced elsewhere in this
> codebase's own comments — `crates/ledger/migrations/0010_dogfood_evidence_capture.sql`,
> `crates/ledger/src/write.rs`, `crates/ledger/src/query.rs`, and
> `crates/cli/src/main.rs` all cite an "ADR-0012" (and an "ADR-0014") that
> belong to a *different*, cross-repo numbering scheme (HORO-1376's
> dogfood-evidence contract), not to this repository's own
> `docs/adr/NNNN-*.md` sequence. The two are independent documents that
> happen to share a number by coincidence of two separate counters. This
> document is strictly `docs/adr/0012-policy-replay-and-decision-regret.md`
> in *this* repo's own local sequence (following `0011-progressive-remaining-estimate-and-shadow-decisions.md`).

- **Status**: Accepted
- **Date**: 2026-10-03
- **Ticket**: HORO-1670

## Context

HORO-1669 added shadow progressive decisions: at a cadence/material-event
gate, the daemon computes what it *would* recommend, under the recorded
policy, and persists it to `shadow_runtime_decisions` — purely for later
analysis, never acted on. That gives Libra a growing corpus of real,
point-in-time decision records, but no way yet to ask the question that
corpus exists to answer: **would a different policy have made better
decisions on this same history?**

HORO-1673 (a later ticket) needs an answer to a narrower, high-stakes
version of that question — gating real policy changes on evidence that
they don't silently lower a task's quality floor. That ticket depends on
this one's central guarantee: that replay is **genuinely leakage-free**.
If replay could see anything about how a trajectory actually turned out
— a later decision point, the receipt, the final outcome — a replayed
"candidate would have done better" result would be unfalsifiable: of
course a policy "performs better" if it's allowed to peek at the answer.

## Decision

### Replay re-evaluates the policy, never the estimator

A replay decision point is exactly one `shadow_runtime_decisions` row.
Its [`RemainingWorkEstimate`] is read back **verbatim** — `remaining_bucketed`
is never called during replay, and no decision point is ever
reconstructed from receipts. [`replay_point`] composes
[`Policy::evaluate_resource`]/[`Policy::evaluate_time`] (made `pub` by
this ticket, pure visibility changes, no behavior change) against that
frozen estimate for whatever candidate [`Policy`] is being evaluated.

This also settles the "what would the actual policy have decided"
question without adding a second, parallel record of admission
decisions: `propose_runtime_decision` (HORO-1669's shipped behavior)
never actually computes an [`Admission`] verdict — it only distinguishes
`Continue` from `InsufficientEvidence`. Rather than teaching it to
compute one (explicitly HORO-1673's work, see Non-Goals below), this
module replays the **recorded policy itself** through the same
[`replay_point`] path used for every candidate:
`replay_point(point, &point.recorded_policy)`. Because `replay_point` is
a pure function of `(point, policy)`, this reproduces exactly what the
recorded policy would decide at that point, and the actual and every
candidate are compared through identically-shaped values.

### The replay boundary is a signature, not a convention

[`DecisionPoint`] has no field — and `replay_point` takes no parameter —
through which an `ExecutionReceipt`, `ExecutionOutcome`, a later decision
row, or future spend could enter. `replay_point` takes a single
`&DecisionPoint`, never a slice: it is structurally unable to see a later
point. Only [`PostHocRegret`], produced by a separate function with no
path back into decision-point replay, may reference actual outcome data.

### Version pinning

[`ReplayPins`] packages 12 schema/version dimensions a reproducible
replay must agree on: 8 already transitively carried on a recorded
`RuntimeDecision`, plus 4 new ones (`PersistedPins`, migration `0014`)
this codebase had no existing way to recover
(`economic_attribution_contract_version`,
`execution_identity_envelope_version`, `resource_account_schema_version`,
`reservation_schema_version`). `pricing_version` compares via the
existing positive-evidence rule (`RegimeKey::comparison`'s rule: a
mismatch counts only when both sides are `Known` and differ); every
other dimension compares by plain inequality.

A row predating migration `0014` has no `policy_json`/`pins_json` and is
`ReplayEligibility::Unpinned` — refused outright, not guessed. A row
whose pins/policy drift from the current build still replays (the
estimate is frozen; replay stays exactly reproducible) but is segregated
into its own cohort, never silently pooled with a non-drifted row from
the same task.

### Quality floor is a type-level guard, not a flag

[`PolicyComparison::evaluate`] checks whether the candidate's required
completion criteria are a superset of the recorded ones **before**
computing any regret at all. `PolicyComparison::QualityFloorViolated`
carries no regret/savings/headroom field — there is structurally no way
to read a floor-violating candidate as "cheaper" from this type. This is
the type HORO-1673 is expected to gate real policy changes on.

### Aggregation never double-counts

`aggregate_regret` sums only disagreement counts and point counts across
a set of `(account, parent_account, TrajectoryRegret)` triples, refusing
outright if any entry's own `parent_account` names another entry present
in the same call (a direct-parent check against the passed-in list, not a
walk of the full account tree — callers must pass one flat sibling
generation; the same custody-tree double-counting hazard ADR-0008
identifies) or if entries disagree on pins/spend scope. It does not sum
spend, and it is not a re-derivation of `economic_rollup`'s
provider-proven agent-lineage forest — those remain two intentionally
separate trees (ADR-0008). No production call site exists yet —
HORO-1673 is the expected first one; the `v003_replay` runner this ticket
adds sums per-candidate counts by hand instead.

## Corrections to the original design sketch, and scope actually landed

The design that originated this implementation assumed a pre-existing
`ResourceDelta` type (attributed to "HORO-1141"). An exhaustive search of
`crates/domain/src/*.rs` at implementation time found no such type
anywhere in the codebase. Rather than importing a type that does not
exist, this implementation narrows `PointRegret` to the fields that do
not depend on a signed resource-delta type at all:

```rust
pub struct PointRegret {
    pub decided_at: OffsetDateTime,
    pub elapsed_secs: u64,
    pub tool_calls_total: u64,
    pub disagreement: Disagreement,
    pub admitted_spend_so_far: SpendSoFar,
}
```

Dropped from the original design sketch, disclosed rather than silently
absorbed:

- `PointRegret`'s six headroom/conservatism fields
  (`actual_time_headroom_secs`, `candidate_time_headroom_secs`,
  `actual_resource_headroom`, `candidate_resource_headroom`,
  `elapsed_before_candidate_would_have_stopped_secs`,
  `candidate_conservatism`) and the `ConservatismSignal` type entirely.
  `Disagreement` (restrictive/permissive per dimension) already carries
  the comparably load-bearing signal for HORO-1673's purposes; the
  headroom magnitude is a follow-up enhancement, not a correctness
  requirement for the leakage-free guarantee this ticket exists to
  establish.
- `TrajectoryRegret.replan_count_after_first_candidate_refusal`.
- `PostHocRegret.elapsed_from_first_refusal_to_finalize_secs`.
- `AggregationError::MixedSpendScope`'s fields (kept as a unit-like
  variant with a descriptive `#[error]` message instead).
- Per-row cohort-exclusion reasons inside `replay_trajectory`
  (`PinsDriftedWithinTrajectory`/`PolicyChangedWithinTrajectory`) are
  folded into one `points_skipped: usize` total rather than kept as named
  per-reason tallies on `TrajectoryRegret`.

None of these omissions weaken the ticket's central guarantees (replay
leakage-freedom, quality-floor type-level partitioning, aggregation
overlap refusal) — all three are fully implemented and tested. They are
narrower descriptive statistics that can be added additively in a
follow-up without touching the replay boundary itself.

## Non-Goals (explicitly out of scope — HORO-1673)

- Teaching `propose_runtime_decision` to construct
  `StopEconomicallyIrrational`/`DegradeOptionalScope`/`RequestMoreBudget`/`Replan`.
  It still only ever produces `Continue`/`InsufficientEvidence`.
- Fixing `remaining_feasibility`'s hardcoded `FeasibilityBound`.
- A `libra-governor replay` CLI subcommand, a new `Request` protocol
  variant, or a protocol version bump. Replay is a `cargo run --example`
  offline harness only, exactly like `v003_gate` (HORO-1669).
- Unifying `aggregate_regret` with `economic_rollup::inclusive_spend`.

## Consequences

- `shadow_runtime_decisions` rows recorded before this ticket's daemon
  wiring landed (practically, every row recorded so far) cannot be
  replayed — they honestly report `ReplayEligibility::Unpinned`. Replay
  value accumulates from here forward.
- `crates/domain/src/replay.rs` is a pure, dependency-light module: no
  I/O, no clock reads beyond what's passed in, fully unit-testable
  without a ledger.
- HORO-1673 can build its quality-floor gate directly on
  `PolicyComparison`/`PolicyComparison::comparable_regret` without
  re-deriving any of this module's logic.
