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
The daemon re-reads the file on every decision (see *Daemon behaviour delivered in S4*).
**S5 (HORO-2196, HORO-2190) delivers settlement**: the turn hold is settled
at `Stop` from priced, session-bound transcript usage, and a subagent's
transcript is charged by `ChargeAgentTranscript` (see *Settlement delivered
in S5*). `PROTOCOL_VERSION` is 21. Everything else below is **design intent
implemented in later slices** (S6 hooks and the daemon-unreachable
behaviour, S7 installer, S8 doctor/status, S11 gateway `governor_held`); it
must not be read as shipped behaviour. No real Claude Code host has
exercised any of it: S4's and S5's evidence is `TestOwnedLive`, and S5's
transcripts are synthetic.

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
`usage_known=false` (delivered in S5) and names the model in `Status`
`unpriced_models`; the `doctor` presentation is S8. The
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
| Unpriced model | Settle at hold amount, `usage_known=false`, named in `Status.unpriced_models` (S5; `doctor` is S8) |

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
answers `config_invalid`; the caller (hook runtime, S6) must retry or spool it.

**Heartbeat and over-reservation.** A tool event renews the session's active
holds in the current period's pool (it carries no agent id, so the lane cannot
be preferred). Since S5 the main-executor hold clears at `Stop`; a subagent
lane's hold (admitted with an `agent_id`) and any hold whose `Stop` never
arrives or was refused (transcript mismatch) still clear only by expiry and
renewal, so such a hold can stay reserved while tool events continue. This
over-reserves; it never under-reserves.

### Settlement delivered in S5 (HORO-2196, HORO-2190)

**What is measured.** A session's `Stop` (`Request::Finalize`, when a
shared-budget file is in force; with none the path is not consulted and
`Finalize` is unchanged) settles the lane's `(session, agent_absent)` turn
hold exactly once, with `charge_source = turn_hold`. The actual is the
transcript window's usage priced in **USD cents**: every assistant record's
`model` and all four token tiers (input, output, cache-creation,
cache-read; `fresh_tokens()` remains the token-budget rule only), priced
from the gateway `PricingTable` plus the file's optional `pricing_overrides`
(an addition to the S4 file, `schema_version` stays 1; all four prices per
model, finite, 0-100000 USD/Mtok, at most 64 models). Rounding: summed
exactly in integer pico-dollars and rounded **up** to a whole cent **once
per Stop window** (never per record), so the error is below one cent per
window and never downward. The actual may exceed the hold: the overrun is
recorded unclamped and later `Admit`s are refused while `remaining < hold`.
Dedup by `message.id` (HORO-1882) is unchanged; an assistant record with
all-zero usage is free whatever its model.

