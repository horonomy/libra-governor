# ADR-0026: Local monthly shared budget (HORO-2193)

## Status

Accepted by founder decision for **engineering** (Option 2, decisions D1 and
D3, recorded at HORO-2186 Jira comment 28778). This is **not a release GO**:
tagging or publishing v0.0.4 stays a separate founder decision (ADR-0025).
Builds on [ADR-0023](0023-metered-shared-pool.md) and amends
[ADR-0003](0003-gateway-enforcement-boundary.md) for the local budget only.
Slice S2 delivers this record and `calendar_month_bounds`; later slices wire it.

**Implementation status.** `calendar_month_bounds`, `LOCAL_BUDGET_OWNER`,
`monthly_pool_alias` and the ledger's local-capacity primitives (S2/S3) exist.
**S4 (HORO-2195) delivers the daemon core**: `shared-budget.json` loading,
`Request::SharedBudget` (`Admit`, `Check`, `ChargeAgent`, `Status`),
admission against the locally configured cap, month rollover, the failure
rows `config_invalid` and `clock_anomaly`, and the tool-event heartbeat.
The daemon re-reads the file on every decision (see *Daemon behaviour delivered in S4*). `PROTOCOL_VERSION` is 20. Everything else below is **design intent
implemented in later slices** (S5 settlement from transcripts, S6 hooks and
the daemon-unreachable behaviour, S7 installer, S8 doctor/status, S11
gateway `governor_held`); it must not be read as shipped behaviour. No real
Claude Code host has exercised any of it: S4's evidence is `TestOwnedLive`.

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

### D3: semantics (design intent; implemented in slices S4-S8)

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

Hooks-only mode is **admission-bounded (cooperative)**. Admission is
refuse-before-spend at prompt admission (cooperative), but in-turn spend is visible only at Stop, so spend can overshoot
the hold. A per-request hard cap exists only with the gateway in
`governor_held` mode, which is follow-up slice S11 and **not part of this
MVP**. ADR-0003's wording that the gateway is the only enforcement point is
superseded **for the local budget only**, by cooperative admission.

### Meaning of USD

The unit is **Libra-accounted, list-price-equivalent USD at the pinned
`PRICING_VERSION`**. It is never the provider's bill, a plan quota, or an
upstream guarantee.

Unpriced model, by path: hooks-only settlement uses the hold amount with
`usage_known=false`, surfaced in `doctor` (design intent, S5/S8). The
gateway's USD budget path instead refuses with `403 unpriced_model`
(`crates/gateway/src/pricing.rs` module docs; `Decision::UnpricedModel` in
`crates/gateway/src/proxy.rs`).

#### Known pricing limitations (pricing refresh, PR #208)

The pinned table is list-price-equivalent and **not exact**:

- (a) The 1-hour cache-write tier (2x base input) is not modelled; those
  tokens are priced at the 5-minute rate, so spend is **under-accounted**.
- (b) Fast-mode premium and the `inference_geo` multiplier (1.1x, US-only)
  are not modelled, so spend is **under-accounted** for such requests.
- (c) Haiku 5.5 prompts under 100k tokens are priced at the over-100k rate,
  so they are **over-accounted** by about 5x.

The cap is therefore a Libra-accounted estimate that may drift from the
provider bill in either direction and is never a bill guarantee. The table
is operator-overridable through `PricingTable::with_overrides`.

### Evidence class

Captures from the real host binary against a fake upstream are
`TestOwnedLive`, never `ProductionObserved`. Nothing in this slice is
`ProductionObserved`.

### Rollover (design intent; implemented in slices S4-S8)

- A hold belongs to the period it was admitted in.
- A post-hoc charge goes to the period of its **event time**, falling back to
  the parent session's latest-hold pool.
- A late settle after expiry is still recorded.

### Identity and non-duplication (design intent; implemented in slices S4-S8)

- Executor key: `(host_id, provider, session_id, agent_id | null)`. A missing
  agent never means root.
- Three categories: **chargeable** under a verified parent session; explicit
  **UNKNOWN** (`unattributed`) under a documented contract; **refuse** (empty
  session, reserved prefix, malformed). Never fabricate an identity and never
  silently omit a row.
- Each row is recorded **once**, at the narrowest scope. A session subtree is
  the sum of rows with that session id, the pool is the sum of all rows, and
  "self" is the `agent_absent` rows only.
  (Design intent; implemented in slices S4-S8.)

### Safe failure policy (design intent; implemented in slices S4-S8)

| Condition | Result |
|---|---|
| No shared-budget file | Pool not consulted; `SharedBudget` requests answer `not_configured` and write nothing (pinned at daemon level by `shared_budget_core.rs`; the hook-level byte-exact guarantee is slice S6) |
| File present but invalid (parse, timezone, cap, version) | Deny `config_invalid` |
| Clock anomaly: newest pool period is later than now's period | Deny `clock_anomaly` |
| Daemon unreachable, file present | Deny at prompt admission; allow and log at PreToolUse |
| Unpriced model | Settle at hold amount, `usage_known=false`, shown in `doctor` |

### Daemon behaviour delivered in S4 (HORO-2195)

**Hot reload, fail closed.** The daemon consults `shared-budget.json` on
every `Admit`, `Check`, `ChargeAgent` and heartbeat (a stat, re-read when the
file changed or was just modified), not only at start:

| File after start | Result |
|---|---|
| Created | Enforced from the next decision |
| Edited, valid | New cap applies to the next decision. A lower cap never revokes existing holds; it denies new ones while `remaining < hold` |
| Edited, invalid or half-written | `config_invalid` (deny) until it is valid again; existing holds are kept |
| Deleted | Treated as absent (`not_configured`): deleting the file is read as the operator removing the budget. `Status` shows `config_state: absent` and `config_changed_since_start: true` |

**Trust boundary of the file.** Any process running as the same OS user can edit or delete `shared-budget.json` (or swap it with a same-size file that keeps an older mtime, which the `(mtime, size)` change detector does not notice until the next edit). The cap is therefore cooperative, not tamper-proof, consistent with the same-user boundary in ADR-0023.

**Operator recovery from `clock_anomaly`.** A forward clock jump that created
a pool for a later month makes every `Admit` and `Check` deny `clock_anomaly`
until real time reaches that month. `Status` names it in
`future_pool_detected`. Recovery: correct the system clock. If a pool was
created by a clock that has since been corrected, there is no repair command
yet (owned by S8's `shared-budget` tooling); the manual step is to stop the
daemon and delete that month's `quota_pools` row (and its reservations) from
`ledger.sqlite3` after confirming it holds no real spend. Until then the
budget fails closed rather than guessing. Changing `time_zone` near a month
edge can produce the same state for up to about a day, since the same instant
may name a different month.

**Charges.** `ChargeAgent` is idempotent across periods: a retry resolves to
the pool of the first attempt, found by key across all local-budget pools. An
event time in a later period than now is booked to the current period and
returned with `event_time_clamped: true`, because the spend already happened;
only years outside 2000-2200 are refused. One charge above ten times the
monthly cap, or one that would push a pool total past a safe range, is refused
(`amount_too_large`). Against an invalid file a charge records nothing and
answers `config_invalid`; the S5 caller must retry or spool it.

**Heartbeat and over-reservation.** A tool event renews the session's active
holds in the current period's pool (it carries no agent id, so the lane cannot
be preferred). Until S5 settles holds at Stop, a hold clears only by expiry
and renewal, so a finished or subagent hold can stay reserved while tool
events continue. This over-reserves; it never under-reserves.

### Egress inventory

Only `crates/gateway` (upstream allowlist, default `api.anthropic.com`) and
`crates/extension` (loopback literal only) may hold HTTP clients. Host
telemetry and WebFetch are Claude Code's own traffic: Libra neither reports
nor enforces them today; slice S8 (HORO-2217) adds `doctor` reporting of the
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` setting (reporting only).

The full pathway table, what is enforced and what is not, is in
[`docs/egress-inventory.md`](../egress-inventory.md); a static test
(`crates/cli/tests/egress_static_guard.rs`) enforces the crate boundary.

### Retention and export (prune and local export implemented in S10; upload not built)

Foundation only: prune closed rows of **ended** periods, with an audit count,
and a **local-only** export with no upload. Monthly automated remote upload is
a **separate, versioned workstream and is not delivered** by this ADR.

**Implementation status (S10, HORO-2219).** Implemented:
`libra-governor shared-budget prune` and `shared-budget export`.

- `prune` is a dry run (deletes nothing) unless `--apply` is given and
  requires `--keep-months >= 1`. `--time-zone` defaults to the zone in
  `<state-dir>/shared-budget.json`, must equal it when both are given, and
  is required when there is no valid file. It deletes closed rows of
  `local-budget` `monthly-YYYY-MM` pools whose period ended before the last
  N closed months, one `BEGIN IMMEDIATE` transaction per pool. It never
  touches the current or a future period, a pool with any `active` row, or
  an unrecognised alias, and it reports counts only. A dry run and `export`
  open the ledger read-only (no migration, no WAL checkpoint) and refuse a
  schema that is not current.
- **Export before prune.** Pruned periods can no longer be exported, and a
  pruned period also drops its idempotency-key memory: replaying a charge
  older than the retention window is treated as new (a backdated charge
  whose event time falls in a pruned period would recreate the pool and
  book again). Follow-up, not done here: the daemon should refuse or clamp
  an `event_time` older than the retention horizon.
- `export --period YYYY-MM` writes a local `0600` file (schema
  `libra.shared-budget-export.v1`, per-executor aggregates plus pool
  totals, SHA-256 over the canonical body, no content). Session and agent
  ids are **pseudonymised** by default (32-bit unsalted digest; guessable
  for low-entropy ids; linkable across exports); this is not anonymisation
  and a redacted export is not safe to publish. `--raw-ids` opts out.

Upload is **not built**: no transport, retry, resume or de-duplicated
delivery exists; that remains the separate, versioned workstream above.

## Consequences

- Once slices S4-S8 land (design intent), refusal before spend at prompt
  admission becomes enforceable locally, with an honest "cooperative" label
  instead of a hard-cap claim.
- Overshoot beyond the hold is possible in hooks-only mode and is accepted
  until S11.
- Tests of the month window pin tzdb behaviour (Asuncion 2023-10 gap, Havana
  2026-11 fold); a jiff bump that changes those rules must update them
  deliberately.

## Not decided here

Release GO/NO-GO, remote upload, gateway `governor_held`, any provider-bill
reconciliation, and any change to the ADR-0023 production-feed ban.
