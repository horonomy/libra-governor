# ADR 0011: Progressive remaining-cost estimate and shadow runtime decisions

## Status

Accepted (HORO-1669).

## Context

Prior to this ticket, Libra's only "govern the run" mechanism was HORO-1139's
deterministic replan tier (`RemainingEstimate::from_bucketed`,
`crates/domain/src/replan.rs`): on a coarse material-event signal (tool-call
count, possible loop), recompute the bucketed estimate and apply a fixed
1.5x widening to the tail quantiles plus a one-step confidence downgrade.
This is a single-shot adjustment, not a progressive re-estimate that
re-conditions as more runtime evidence arrives.

HORO-1669 asks: *given what has happened so far, how much is still
required to finish, and is the task still economically feasible?* —
answered repeatedly, not once.

## Decision: conditional empirical quantiles, not `total_pX - elapsed`

The naive approach — subtract elapsed time/spend from the original total
quantile — has a fatal pathology: once elapsed exceeds the original P90,
remaining reads 0 (or negative, clamped to 0). The task looks "done"
at exactly the moment it is overrunning.

Instead, `libra_governor_estimator::remaining_bucketed` computes remaining
quantiles as empirical quantiles over the *conditional* sample set:
`{ d - elapsed : d in historical samples, d > elapsed }`. When fewer than
`MIN_CONDITIONAL_SAMPLES` samples exceed the current point,
`RemainingDuration::Insufficient`/`RemainingResource::Insufficient` is
returned — not a fabricated number. That insufficiency IS the correct
signal: "you have already run longer than nearly every comparable task,"
exactly the moment a human (or the shadow decision gate) should want to
know.

This reuses the exact same bucket-ladder/`quantile_u64` machinery
`estimate_bucketed` already uses — "this task's bucket" is never a second,
silently-drifting notion.

## Duration/resource arm asymmetry

The resource arm (USD/token quantiles) has no evidence basis in the
common `HooksOnly` deployment: no receipt carries usage data without a
gateway configured (`resource_quantiles` already returns `None` on every
real local receipt today — this is not new to this ticket). The duration
arm is live now; the resource arm honestly reports
`RemainingResource::Unavailable` until a gateway is configured, then
lights up automatically with zero code change. The two arms are modeled
independently (not six `Option<ResourceAmount>` fields collapsed into one
shape) because they are independently grounded — one can be live while
the other is not.

## `account_spend` vs. `economic_rollup::inclusive_spend` — two different trees

HORO-1668 shipped the hierarchical resource-account tree but no
per-account exclusive/inclusive spend *query* — this ticket adds
`LedgerStore::account_spend` as genuinely new work.

This is deliberately **not** the same function as
`economic_rollup::inclusive_spend`. That function sums the
provider-proven agent-lineage forest (`AgentLineage`, derived from
`LineageStatus` — always an all-roots forest on every supported provider
today). `account_spend` sums the **custody tree** instead: account
`parent_account_id` edges are Libra-minted, proven only by the act of
leasing (ADR-0008). The two trees may legitimately disagree and are
cross-referenced from both call sites, never unified into one function —
conflating them would silently assert a lineage claim nothing has proven.

`account_spend`'s `Inclusive` scope uses a recursive CTE over
`resource_accounts.parent_account_id`. Migration 0011's own module docs
say "never a recursive CTE on the *write* path" — `account_spend` is a
read-only query off the hot path, so that rule is not violated; it is
deliberately read-path-only, and this ADR records that asymmetry rather
than leaving it looking like an oversight.

`subaccount_funding` leases are excluded from both scopes at every level:
that capacity belongs to the child it funds, not to the account that
handed it down. Counting it would double-count with the child's own
exclusive spend — pinned by a test provisioning a 3-level
task/session/agent tree and settling real work only at the grandchild.

## Shadow mode is structural, not a convention

The ticket requires every new stop/degrade decision to run in shadow mode:
record what Libra would have recommended, never act on it. Rather than a
flag or a doc comment saying "don't act on this yet,"
`libra_governor_domain::progressive::Shadow<T>` makes this structural:

