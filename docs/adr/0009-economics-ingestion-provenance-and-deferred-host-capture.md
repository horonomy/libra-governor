# ADR 0009: Economics ingestion provenance, and deferred host-payload capture

- Status: Accepted
- Date: 2026-10-02
- Related: HORO-1667, HORO-1666 (`docs/adr/0007-economic-attribution-vs-execution-identity.md`),
  HORO-1668 (`docs/adr/0008-hierarchical-resource-accounts-and-lease-custody.md`)

## Context

HORO-1666 defined `EconomicEvent`/`EconomicAttribution`/`ResourceBasis` — a
typed, provenance-carrying model for one economic fact. HORO-1667's job is
to produce real instances of that model from the economic/resource
signals Libra's host and provider boundary actually expose, without
turning a client-side estimate into a claimed provider bill.

## Surface audit

Neither Claude Code's nor Codex's **hook payloads** expose any economic
signal. The union of fields either host sends via hooks is session/tool
identity, prompt/tool metadata, and lifecycle state — no tokens, no cost,
no duration, no quota. `AgentCapabilities::model_event_observation`
already states this correctly for both agents and is unchanged by this
ticket.

The only surface that is genuinely **provider-authoritative** today is
`gateway_requests` (migration `0007`, HORO-1144): it holds a settled
amount, a `resource_kind` discriminator, four token counters,
`usage_known`, and `pricing_version`, for every request that passed
through Libra's own gateway.

Claude Code's `statusLine` stdin payload is the richest host-economics
surface that exists anywhere in this ecosystem (`cost.total_cost_usd`,
`rate_limits.*`, `effort.level`, cache/model/pricing provenance). See
"Deferred: host-payload capture" below for why this ticket does not
ingest it.

## Basis-selection table

