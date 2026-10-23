# ADR-0026: Local monthly shared budget (HORO-2193)

## Status

Accepted by founder decision for **engineering** (Option 2, decisions D1 and
D3, recorded at HORO-2186 Jira comment 28778). This is **not a release GO**:
tagging or publishing v0.0.4 stays a separate founder decision (ADR-0025).
Builds on [ADR-0023](0023-metered-shared-pool.md) and amends
[ADR-0003](0003-gateway-enforcement-boundary.md) for the local budget only.
Slice S2 delivers this record and `calendar_month_bounds`; later slices wire it.

## Context

ADR-0023 shipped a metered shared pool with no production feed, and the
requirements gap matrix (`docs/requirements-gap-matrix-v0.0.4.md`) shows the
original "one shared budget" journey is unmet: monthly caps, per-executor soft
limits, sub-agent attribution, retention and export do not exist. ADR-0023 and
HORO-1915 forbid feeding the pool from provider observation. The founder chose
a local, operator-configured budget as the first real enforceable capacity.

## Decision

### D1: capacity source

- A **locally configured monthly USD cap** is the first real enforceable
  capacity source for the metered pool.
- No corporate quota adapter makes remote enforcement decisions.
- The HORO-1915 boundary is preserved. `quota-observation` never feeds
  admission, and the ban on calling `ingest_pool_provider_snapshot` from
  production code stays (`crates/daemon/tests/metered_pool_no_production_feed.rs`
  keeps guarding it). The local cap is written by the local ledger, not
  derived from a provider reading.

### D3: semantics

1. **One shared hard budget** per period, held in the pool
   `metered:local-budget/monthly-YYYY-MM` (`LOCAL_BUDGET_OWNER`,
   `monthly_pool_alias`).
2. **Explicit boundary and timezone.** The period is a calendar month in a
   **required** IANA timezone. There is no UTC default; an unknown or missing
   zone is a configuration error.
3. **Per-executor soft limits are warn-only.** They never mint capacity and
   never deny.
4. **Every accepted reservation consumes global availability.** Settlement
   reconciles actual usage against the hold.
5. **Crash and expiry recovery.** An active hold that expires is reclaimed
   by the existing stale-hold expiry. A late settle after expiry is still
   recorded (see Rollover).
6. **No over-allocation.** Accepted holds plus settled spend never exceed the
   cap.

### Month window

`calendar_month_bounds(now, &IanaTimeZone) -> (start, end)` returns the
half-open interval `[day-1 00:00 local, next month's day-1 00:00 local)`.

- It uses the same bundled jiff tzdb and `Compatible` gap/fold resolution as
  `fixed_aligned_bounds` (ADR-0015). A midnight inside a gap resolves to the
  later valid instant; inside a fold, to the earlier occurrence.
- The boundary instant belongs to the **new** month; one nanosecond earlier
  belongs to the old.
- It is a free function. `AlignedPeriod` is a public serde enum and gains no
  `Month` variant.
- Month length in hours varies with DST (America/New_York March 2024 is 743).

### Guarantee label

Hooks-only mode is **admission-bounded (cooperative)**. Admission is a hard
decision, but in-turn spend is visible only at Stop, so spend can overshoot
the hold. A per-request hard cap exists only with the gateway in
`governor_held` mode, which is follow-up slice S11 and **not part of this
MVP**. ADR-0003's wording that the gateway is the only enforcement point is
superseded **for the local budget only**, by cooperative admission.

### Meaning of USD

The unit is **Libra-accounted, list-price-equivalent USD at the pinned
`PRICING_VERSION`**. It is never the provider's bill, a plan quota, or an
upstream guarantee. An unpriced model settles at the hold amount with
`usage_known=false` and is surfaced in `doctor`.

### Evidence class

Captures from the real host binary against a fake upstream are
`TestOwnedLive`, never `ProductionObserved`. Nothing in this slice is
`ProductionObserved`.

### Rollover

- A hold belongs to the period it was admitted in.
- A post-hoc charge goes to the period of its **event time**, falling back to
  the parent session's latest-hold pool.
- A late settle after expiry is still recorded.

### Identity and non-duplication

- Executor key: `(host_id, provider, session_id, agent_id | null)`. A missing
  agent never means root.
- Three categories: **chargeable** under a verified parent session; explicit
  **UNKNOWN** (`unattributed`) under a documented contract; **refuse** (empty
  session, reserved prefix, malformed). Never fabricate an identity and never
  silently omit a row.
- Each row is recorded **once**, at the narrowest scope. A session subtree is
  the sum of rows with that session id, the pool is the sum of all rows, and
  "self" is the `agent_absent` rows only.

### Safe failure policy

| Condition | Result |
|---|---|
| No shared-budget file | Pool not consulted; behaviour byte-exact with today |
| File present but invalid (parse, timezone, cap, version) | Deny `config_invalid` |
| Clock anomaly: newest pool period is later than now's period | Deny `clock_anomaly` |
| Daemon unreachable, file present | Deny at prompt admission; allow and log at PreToolUse |
| Unpriced model | Settle at hold amount, `usage_known=false`, shown in `doctor` |

### Egress inventory

Only `crates/gateway` (upstream allowlist, default `api.anthropic.com`) and
`crates/extension` (loopback literal only) may hold HTTP clients. Host
telemetry and WebFetch are Claude Code's own traffic: `doctor` reports them,
Libra does not enforce on them.

### Retention and export

Foundation only: prune closed rows of **ended** periods, with an audit count,
and a **local-only** export with no upload. Monthly automated remote upload is
a **separate, versioned workstream and is not delivered** by this ADR.

## Consequences

- Admission is genuinely enforceable locally for the first time, with an
  honest "cooperative" label instead of a hard-cap claim.
- Overshoot beyond the hold is possible in hooks-only mode and is accepted
  until S11.
- Tests of the month window pin tzdb behaviour (Asuncion 2023-10 gap, Havana
  2026-11 fold); a jiff bump that changes those rules must update them
  deliberately.

## Not decided here

Release GO/NO-GO, remote upload, gateway `governor_held`, any provider-bill
reconciliation, and any change to the ADR-0023 production-feed ban.