- No `into_inner`, `AsRef`, `Deref`, `Deserialize`, or `Copy` exists on
  `Shadow<T>`.
- The only ways to observe a `Shadow<RuntimeDecision>` are its
  `Serialize` impl (for the audit row in `shadow_runtime_decisions`, see
  migration `0013`) and `Shadow::summary()`, which returns only a variant
  tag and a count — nothing an act-path could use to drive a real effect.

A daemon act-path therefore cannot obtain a `ProposedAction` from a
`Shadow` without re-serializing and re-parsing JSON — an obvious,
reviewable act in any future diff. **No promotion mechanism exists in
this ticket.** There is no `fn apply(decision)` anywhere in this crate.
Promotion to enforcement is HORO-1673's own reviewable work, gated on the
evidence this ticket starts collecting.

## The LLM-assisted critic: unmeterable, not deferred

The ticket's optional `ReplanTier::LlmAssisted` shadow experiment requires
the critic to be "separately metered" and "benchmarked against
deterministic/current baselines... killed if it does not materially
improve decision quality per unit cost." That requires a real per-decision
cost fact to benchmark against.

In the common `HooksOnly` deployment, no such fact exists: Claude
Code's/Codex's hook payloads expose no cost/token data (see HORO-1667's
audit, ADR-0009), and `gateway_requests` (the only provider-authoritative
economics surface) is empty with no gateway configured. The replan-
economics derivation below returns an honest insufficiency in exactly
this situation (`ReplanEconomicsInsufficient::NoBurnRate`/
`NoFreshResourceBasis`) rather than fabricating a benefit — and the same
absence of a cost fact is what makes the LLM critic unmeterable.

**This ticket does not implement the LLM critic.** `ReplanTier::LlmAssisted`
remains exactly as it was — an unimplemented, documented placeholder,
constructed nowhere. The finding to carry into HORO-1673: **DROP LLM
CRITIC — unmeterable at current enforcement tier (HooksOnly: no
per-decision cost fact exists)**. If v0.0.3 dogfooding later runs with the
gateway on by default, this becomes measurable automatically — no rework
of anything in this ticket is required.

## Replan economics: never fabricate a benefit

`replan_cost_benefit_from_remaining` derives a `ReplanCostBenefit` from
two `RemainingWorkEstimate`s (a stale plan's implied remaining cost vs. a
freshly re-conditioned one) and hands it to the existing, unchanged
`should_replan` gate (`ExpectedBenefit > Cost + SwitchingCost + DelayCost`).
When the fresh estimate's resource arm is `Unavailable`/`Insufficient`, or
no burn rate is available to cost-convert the replan delay, this function
returns `Err(ReplanEconomicsInsufficient)` rather than a number. In the
common deployment this correctly yields insufficient-evidence — the same
honest answer the LLM-critic finding above describes.

## Latency constraint

`Request::ToolInvoked`'s protocol docs commit to staying cheap enough not
to add perceptible latency to every tool call. The progressive estimate
is therefore not recomputed on every tool call; a cadence gate
(`progressive_interval_secs`, default 60) joins the existing
material-event gate so the expensive history read only runs when either
gate opens. The remaining estimate can be up to `progressive_interval_secs`
stale — recorded in every audit row (`elapsed_secs`, `observed_at`) so a
later evidence-gate ticket can measure empirically whether staleness
mattered.

## Scope boundaries

- `RemainingEstimate::from_bucketed`/`DETERMINISTIC_WIDENING_FACTOR`
  (HORO-1139) are untouched — the named benchmark baseline this ticket's
  own benchmark harness compares against.
- No promotion-to-enforcement path exists — HORO-1673's job.
- `ReplanTier::LlmAssisted` is not implemented; no critic
  service/daemon/subagent exists.
- No protocol version bump, no new `Request` variant — nothing reads the
  shadow-decision table over the wire yet; a future reporting ticket can
  read the ledger directly.
- `confidence_basis`/`detect_drift`/`RegimeCalibrationReport` (HORO-1671)
  are consumed, not modified.