**What is not counted.** Records flagged sidechain are excluded from the
parent window (they belong to the subagent charge). A `usage` object on a
tool-result line (the Agent tool's `tool_response.usage`) is never read.
Nothing is measured from the gateway, a provider bill or an inferred
source. Known pricing limitations above still apply.

**Session binding (HORO-2190).** Before any window is charged every usage
record in it must carry the transcript's native per-line session field,
equal to the hook's `session_id`; path, cwd and timestamps are never used to
infer ownership.

| Window | Result |
|---|---|
| all records present and equal | charged |
| any record present and **different** (session B's hook given A's file) | charge **nothing**, hold left to its TTL, `transcript_binding_mismatches++` (the spend is counted by its true session's own Stop) |
| otherwise any record missing/unreadable field | settle the hold at the **hold amount**, `usage_known=false`, `transcript_unverified++` |

One foreign record makes the whole window a mismatch; a window is never
charged partially.

**Evidence status of the field names: UNCONFIRMED.** The field names live in
two constants (`TRANSCRIPT_SESSION_FIELD = "sessionId"`,
`TRANSCRIPT_SIDECHAIN_FIELD = "isSidechain"`, `usage/priced.rs`). This
repository holds no transcript fixture, characterization document or test
that evidences them (the Claude Code hook payload's `session_id` is not a
transcript line); they are the names the public Claude Code JSONL format is
understood to use, mirrored by synthetic fixtures. Real-host confirmation is
a later step. A wrong session name fails safe (every window unverifiable,
settled at the hold amount); a wrong sidechain name would let a parent window
include subagent records, so the subagent charge must not be relied on until
it is confirmed.

**Window watermark (no migration).** A window is every record **strictly
after** the lane's watermark: the latest `settled_at` of the lane's settled
holds, or the oldest hold's `created_at` when none has settled. It is derived
from existing ledger rows and advances only when a hold settles, so two
consecutive Stops never count a record twice, and a **crashed turn** (no
Stop) is measured by the next Stop (its hold expired unsettled; the
watermark did not move) while the abandoned hold is reclaimed by the
existing TTL sweep. A late `Stop` after the hold expired still settles in
the hold's own row and period.

**Settled at the hold amount, `usage_known=false`, never zero:** no
transcript path; missing, unreadable, truncated-below-the-watermark or
rotated transcript (no usage after the watermark); a window over the 32 MiB
scan cap; a usage record with no timestamp; an unpriced model (named in
`unpriced_models`); an unverifiable session field; and an invalid
configuration file at `Stop` (settles the hold in its own pool; overrides
from an untrusted file are not used). With no hold nothing is written.

**Known limits.** Claude Code writes its transcript asynchronously: a record
stamped before a `Stop` but flushed after it falls behind the watermark and
is not counted (the window settled measured-short or at the hold amount).
`Finalize` carries no idempotency key: a retry before the next `Admit` is a
no-op, but a duplicate delivered after the next `Admit` would settle the
new hold, so the S6 hook must not resend. Status telemetry (counters,
`unpriced_models`) is process memory and restarts at zero; the ledger's
`usage_known` flag is the durable record.

**Subagent charge.** `ChargeAgentTranscript { session_id, agent_id,
agent_transcript_path }` makes the daemon read the subagent's own file
(32 MiB cap, same binding rule, all records counted) and book only
`cumulative - already charged` with source `subagent_transcript`, executor
`agent` (or `unattributed` when no `agent_id`: no id is ever invented, and
each unattributed transcript path is its own family). "Already charged" is
the sum of every row under the `(session, agent-or-path)` key prefix across
periods, so a repeated `SubagentStop` books nothing and a larger file books
the difference; charges are never refunded. Parent windows exclude
sidechain records, so parent + subagent never double count (pinned by a
fixture test asserting the pool total equals the sum of executors).

*Unmeasured subagent fallback* (missing/unreadable/relative path, over the
cap, empty, unpriced model, unverifiable session): never silently omitted;
`subagent_unmeasured_charge_usd_cents` (else `turn_hold_usd_cents`) is
charged **once per family** with `usage_known=false` and counted in
`unmeasured_charges`. It counts toward already-charged, so a later
measurement books only the excess. A transcript naming another session is
refused (`transcript_session_mismatch`) and charges nothing, not even the
fallback.

**Not done in S5:** no hook runtime or installer (S6/S7), no `doctor`/status
CLI (S8), no real host evidence, no gateway path (S11). `Finalize` itself
returns nothing about the settlement; it is observable through `Status`.

### Egress inventory

Only `crates/gateway` (upstream allowlist, default `api.anthropic.com`) and
`crates/extension` (loopback literal only) may hold HTTP clients. Host
telemetry and WebFetch are Claude Code's own traffic: Libra neither reports
nor enforces them today; slice S8 (HORO-2217) adds `doctor` reporting of the
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` setting (reporting only).

The full pathway table, what is enforced and what is not, is in
[`docs/egress-inventory.md`](../egress-inventory.md); a static test
(`crates/cli/tests/egress_static_guard.rs`) enforces the crate boundary.

### Retention and export (design intent; implemented in slices S4-S8)

Foundation only: prune closed rows of **ended** periods, with an audit count,
and a **local-only** export with no upload. Monthly automated remote upload is
a **separate, versioned workstream and is not delivered** by this ADR.

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