For one `gateway_requests` row, at most two sibling `EconomicEvent`s are
produced (never one event with two facts — see
`crates/domain/src/economic_event.rs`'s module docs):

| `tier` | `usage_known` | USD fact | token fact |
|---|---|---|---|
| `GatewayMetered` | `true` | `GatewayMeteredActual` | `GatewayMeteredActual` |
| `GatewayObservedQuota` | `true` | **none** — the monetary figure is not provider-authoritative at this tier (the subscription's own accounting is opaque to Libra, per `EnforcementTier::GatewayObservedQuota`'s own docs) | `ProviderReportedActual` |
| any | `false` | `LibraReservationHold` — the conservative reserved-amount fallback (`UsageAccounting::ReservedAmountFallback`), never `GatewayMeteredActual` | none |

A host-reported list-price estimate is never conflated with an invoice.
A quota percentage is never summed as USD. These are structurally
distinct `ResourceBasis` variants with different `is_additive()`/`role()`
semantics (HORO-1666), not a convention this ticket could silently
violate.

## Idempotency

`EconomicEventId::deterministic(name)` (UUIDv5 over a fixed namespace) is
used with `name = "{gateway_request_id}:usd"` / `"{gateway_request_id}:tokens"`.
`gateway_requests.id` is already idempotent on replay ("a replay replaces
the row"), and every rollup in `economic_rollup` de-duplicates by
`EconomicEventId` before summing — so a replayed ingest can never double
count, with zero additional state.

## Correlation priority

Implemented in `crates/domain/src/economic_ingest.rs::attribution_for`,
exactly the ticket's three levels:

1. A caller-supplied `ExecutionIdentity` whose `provider_session_id`
   matches the row's `session_id` → `execution: Known`, `owning_scope =
   Session`. The identity is **passed in, never re-derived** —
   `gateway_requests` carries no `host_id`/`tool_provider`, so
   synthesising one would be the second identity system ADR-0007
   forbids.
2. Else a proven `task_id` → `task: Known`, `owning_scope = Task`.
3. Else → every dimension `Unknown`, `owning_scope = Unknown`.

This path never produces `EconomicScope::Agent` or `EconomicScope::TurnTask`:
the gateway exposes neither `agent_id` nor `turn_id`.

## The snapshot-to-delta differ

`deltas_from_snapshots` is a pure function over an ordered series of
cumulative counter readings — it holds no live state. A cumulative value
can only become an `EconomicEvent` by passing through this function;
nothing in this crate can emit a raw cumulative total directly as a
spend-additive fact.

`CounterKey` is `(session_scope_key, CounterKind)`. `model`/`effort` are
carried as provenance on the observation, **never part of the key** — a
session total spans models, so keying by model would produce parallel
gauges each wrongly claiming the whole session.

Handled, each with its own test: repeated identical sample (no event);
out-of-order sample (resolved by sorting on `observed_at`, later wins);
counter reset (the pre-reset high-water mark survives as its own
terminal observation; the post-reset value starts a fresh delta from
zero — the earlier spend is never lost); session restart/resume (a new
scope key starts a fresh series); model switch (no key change);
concurrent sessions (distinct scope keys are structurally isolated).

**This differ has no live producer yet** — see below. These are tested
pure functions ready for a future producer, not a vacuous acceptance
criterion.

## Deferred: host-payload capture (founder decision, 2026-10-02)

Capturing Claude Code's `statusLine` stdin payload was evaluated and is
**explicitly deferred**, not silently dropped.

`horonomy/.github`'s statusline compositor (`scripts/statusline_compositor.py::run_provider`)
deliberately invokes each product's provider with empty stdin — never the
host's real payload — with the stated intent: *"Providers are
deliberately denied it: it is the one thing here that carries the user's
session... and a provider that cannot receive it cannot render it, log
it, or grow a dependency on it."* Capturing the payload would require
routing through the upstream-command wiring instead, taking on exactly
the cross-repo dependency that design exists to prevent. This is a
genuine cross-repo product trade-off with no technically-correct
default, so it was escalated rather than decided unilaterally.

**Decision: defer.** HORO-1667 ships provider-agnostic — the gateway
typing and the snapshot/delta differ above, neither of which depends on
host-payload capture. If host economics are ever wired in, it must be a
separate adapter/normalization boundary that converts the host-specific
payload into the canonical `EconomicEvent` contract before it reaches
this module, decided and scoped on its own.

Two real gaps a future implementer of that boundary must handle,
recorded here so they are not rediscovered from scratch:

1. `rate_limits.spend_limit.used_percentage` can report **above 100**
   once a subscription exceeds its limit. `ResourceAmount::QuotaPercent`
   clamps to `0.0..=100.0` and documents that domain as hard truth — a
   >100% reading would be **silently clamped**, which is exactly the
   truthfulness loss this campaign exists to prevent. Do not route that
   field through `QuotaPercent` unmodified; carry the raw value
   alongside, or treat it as a domain gap to resolve first.
2. Claude Code's statusline exposes **no cumulative token count** at
   all — only context-window-level and last-API-call-level token
   fields. Never ingest either as a spend-additive delta; they do not
   mean what a cumulative session total would mean.

Audited and rejected outright (not merely deferred): Claude Code's
OpenTelemetry metrics (richest surface available, but wiring an OTLP
receiver is the "no generic observability product" non-goal and
collides with host-env-config ownership); `transcript_path` (never
opened — it contains per-message exact token usage, a signal this
ticket is not entitled to read); Codex (no statusline/status surface at
any level — `host_reported_economic_observation` is `Unavailable` with
`HostExposesNoPrimitive`).

## Privacy

Every type in `economic_ingest.rs` carries only normalized economics,
capability/version metadata, and opaque execution-identity references.
There is no path-typed or content-typed field on `GatewayRequestObservation`
or `HostCounterSnapshot` — this is structural (no field exists to
populate), not a filter a caller could bypass.

## Consequences

- No SQLite migration in this ticket (HORO-1668 owns `0011`); no
  protocol bump; no new crate; `crates/ledger` is untouched.
- `economic_ingest.rs` has two producers with zero live callers today.
  The gateway producer is ready to be wired the moment a caller in
  `crates/daemon`/`crates/gateway` has a `gateway_requests` row and an
  optional `ExecutionIdentity` to pass in. The differ has no producer
  until the deferred host-capture decision is revisited.
