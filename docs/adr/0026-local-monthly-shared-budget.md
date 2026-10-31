# ADR-0026: Local monthly shared budget (HORO-2193)

## Status

Accepted by founder decision for **engineering** (Option 2, decisions D1 and
D3, recorded at HORO-2186 Jira comment 28778), and for the **v0.0.4
Cooperative Preview release scope and disclosure wording** effective only with
the SHA-named founder GO recorded under ADR-0025 (HORO-2186 comment 29520, SHA
`313d26a09407c8a77d2b4cc7685ea694a822905a`). This ADR is **not itself a release GO**: tagging or
publishing v0.0.4 stays a separate founder decision (ADR-0025). The required
disclosures (the five listed under "Release disclosure" below, plus the
additional fail-open, pricing, trust-boundary and limitation disclosures in the
v0.0.4 release notes) are final for the release notes; the release dispositions
D2/D3/D4/D5/D7 are recorded in ADR-0025.
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
in S5*). `PROTOCOL_VERSION` was 21 at S5, is 22 since HORO-2197, is 23 since
HORO-2246 (the `TranscriptReady` probe) and is 24 since HORO-2250 (`Finalize`
carries the `Stop`'s `prompt_id`). The S6 Claude Code hook gate and the S10
retention/export commands landed on `main` separately and are recorded in
their own commits and in *Retention and export* below.
**S7 (HORO-2216) delivers the installer and the settlement hooks**:
`libra-governor install` writes `PreToolUse`, `SubagentStop` and `SessionEnd`
entries beside the original three, and `hook subagent-stop` /
`hook session-end` send `ChargeAgentTranscript` / `SettleTail` (see
*Installer and settlement hooks delivered in S7*).
**HORO-2246 fixes the flush lag at `Stop`**: the `Stop` hook now waits,
bounded and never denying, until the daemon reports the turn's transcript
readable before it settles (see *Flush lag at Stop*); SettleTail's
description below is corrected.
**S8 (HORO-2217) delivers the operator surfaces**: `libra-governor
shared-budget status [--json]`, a budget segment on the legacy statusline,
and the shared-budget checks in `doctor` (see *Status, statusline and doctor
delivered in S8*). **Not built**: the S11 gateway hard cap (`governor_held`),
a Codex budget gate, remote upload, and any real-host evidence (everything is
`TestOwnedLive`; nothing is `ProductionObserved`). Everything else below that
is not delivered by a slice named above is **design intent**; it must not be
read as shipped behaviour. No real Claude Code host has
exercised any of it as an installed hook: S4's and S5's evidence is
`TestOwnedLive`; S5's transcript field names were confirmed against a real
Claude Code 2.1.306 binary in an isolated harness, and its fixtures mirror
that shape with synthetic ids.

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
`unpriced_models`; `shared-budget status` and `doctor` show it (S8). The
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
| Unpriced model | Settle at hold amount, `usage_known=false`, named in `Status.unpriced_models` (S5; shown by `status` and `doctor`, S8) |

### Daemon behaviour delivered in S4 (HORO-2195)

**Hot reload, fail closed.** The daemon consults `shared-budget.json` on
every `Admit`, `Check`, `ChargeAgent` and heartbeat (a stat, re-read when the
file changed or was just modified), not only at start:

| File after start | Result |
|---|---|
| Created | Enforced from the next decision |
| Edited, valid | New cap applies to the next decision. A lower cap never revokes existing holds; it denies new ones while `remaining < hold`. A re-prompt (an `Admit` naming a prompt the live hold has not accepted) is a new turn: it renews the hold only while `remaining + that hold >= turn_hold`, otherwise `cap_exhausted` |
| Edited, invalid or half-written | `config_invalid` (deny) until it is valid again; existing holds are kept |
| Deleted | Treated as absent (`not_configured`): deleting the file is read as the operator removing the budget. `Status` shows `config_state: absent` and `config_changed_since_start: true` |

**Trust boundary of the file.** Any process running as the same OS user can edit or delete `shared-budget.json` (or swap it with a same-size file that keeps an older mtime, which the `(mtime, size)` change detector does not notice until the next edit). The cap is therefore cooperative, not tamper-proof, consistent with the same-user boundary in ADR-0023.

**Operator recovery from `clock_anomaly`.** A forward clock jump that created
a pool for a later month makes every `Admit` and `Check` deny `clock_anomaly`
until real time reaches that month. `Status` names it in
`future_pool_detected`. Recovery: correct the system clock. If a pool was
created by a clock that has since been corrected, there is no repair command
yet (S8's `status` and `doctor` report the anomaly but do not repair it); the
manual step is to stop the
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
answers `config_invalid`; the caller (the hook runtime) must retry or spool it.

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
One API message written as several content-block lines (same `message.id`,
identical `usage`, `apiBlockIndex` 0, 1, ...) is counted once per window and
across windows (see *Window watermark*). Within a window usage is the
per-tier **maximum across all occurrences** of an id, however far apart; across
windows a persisted id is skipped by id alone, so a streaming partial with a
smaller usage that is counted first and a larger final usage flushed later
would be under-counted (a residual; real Anthropic ids are unique per message
and usage is identical across the blocks on 2.1.306). An assistant record with all-zero
usage is free whatever its model; a counted record with no `message.id` is
its own message and is reported in `Status.id_less_records`.

**What is not counted.** Records flagged sidechain are excluded from the
parent window (they belong to the subagent charge). A `usage` object on a
tool-result line (the Agent tool's `tool_response.usage`) is never read.
Nothing is measured from the gateway, a provider bill or an inferred
source. Known pricing limitations above still apply.

**"Fails safe" is conservative only if the hold covers the turn.** Every
window that cannot be measured settles at the flat hold amount. That is an
over-count only when `turn_hold_usd_cents` is at least the real cost; an
expensive turn that is unverified, unpriced, malformed, scan-capped or a
binding mismatch is **under-counted** by the difference. The cap is an
estimate; a larger hold is the only lever.

**Session binding (HORO-2190).** Before any window is charged every usage
record in it must carry the transcript's native per-line session field,
equal to the hook's `session_id`; path, cwd and timestamps are never used to
infer ownership.

| Window | Result |
|---|---|
| every usage line is this session's | charged |
| some lines name **another** session, some are this session's | the foreign lines are **ignored** (counted in `Status.foreign_lines`, never priced or imported); this lane's own lines are priced normally and the watermark advances only to the newest **own** counted record |
| usage lines exist but **none** is this session's (session B's hook handed A's file) | foreign spend is never imported; the lane hold is settled at the **hold amount**, `usage_known=false`, reason `binding_mismatch`, `transcript_binding_mismatches++`. The watermark does **not** move (it never jumps to now), so the lane's own earlier unmeasured records are not skipped; foreign lines older than the mark are excluded by timestamp and newer ones are ignored at no cost. A mismatch is a flagged conservative settle, not an omission; the spend is counted by its true session's own Stop |
| any usage line whose session field is absent / unreadable | the line could be ours: settle the hold at the **hold amount**, `usage_known=false`, `transcript_unverified++` |
| a usage count that is not a non-negative integer | `usage_malformed`: the hold amount, `usage_known=false` (never dropped silently) |
| a record stamped later than `now + 5 min` | `future_timestamp`: see *Future-stamped records* |

A subagent file follows the same rule with the per-line `agentId` as a second
binding: lines of another session or another agent are ignored; a non-empty
file with **no** own usage line takes the unmeasured fallback once per family,
flagged `binding_mismatch`.

**Future-stamped records.** A record stamped later than `now + 5 minutes`
(clock skew, a bad line, a hostile same-user file) is inconsistent: it is not
counted, and a watermark is never stored later than that bound (nor with a
year outside 2000-2200). The in-bound own records of such a window are
still measured: it settles at **max(priced, hold)**, reason
`future_timestamp` (recorded as measured only when the priced amount exceeds
the hold; otherwise the hold amount, `usage_known=false`). The watermark
advances at most to the newest in-bound own record, and only when those
records were priced (otherwise it does not move and they are rescanned). The
future-stamped lines are **never** remembered as counted: they stay after the
mark and are re-evaluated every window, so each is counted exactly once, when
the clock brings its stamp inside the horizon. The cost of leaving such a
line in the file is that every window it appears in books at least the hold
(`max(priced, hold)`). As defence in depth,
`shared_lane_watermark` ignores any stored mark later than that row's
`settled_at` plus ten minutes, so an already-poisoned row cannot hide later
records; it returns at most 512 `message.id`s, newest row first, deduplicated
linearly. `SettleTail` applies the same bound and charges nothing for such a
window.

**Evidence status of the field names: CONFIRMED on Claude Code 2.1.306
(2026-10-24)** against a real binary (isolated HOME, fake loopback
upstream, network-deny sandbox, synthetic prompts only). The constants are
`TRANSCRIPT_SESSION_FIELD = "sessionId"` and
`TRANSCRIPT_SIDECHAIN_FIELD = "isSidechain"` (`usage/priced.rs`). Observed:
main-transcript lines carry `sessionId` equal to the hook `session_id` and
`isSidechain: false`; subagent lines are in a **separate** file
`<session-dir>/subagents/agent-<agent_id>.jsonl` (session-dir = the hook's
`transcript_path` without `.jsonl`) with `isSidechain: true`, `agentId`, and
the **parent's** `sessionId`; `SubagentStop` carries `agent_id`,
`agent_type`, `agent_transcript_path` and the parent's `session_id`; each
subagent file holds only its own agent's lines (it is cumulative for that
agent, not across agents); `--resume` keeps the same `sessionId` and appends
to the same file. Hook payload facts: `UserPromptSubmit` has `prompt_id`,
`permission_mode`, `prompt`; `Stop` has `prompt_id`, `last_assistant_message`,
`stop_hook_active`, `background_tasks`; `SessionEnd` has `reason`; there is no
turn id other than `prompt_id`; `agent_id` looks like `a` + hex.

**Real-host verification checklist** (what must hold on a real host before
the subagent path is trusted; S8 re-verifies):

1. Main-transcript lines carry `sessionId` equal to the hook `session_id` -
   **confirmed** (2.1.306).
2. Resumed session: same `sessionId`, same file - **confirmed**. A
   post-`/compact` transcript - **NOT yet verified**.
3. Subagent lines live in `subagents/agent-<id>.jsonl` with the parent's
   `sessionId`; `SubagentStop.agent_transcript_path` exists; each file holds
   only its own agent - **confirmed**. Whether the file is complete at
   `SubagentStop` - **no**, see 4.
4. Timestamp monotonicity and flush lag - **observed, not monotonic**:
   millisecond inversions (`.033` then `.032`), timestamp-less lines
   (`atis-latch`, `last-prompt`, `cost-state`, `mode`), and a systematic
   flush lag: at `Stop` the turn's final `end_turn` line is often not yet in
   the file (on 2.1.306 `-p` the whole turn appears about 70 ms after the
   hook is dispatched, measured in *Flush lag at Stop*), and at
   `SubagentStop` a subagent's file may hold no assistant line yet.
   Multi-second inversions are covered by a ten-minute slack; larger
   inversions are not measured.
5. Repeated `message.id` across content blocks - **confirmed**: split lines
   with identical usage.

**Window watermark (migration 0022, nullable columns, no backfill).** A
window is every record **strictly after** the lane's watermark: the newest
record timestamp **actually counted** by an earlier window, persisted on the
settled row (`watermark_ts`, separate from `settled_at`) together with the
`message.id`s counted up to it (`watermark_message_ids`, at most 512). Rows
with no mark (legacy, or windows that counted nothing) fall back to the
settle time of the latest settled hold, else the oldest hold's creation; a
window that counts nothing keeps the previous mark. Because the host
flushes asynchronously, a record stamped before a Stop is routinely written
after it; a wall-clock watermark would lose it for good, this one counts it
at the next Stop. A record the host flushed before a window's Stop is not
counted by the next window (the mark is the newest counted record). Windows
that cannot be measured (unpriced, malformed, unverified, scan-capped,
future-stamped, mismatched) write a NULL mark, so the next window rescans
those records and counts them again (an over-count on top of the flat hold:
the safe direction); and one persistently unpriced, malformed or unverified
record keeps every later window on the flat hold until it ages past the
watermark slack or a mark is written. A **crashed turn** (no Stop) is measured by the next Stop while the abandoned
hold is reclaimed by the existing TTL sweep. A late `Stop` after the hold
expired still settles in the hold's own row and period.

*Reverse scan.* The scan does not stop at the first old record (a user,
tool-result or sidechain line with an older stamp would hide newer assistant
lines above it). It keeps going until records are older than the watermark
by ten minutes (`REORDER_SLACK`) or the 32 MiB byte bound, which is
reported as unmeasured (hold amount, flagged), never a partial sum; only
records strictly newer than the watermark are counted, and timestamp-less
lines are ignored for the watermark.

*Residuals.* A record stamped at or before an already-counted record but
flushed later is not recovered. A `message.id` whose blocks straddle the
watermark is protected by the persisted id set; if an id aged out of that
bounded set, or a host omits ids, tail blocks may be counted again (an
over-count, the safe direction). Inversions larger than the slack can hide
newer records. A turn whose `Stop` could not read the transcript is settled at
the flat hold and is **never re-measured**: its late lines are either behind
the next lower bound (no earlier mark: lost, the hold already covers them) or
re-counted on top of the hold by the next `Stop` (an earlier mark exists: an
over-count). `SettleTail` does not change that (see below). The `Stop` hook's
bounded wait ([*Flush lag at Stop*](#flush-lag-at-stop-horo-2246)) exists so
that this rarely happens.

**Settled at the hold amount, `usage_known=false`, never zero:** no
transcript path; missing, unreadable, truncated-below-the-watermark or
rotated transcript (no usage after the watermark); a window over the scan
cap; a usage record with no timestamp; malformed counts; an unpriced model
(named in `unpriced_models`); an unverifiable session field; a session
mismatch; and an invalid configuration file at `Stop` (settles the hold in
its own pool; overrides from an untrusted file are not used). With no hold
nothing is written.

**Subagent charge.** `ChargeAgentTranscript { session_id, agent_id,
agent_transcript_path }` makes the daemon read the subagent's own file
(32 MiB cap, same session binding plus the per-line `agentId`, all records
counted) and book only `cumulative - already charged` with source
`subagent_transcript`, executor `agent` (or `unattributed` when no
`agent_id`: no id is ever invented, and each unattributed transcript path is
its own family). "Already charged" is the sum of every row under the
`(session, agent-or-path)` key prefix across periods, so a repeated
`SubagentStop` books nothing and a larger file books the difference;
charges are never refunded. Parent windows exclude sidechain records, so
parent + subagent never double count (pinned by fixture tests).

*Stop sweep.* A subagent's file lags its own `SubagentStop`, so every
`Finalize` also visits `<transcript_path minus .jsonl>/subagents/agent-*.jsonl`
(at most 64 regular files) and charges each file's delta through the same
logic. A sweep reads at most 128 MiB of subagent files in total; files past
that are deferred to the next `Stop` (`Status.sweep_deferred_files`). A
file is charged against the budget at no more than one read can scan (32 MiB),
and the first file of every sweep is always processed, so a file larger than
the whole budget is never starved. A file the read cap truncates takes the
flagged unmeasured fallback once (it is not skipped on every sweep). A file
whose `(size, mtime)` is unchanged since its last sweep is skipped (daemon
memory only; a restart re-checks each file once, which the delta logic makes
harmless). A file with nothing measurable yet is **skipped** by the
sweep (not billed the fallback); a genuine problem still takes it.

*Unmeasured subagent fallback* (missing/unreadable/relative path, over the
cap, empty, unpriced, malformed, unverifiable session, or a **session
mismatch**): never silently omitted; `subagent_unmeasured_charge_usd_cents`
(else `turn_hold_usd_cents`) is charged **once per family** with
`usage_known=false` and counted in `unmeasured_charges`. A mismatch also
counts in `transcript_binding_mismatches` and is flagged `binding_mismatch`;
the foreign spend is not imported. The fallback counts toward
already-charged, so a later measurement books only the excess.

**`SettleTail { session_id, transcript_path }`** (for `SessionEnd`, wired by
S7, delivered). Charges the main-transcript records past the lane's watermark that no
`Stop` **counted** as a settled `agent_absent`
row (`charge_source = turn_hold`, `usage_known = true`) with the same
binding, de-duplication, future-bound and watermark; idempotent; nothing is
charged when nothing is measurable (a tail has no hold to fall back to; the
response flags `unmeasured`/`binding_mismatch`). It is refused with
`no_settled_lane` unless the lane already has a settled hold or a watermark
(a session never admitted or settled under this budget must not be billed its
whole history); its lower bound is the same as a `Stop`'s: the lane's
watermark, else the latest settle time (never the earliest hold, since
earlier unmeasured settlements were already booked at the hold amount).
**What it does and does not recover.** The lower bound is the newest *measured*
mark. A `Stop` settled at the flat hold writes no mark, so with no earlier mark
the bound is that settlement's instant, and a line the host flushed late but
stamped before it is behind the bound: SettleTail charges 0 for it. That is
deliberate, not a gap: the hold already booked that turn, so recovering the
line would charge it twice. SettleTail therefore recovers only records after the
last measured mark, in practice the lines a measured `Stop` left behind (a
message flushed after that `Stop` read). Earlier wording that it recovers "the
final message of a session" was wrong for a flat-hold settlement, and a
real-host run (HORO-2246) showed 0c for it every time. The charge row and its watermark are written in **one ledger
transaction** (`charge_shared_with_watermark`), so a crash cannot leave a
charge without its mark; an identical replay of a row that lacks a mark
adopts it. `turn_hold` is reused for the source
because the column's CHECK constraint cannot take a new value without a table
rebuild; the row is distinguishable as a charge row (`is_charge = 1`) and by
its `tail:` idempotency key.

**Telemetry is volatile.** The `Status` counters (`unpriced_models`,
`transcript_binding_mismatches`, `transcript_unverified`,
`unmeasured_settlements`, `unmeasured_charges`, `id_less_records`) are daemon
memory and reset on restart. The durable record is the ledger
(`usage_known=false` rows) plus a structured log line for every flagged
event (`lane=<8 hex of sha256(session)>`, reason; never a session id, agent
id, path or content).

**Known limits (tracked follow-ups, not implemented here).**

- *Duplicate Finalize after the next Admit (M1), fixed by HORO-2250 where the
  host names the prompt.* Reproduced: Stop #1 settles hold H1; the user
  prompts and Admit creates H2; a duplicate of Stop #1 settled H2 early from an
  empty window (the hold amount), booking a turn that had not happened. Now
  `Finalize` carries the `Stop`'s optional `prompt_id` (protocol v24, relayed by
  `hook stop`), and a `Stop` naming a prompt whose own hold has already settled
  settles nothing. Every prompt id a hold accepts is recorded, in the same
  transaction as the admit or renewal, in `shared_hold_prompts` (migration 0024,
  keyed by principal, session and prompt id, pointing at the hold). A re-prompt
  renews the hold and keeps its first idempotency key, so the table is what
  makes a replayed `Stop(p2)` recognisable after the lane's next hold exists
  (a `Stop` whose prompt maps to a hold other than the pending one settles
  nothing). The row is not tied to the hold or its pool, so it survives a
  retention prune of the hold, and it is durable across daemon restarts. The
  first-prompt key check is only the fallback for a prompt with no mapping (a
  hold admitted before migration 0024); when a mapping exists it alone decides.
  **Occurrence contract (QA 29119):** a main-lane prompt id names one turn, and
  its binding is stable. An `Admit` whose id a hold already accepted is served
  only while that hold is still the lane's live one (an idempotent retry or
  renewal). Otherwise (the hold settled, expired, was released or pruned, and a
  successor may exist) the `Admit` is a delayed or re-sent replay: it is
  refused as `key_conflict`, writes nothing, neither renews nor reserves, and
  never rebinds the id to the successor, so a stale `Stop` cannot settle a turn
  that never ran. This is a fail-closed change: a host that reused one prompt id
  for two real turns would have its second `Admit` refused (observed host ids
  are unique per prompt; interactive reuse is unobserved). Only the **main** lane records or renews a mapping: a
  subagent `Admit` carrying the parent's prompt id cannot take it over. The
  grant and its row commit together (`reserve_shared_metered_for_prompt`, one
  outer transaction), as does a renewal. No session-erase or export path reads
  or deletes this table, `shared_charge_floor` or `shared_retention_horizon`
  (export is per pool; nothing erases sessions), so nothing needs to cover them.
  **Residual:** the guard needs the id. A `Stop` without one (an older hook, a
  host that omits it) is indistinguishable from a real `Stop` whose transcript
  has not flushed and still settles the pending hold. The table grows by one
  small row per prompt and is not pruned. A prompt id longer than 256 bytes is
  treated as absent (never truncated, so two ids cannot collide), which bounds
  that row's size. Holds admitted before migration 0024 have no mapping, so a
  replayed `Admit` of their first prompt is not refused by the contract above
  (legacy behaviour). An admit that fails after its fresh grant (the capacity
  read errors) releases the hold and forgets its prompt rows in **one
  transaction** (`release_and_forget_unstarted_shared`), so the host's retry is
  neither refused as a replay nor left bound to a released hold. Only an
  `active` hold is touched; a final hold keeps its rows. If that cleanup itself
  fails nothing changed: the hold stays active and bound to its prompt (a retry
  of that prompt reuses it; its capacity stays reserved and frees only when the
  hold expires, so no capacity is created and no spend dropped), and the daemon
  log records both errors (`GrantCleanupFailed`). The host sees only the generic
  internal-error response, so whether it saw the grant is unknown; a retry made
  after that stranded hold has expired is refused as `key_conflict` (rare,
  fail-closed, expiry is one hour). A retry of a
  live prompt that straddles a month rollover looks up the lane's hold in the
  new period's pool, finds none, and is refused; the old hold expires on its
  own (rare, fail-closed). A late `Stop` for an older hold that never settled
  is skipped without touching the newer hold; its spend is not lost, because the
  next hold's transcript window starts at the lane watermark, which that
  settlement never advanced, and so counts those records. The legacy task
  `Finalize` is unchanged.
  **Host assumption, unproven:** that the `prompt_id` on a `Stop` equals the
  one on that turn's `UserPromptSubmit` (ADR evidence lists both payloads as
  carrying `prompt_id`; their equality has not been observed on a real host).
  If they differ the guard never matches and behaviour is the pre-fix one.
  **Version skew:** a v24 `hook stop` that has a `prompt_id` sends it; a
  still-running v23 daemon rejects the unknown field (`deny_unknown_fields`), so
  that `Finalize` fails and the hold stays until its TTL. Restart the daemon
  after upgrading. A v24 daemon accepts the v23 hook's `Finalize` unchanged.
- *S10 prune re-charge (M3), fixed by HORO-2250.* Reproduced through the real
  request path: `shared-budget prune` deleted a closed pool and with it the
  idempotency memory of the charges booked there, so a subagent file charged in
  that month and swept again later (the `Stop` sweep re-checks every file once
  after a daemon restart, and a resumed session can be months old) was charged
  a second time, recreating the pruned pool. Two guards close it:
  1. *Event horizon.* `prune` records, in the same transaction as the delete,
     the newest pool it removed (`shared_retention_horizon`, migration 0023). A
     charge whose event time falls in that period or an earlier one is refused
     (`EventBeforeRetention`, protocol v24) and books nothing. This holds even
     when a prune removed every pool. A period after the horizon that simply
     never had a pool (April after a January prune; September before a first
     October pool) is real spend and books to its own period. A retry of a
     charge that already exists resolves to its own pool first. In the `Stop`
     sweep a refused file is not charged and is retried harmlessly at each later
     `Stop`.
  2. *Cumulative floor (migration 0023).* A subagent's "already charged" total
     is the sum of its charge rows. `prune` now folds each pruned pool's
     `subagent-transcript:` charges into `shared_charge_floor`, keyed by the
     family prefix, in the same transaction as the delete, and the total reads
     retained rows plus the floor (a retained row is never also in the floor).
     So a replay of a pruned charge books 0 and a file that grew after the prune
     books exactly the delta (a 300c subagent charged in a pruned month, grown to
     600c, books 300c). The table is additive and forward-only; an older ledger
     opens by applying the migration, and an older binary refuses the newer
     schema as it does for any migration. Month accounting is unchanged.
  Residuals: **a ledger pruned before migration 0023 existed has no floor and
  no horizon, so it gets neither protection** (v0.0.4 is unreleased, so no such
  ledger is expected). Main-lane watermarks are not kept by a prune; a lane
  whose marks were pruned falls back to its oldest remaining hold, and
  `SettleTail` refuses (`no_settled_lane`) a session with neither, so no main
  spend is re-booked. Only `subagent-transcript:` families are floored; a
  `ChargeAgent` with a caller-chosen key relies on the event horizon alone.
- Flush lag costs a flat hold when an entire Stop window is still unflushed
  after the `Stop` hook's bounded wait (it settles at the hold amount, an
  over-count that is not reconciled later). The wait makes this rare on the
  observed host; see *Flush lag at Stop* for the measurement and residuals.
- The post-`/compact` transcript is unverified.

  **Re-prompt capacity (HORO-2250):** a main-lane `Admit` with a new prompt id
  renews the lane's live hold only if a fresh reservation would be admitted
  with that hold released; otherwise it is denied `cap_exhausted`, writes
  nothing, does not extend the hold (it expires at its TTL) and records no
  prompt mapping. A retry of an accepted prompt, or an `Admit` without a
  prompt id (a retry cannot be told from a new turn), is still renewed
  unconditionally. Stop does not fire on interrupt, so without this check an
  interrupted turn could keep starting turns after other sessions overdrew
  the pool.

**Not done in S5** (S7 delivered the installer and `SessionEnd` wiring, S8
the `status` command and `doctor` checks): no gateway path (S11). `Finalize` itself returns
nothing about the settlement; it is observable through `Status`, the ledger
and the log.

### Flush lag at Stop (HORO-2246)

**Problem.** Claude Code writes its transcript asynchronously. A `Stop`
settlement that reads at once usually finds an absent file (first turn of a
session) or no new lines (resumed session) and books the flat turn hold
(`transcript_unreadable` / `no_usage_in_window`). A real-host run on 2.1.306
(HORO-2193 real-host evidence, F1) had 8 of 14 settlements fall back, each at
the default `turn_hold_usd_cents` of 1000 ($10) instead of about one cent.

**Measurement** (`TestOwnedLive`: real `claude` 2.1.306 with `-p` inside a
network-deny Seatbelt profile, isolated HOME and state dir, dummy key, fake
local upstream; 36 `Stop` and 4 `SubagentStop` events over 12 fresh sessions,
6 resumed pairs, 4 tool turns and 4 subagent spawns). Pre-fix run: a recorder
polling the transcript saw the turn's final assistant line **p50 66 ms, p90 71
ms, max 189 ms** after the daemon's read at `Stop` (36 samples; three of them
179-189 ms, eleven are upper bounds because the recorder started after the file
had landed, so the true lag is at most that). In these `-p` runs the whole
turn appeared at once; an interactive session that writes lines incrementally
may expose a partly flushed turn, and its flush timing is unmeasured. A subagent's final line appeared
about 82-88 ms after its `SubagentStop` dispatch. No event was still
unreadable after 8 s. Before the fix **36 of 36** `Stop` settlements fell back
to the flat hold (26 `transcript_unreadable`, 10 `no_usage_in_window`; $360.04
settled); after it **0 of 36** did ($0.44). Post-fix, the hook's own wait until
the daemon first reported the turn readable was min 8 ms, median 74 ms, p90 82
ms, max 99 ms over 34 waits (it includes up to 25 ms of poll granularity),
which is the added `Stop`-hook wall time; none gave up at the cap. A further
16 turns run with every core busy waited p50 74 ms, p90 84 ms, max 89 ms, all
measured. The fake upstream answers instantly and the runs are
`-p`, not an interactive TTY; neither is evidence for real latency or
interactive flush timing.

**Design: a bounded wait on the hook side, decided by the daemon.**
`hook stop` (Claude Code only, and only when `shared-budget.json` exists)
polls a new read-only request, `SharedBudgetRequest::TranscriptReady`
(protocol v23), before it sends the unchanged `Finalize`:

- *The daemon defines "ready".* It reuses the settlement's own lane lookup
  and windowed, session-bound reader, so the hook never opens a transcript
  (HORO-1725 stays true). `ready = false` only for `Unreadable` and
  `NoUsageInWindow`, the two outcomes a host that has not written yet
  produces. Everything else answers `ready = true` at once, genuine problems
  included (mismatch, unverified, unpriced, malformed, future-stamped, scan
  cap, no pending hold, invalid file): a later probe would not change what
  `Finalize` books, so waiting would only delay the hook.
- *Read-only.* The handler takes `&LedgerStore`, writes no row and moves no
  `Status` counter (a test compares the whole ledger and the counters around
  repeated probes).
- *The daemon is never occupied.* It is a single-threaded serial loop, so a
  wait inside it would stall every other session. Each probe is its own short
  connection, closed before the hook sleeps (5 ms doubling to 25 ms). A test
  shows another session's `Admit` answered while a `Stop` hook is still
  waiting.
- *Never denies.* Every way out ends in the unchanged `Finalize`: ready, the
  cap, a daemon that is down, too old (an unknown request is an error answer),
  slow (a probe has 400 ms) or refusing. Only prompt admission denies.
- *Bounded.* The cap is 1.5 s (about 20x the measurement) and the installed
  `Stop` entry carries no timeout. Worst-case hook latency is the cap plus
  one in-flight probe (400 ms), so about 1.9 s by default and about 5.4 s at
  the 5000 ms clamp, not 1.5 s. `LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS`
  overrides it: `0` disables the wait, larger values are clamped to 5000.
- *Unchanged elsewhere.* No budget file: no probe, no connect. Codex: not
  involved. `SubagentStop` is not waited on: its delta charge and the `Stop`
  sweep already absorb the lag.

**Why not reconcile the booked hold later (B).** The measurement shows a
tight lag, not a persistent gap, so a short wait removes the cause. A
reconciliation would have to edit a closed row (or add a compensating row)
keyed to the original settlement while keeping the S5 watermark, id-set and
future-stamp rules free of double counts, which is a large new surface for a
loss the wait already avoids. It stays unbuilt; revisit it only if an
interactive host shows lags beyond the cap.

**What `ready` means.** Some priced usage exists after the lane's watermark.
It does **not** mean the turn's final message is flushed. A host that writes
lines incrementally can answer ready on the first probe, and that `Stop`
settles measured-short; the remainder is recovered only by a later `Stop` or
`SettleTail` (lost if neither runs). Requiring a terminal `stop_reason` in the
window would narrow this but needs the reader to expose it; not built.

**Residuals: over-counts, plus a bounded timing under-count when a turn is
partly flushed (recovered at the next Stop/SettleTail, lost if neither runs).**

- A transcript still unflushed at the cap (a stalled host, a lag above 1.5 s),
  or a hook the host kills while it waits (then no `Finalize` is sent at all
  and the hold stays held until its TTL; a later `Stop` measures that turn's
  lines once), books the flat hold and is not reconciled. Each such event costs
  up to `turn_hold_usd_cents` minus the real spend.
- A partly flushed turn (for example only a tool call so far) answers
  `ready` and settles on what is there (the under-count above); the remaining
  lines are counted by the next `Stop`, not lost unless no later `Stop` or
  `SettleTail` runs. Not observed on 2.1.306 `-p`; interactive is unmeasured.
- A transcript path that never appears (`--no-session-persistence`, a mount
  the daemon cannot see, `/compact`-style turns without a file) costs the full
  wait on every `Stop` and then the flat hold. There is no per-session memory
  of a previous give-up (hooks are separate processes; that needs persisted
  state). Set `LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS=0` for such setups.
- If the host's own `Stop` hook timeout is shorter than the wait, the host
  kills the hook mid-wait: no `Finalize`, the hold stays held until its TTL.
- Interactive (TTY) flush timing, `/compact` summarizers and a daemon older
  than protocol v23 (probe fails, no wait) are unmeasured or unwaited.

**Operator implication: `turn_hold_usd_cents`.** The default of 1000 ($10) is
the price of every residual fallback above and, on a host that does not flush
in time, of every turn. With the wait, a normal turn books its measured cost;
the hold remains the upper bound that admission reserves, and the amount
booked when a turn cannot be measured. Set it near the real cost of one of
your own turns (and no lower than the dearest turn you expect, because an
unmeasured expensive turn is under-counted by the difference). Do not rely on
a large hold to hide flush lag; the wait is the fix. Lowering the hold is not
pure upside: admission reserves one hold per session, so a smaller hold lets
concurrent sessions overshoot the cap by more before the next admission is
refused.

### Installer and settlement hooks delivered in S7

**Implementation status (S7, HORO-2216).** Guarantee label unchanged:
**admission-bounded (cooperative)**, Libra-accounted USD, never provider
quota. Evidence class: `TestOwnedLive` (real hook processes against a real
daemon, synthetic transcripts); **no real Claude Code host has run the
installed hooks**, so nothing here is `ProductionObserved`.

- *Installer.* `install` / `install --hooks-only` add, idempotently, a
  `PreToolUse`, `SubagentStop` and `SessionEnd` entry next to
  `UserPromptSubmit`/`PostToolUse`/`Stop`. Ownership is the existing
  predicate (command contains `libra-governor` **and** ends with
  `hook pre-tool-use` / `hook subagent-stop` / `hook session-end`), so
  `uninstall` removes exactly those and foreign hooks, matcher groups and
  unknown settings are untouched. The two settlement entries carry
  `"timeout": 10`; the host's default `SessionEnd` window is short, and
  whether it honours the per-hook value is **not verified on a real host**.
  They are installed whether or not `shared-budget.json` exists.
- *No file, no change.* Both new handlers stat `shared-budget.json` first and
  return before parsing or connecting; `PreToolUse` already did.
- *`hook subagent-stop`.* Reads `session_id`, `agent_id`,
  `agent_transcript_path` and sends `ChargeAgentTranscript`. A blank or
  non-string id is absent and no id is invented; an `agent_id` outside
  `[A-Za-z0-9_-]` is dropped (the daemon's sweep uses the same charset). The
  path is relayed verbatim and the daemon alone validates and reads it.
  *One family per file.* The daemon books a charge under the agent id, or
  under a path hash when there is none, while the `Stop` sweep books
  `<session>/subagents/agent-<id>.jsonl` under the id in its file name. To
  make both converge, when the path's final component is exactly
  `agent-<[A-Za-z0-9_-]+>.jsonl` the hook sends that file-name id as the
  `agent_id` (a string check; nothing is opened, and the id is the
  host-provided file name, not invented). If the payload carried a different
  valid id the file-name id wins and the disagreement is logged, so a file is
  never sent under two families; the existing per-family ledger idempotency
  then dedupes the hook, the sweep, repeats, `..`, symlinked or aliased paths
  (`/var` vs `/private/var`) and files in other directories. Any other file
  name is charged under the payload id when valid, else `unattributed`. A
  payload without `agent_transcript_path` is not charged by the hook; the next
  `Stop` sweep (first 64 files) still finds it.
- *`hook session-end`.* Sends `SettleTail { session_id, transcript_path }`;
  a session never admitted or settled is refused by the daemon
  (`no_settled_lane`) and billed nothing.
- *Fail open.* Neither handler prints, blocks, spawns a daemon or exits
  nonzero; no daemon, a malformed payload, a reserved or blank session id, or
  no answer within 5 s is a log line. Only prompt admission denies.
- *Residuals.* S8 closed the `doctor` gap: it now reports all six hooks. A session whose
  `SessionEnd` never fires (a killed host) has its tail left unsettled; no
  other path settles it.

### Status, statusline and doctor delivered in S8

**Implementation status (S8, HORO-2217).** Guarantee label unchanged:
**admission-bounded (cooperative)**; amounts are Libra-accounted
list-price-equivalent USD at the pinned `PRICING_VERSION`, never a provider
bill or quota. Evidence class: `TestOwnedLive` (a real daemon and real hook
and CLI processes, synthetic transcripts); **nothing here is
`ProductionObserved`** and this is not a release GO.

- `libra-governor shared-budget status [--json]` is read-only. With no
  `shared-budget.json` it prints `not configured` and does not connect to,
  start or write anything. Otherwise it asks the running daemon for the
  existing `Status` request (it never starts one; with the daemon down it
  describes only the file and says the figures are unavailable). It shows the
  config state and reason, the zone-local period, cap, settled, held and
  remaining (negative remaining is shown as an overrun), a per-executor table
  (session, agent, kind `agent_absent`/`agent`/`unattributed`, settled, held)
  reconciled against the pool total with any discrepancy flagged, warn-only
  soft-limit warnings, flagged-settlement counts since the daemon started
  (`unmeasured_settlements`, `unmeasured_charges`, `future_timestamp_settlements`,
  binding mismatch, unverified, foreign lines, deferred sweep files), and
  `clock_anomaly`. `Status` gained one additive, defaulted field,
  `future_timestamp_settlements`; no protocol version bump (an older peer
  decodes it as zero).
- The legacy `statusline` line gains ` | budget $12.30/$500 (3%)` (settled
  plus held against the cap, percent rounded up for any spend) with a
  trailing ` WARN` for a soft-limit warning or when nothing remains (fully
  held or overrun), **only when a budget file exists**; without one the line is
  byte-for-byte unchanged, and any failure drops the segment. The shared-host
  `statusline provider` document is not changed.
- `doctor` reports the budget file state, whether the six hooks are wired in
  an all-tools group, daemon reachability, clock anomaly, flagged settlement
  counts, any per-executor reconciliation discrepancy, an exhausted or
  overrun pool (Warn: less than one turn hold remains, every new prompt is
  denied) and unknown capacity (Warn), and whether
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set (two different facts: a
  non-empty value in its own environment, and the key's presence in the
  `settings.json` env block; the value is never printed or interpreted, so a
  present key set to `0` or `false` does not mean traffic is disabled, and no
  other variable is read into the report). It also states what the budget
  does not cover. With a budget file present, a missing `UserPromptSubmit`
  (admission) or `Stop` (settlement) hook is an **Error** (doctor exits 1):
  nothing is admitted or charged. Other missing hooks are a Warn.
- **Exit codes.** `shared-budget status` is a viewer and exits 0 even for a
  reconciliation DISCREPANCY, `clock_anomaly` or an invalid config (the
  state is in its output and `--json`); `doctor` is the failing check.
- **Intentional behaviour change (L-A).** `inspect()` and so the legacy
  `hooks_wired` result now treat a hook in a matcher-scoped group (for
  example `PostToolUse` under `Bash`) as not wired, matching `install`.
  `doctor`'s legacy `claude_settings` finding therefore moves from Ok to
  Warn for an install that only has such a group, and the evidence report
  that consumes `hooks_wired` follows. This is deliberate, not a regression.
- The S7 review finding L-A is fixed: `inspect()` counted our command under a
  user's foreign matcher group (for example `Bash`) as wired; it now requires
  a no-matcher, `""` or `"*"` group, the same rule `install` applies.
- **Not built, and not claimed:** the S11 gateway hard cap (`governor_held`);
  a Codex budget gate (the Codex route neither admits nor charges); remote
  upload (S10 delivers a local export only); a `clock_anomaly` repair command;
  in-turn overshoot protection in hooks-only mode; real-host evidence.

### Egress inventory

Only `crates/gateway` (upstream allowlist, default `api.anthropic.com`) and
`crates/extension` (loopback literal only) may hold HTTP clients. Host
telemetry and WebFetch are Claude Code's own traffic: Libra neither reports
nor enforces them; slice S8 (HORO-2217) delivered `doctor` reporting of the
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` setting (reporting only; value never
printed or interpreted).

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
  pruned period also drops its idempotency-key memory. Since HORO-2250 the
  daemon refuses (`EventBeforeRetention`, books nothing) a charge whose event
  time falls in a period older than the oldest retained pool, so a replay of an
  old charge cannot recreate the pruned pool and book again.
- `export --period YYYY-MM` writes a local `0600` file (schema
  `libra.shared-budget-export.v1`, per-executor aggregates plus pool
  totals, SHA-256 over the canonical body, no content). Session and agent
  ids are **pseudonymised** by default (32-bit unsalted digest; guessable
  for low-entropy ids; linkable across exports); this is not anonymisation
  and a redacted export is not safe to publish. `--raw-ids` opts out.

Upload is **not built**: no transport, retry, resume or de-duplicated
delivery exists; that remains the separate, versioned workstream above.

### Founder version-scope decision (HORO-2261, 2026-10-27)

The founder approved this scope for **v0.0.4** (recorded in HORO-2261):

- v0.0.4 ships the existing Claude Code shared monthly pool as an explicitly
  labelled **Cooperative Preview**, subject to its remaining original
  applicable QA and release gates.
- S11 gateway `governor_held` enforcement and the Codex budget gate are
  **not required to be enabled** for v0.0.4. They stay off by default and
  must not be enabled in v0.0.4.
- This is a founder-approved scope adjustment. It is **not** a claim that the
  original hard-budget guarantee has been delivered.
- The accepted HORO-2250 (comment 29205, PR #227) and HORO-2258 QA evidence
  is preserved unchanged.
- No new OpenAI Responses gateway is authorised; it needs its own decision.
- No publication, production deployment, new egress route or paid model test
  is authorised by this decision. v0.0.4 is not released.

**Provenance of this decision.** Kept separate so no record is mistaken for
independent proof.

| Kind | Item |
|---|---|
| Explicit instruction | A human-origin message headed "FOUNDER DECISION - LIBRA HORO-2261" in the v0.0.4 release-owner Claude Code session, 2026-10-27 02:03:01Z (10:03 +08). It approves the scope above in its own words. The session transcript records it with origin `human`. |
| Jira relay | HORO-2261 comment 29248, written by the release-owner agent session. The Jira author field is the same Atlassian account as agent comments, so it cannot separate founder from agent. It is a relay, not proof. |
| Agent interpretation | The five disclosure wordings and the README text. The instruction lists the disclosure topics; the exact wording is the agent's. |
| Agent proposal | The earlier A/A recommendation and the S11/Codex designs (PR #228, #229). Proposals, never approvals. |
| Actual version acceptance | None. v0.0.4 is not accepted or released. The decision sets scope only. |

Limit: the instruction arrived on the session's interactive human channel,
the same channel as the earlier Option 2 directive (HORO-2186 comment 28778).
No out-of-band confirmation that the sender is the founder exists. If the
founder did not send it, restore S11 and the Codex gate to Proposed/Pending.

**Release disclosure (required wording content).** The v0.0.4 release notes
and in-product text must state that the pool is:

1. admission-bounded (cooperative), not a strict monetary hard cap;
2. subject to in-turn overshoot, because spend is visible only at Stop;
3. not applied to Codex, which remains ungated;
4. not a guarantee of matching the actual provider bill (amounts are
   list-price-equivalent estimates or flat holds, never billed USD);
5. limited to the supported executor and subagent accounting boundaries
   described in `docs/execution-owner-contract.md`; unsupported executors and
   subagent spend are not covered.

**Policy for future off-by-default work** (design:
`docs/design/s11-gateway-governor-held-hard-cap.md`,
`docs/design/codex-shared-budget-gate.md`; not shipped):

- S11 hard mode must not silently admit server-tool costs it cannot measure,
  and must fail closed on request shapes it has not verified. The existing
  cooperative mode keeps normal WebSearch behaviour.
- Interrupted or orphaned gateway requests keep a conservative bounded
  reservation until reliable reconciliation exists. Reserved or estimated
  amounts are never presented as provider billing.
- The input bound must be a verified conservative bound; bytes/3 is not
  treated as a mathematical guarantee.
- The Codex gate has no automatic activation, does not assume hooks are
  trusted, does not claim subagent cost is covered when it is not, and
  distinguishes list-price-equivalent spend from billed USD. Real-host Codex
  runs are authorised separately.

### Provider-namespaced lanes (HORO-2281, v0.0.5, dark)

The executor key in this ADR is `(host_id, provider, session_id, agent_id | null)`, but until v0.0.5 the budget keyed its holds on the raw `session_id` alone, so a request from another provider that reused a Claude session id could renew or settle a Claude hold. v0.0.5 closes that without changing any Claude row or byte:

- `Admit` and `Check` carry an optional `provider` (protocol v25). Absent or `"claude-code"` keeps the raw session id as the lane key. `"codex"` derives a reserved-prefix key `shared-budget-provider-v1:` plus the JSON array `["codex", session_id]`, which is injective. Any other value is refused as `UnsupportedProvider` and never falls back to the Claude lane; this applies when a budget file is present, and an absent budget file still answers `not_configured` before the provider is examined, and a Codex request under an invalid budget file answers `not_configured` rather than `config_invalid` because the Codex answer precedes the configuration check. A client session that starts with the reserved prefix is refused as `ReservedSessionScope` for every provider, a deliberate tightening: no Claude host mints such ids.
- The tool-event heartbeat is routed by the request's provider, and `Finalize` runs the Claude turn settlement and subagent sweep for the Claude lane only. `ChargeAgent`, `SettleTail` and `TranscriptReady` also carry the optional `provider` (same omitted-when-absent rule, still protocol v25; Claude bytes are unchanged) and resolve their lane through the same path as `Admit`. The lane-taking functions (`heartbeat`, `settle_turn`, and the `*_on_lane` variants of `settle_tail`, `transcript_ready` and `charge`) take an already-derived lane key and only require it to be non-empty; client-supplied ids are validated, and the reserved prefix refused, once at the daemon boundary by `client_lane`/`admission_lane`. `ChargeAgentTranscript` and the subagent sweep still key on the raw Claude session id. No Codex hold exists to renew or charge until the Codex settlement (C2) lands.
- A Codex-provider `Admit` or `Check` is answered `not_configured` and writes nothing. No Codex hold is created, charged or enforced; enabling a Codex budget gate remains a founder decision (see "Not decided here").
- The guarantee label is unchanged: admission-bounded (cooperative).

### Codex turn settlement (HORO-2282, C2, v0.0.5, dark)

- `settle_codex_turn` is a daemon function only. No `SettleTurn` wire request and no hook exist yet (C3), and Codex `Admit` and `Check` still answer `not_configured`, so no Codex hold is created by the product. The function does nothing, reads nothing and writes nothing unless a valid-or-invalid budget file exists **and** the marker file `<state_dir>/shared-budget.codex` is present (stat on every call; contents never read; a symlink does not count). Activating Codex participation remains a founder decision.
- The hold settled is the one the turn id was admitted into (`shared_hold_prompts`). There is no first-prompt-key fallback and no newest-pending-hold fallback, unlike the Claude `settle_turn`. A mapped hold that is already final, or whose row was pruned, is `AlreadySettled` with no write. A turn with no mapping is charged after the fact as `codex-turn:<lane>|<turn_id>` (executor `agent_absent`, source `turn_hold`) at its measured cents, else the hold amount, `usage_known=false`; a pending hold of another turn is never touched. The charge is booked to the period of the settle time because the reader returns no record timestamps. A measured turn of zero tokens books nothing.
- A re-prompt renews the live hold and maps its turn id to the same hold, so a hold settles over **every** turn id mapped to it (new ledger lookup `prompts_for_hold`), each read by its own `turn_id` with `observe_codex_turn` and priced on its own (pico-dollars, rounded up once per turn). Codex `output_tokens` already includes reasoning, which is never priced again. If any turn is unmeasurable the hold settles at `max(priced_sum, hold)`: measured only when `priced_sum > hold`, otherwise the hold amount with `usage_known=false`. A model that is absent or not in the pinned table plus the file's `pricing_overrides` settles the hold amount, flagged `unpriced_model`; no OpenAI price is pinned, so by default every Codex turn settles at the flat hold unmeasured. An invalid budget file settles the hold amount and ignores its overrides. A hold mapped to more than 16 turns measures the first 16 and settles at `max(priced_sum, hold)` labelled `scan_cap_reached` (measured only when `priced_sum > hold`), which bounds one settlement to 16 backward rollout scans; C3 must not hold the ledger/server lock across the call longer than the request needs.
- The model is the host's claim in the `Stop` payload (the rollout has none), and the rollout is a file any same-user process can write; the amount a measured turn books is therefore bounded by what the host reports, exactly as for Claude.
- Documented residuals: (a) a `Stop` for T1 arriving while T2 shares its live hold and has not reached the rollout settles the hold on what is there, so T2's own `Stop` is a replay and its spend is not booked until the C4 true-up. (b) A post-hoc charge is booked with `event_time` = settle time, so a replay of the same turn after a retention prune of that month's pool can book it a second time; C4 must account for it. (c) An unadmitted turn under an invalid budget file books nothing (the outcome is a label only, `Unbooked(config_invalid)`); C3 must surface it as a counter. (d) A measured turn of zero tokens books no post-hoc row.
- Telemetry: `unadmitted_turns` counts post-hoc Codex charges in daemon memory. It is not yet a field of `SharedBudgetStatus`; exposing it needs an additive protocol field.

### Codex hook wiring (HORO-2283, C3, v0.0.5, dark)

- The Codex hooks gate only when `shared-budget.json` **and** the marker `<state_dir>/shared-budget.codex` exist (hook and daemon each stat the marker; a symlink does not count). Without the marker the hooks send no `Admit` and no `SettleTurn` and the daemon answers every Codex-provider request `not_configured` with no write, so Codex behaviour is unchanged. Claude Code hooks and wire bytes are untouched.
- Daemon: a Codex `Admit` / `Check` is honoured only while the marker exists (`admission_lane` takes the runtime's answer; `ChargeAgent`, `SettleTail` and `TranscriptReady` stay inert for Codex). New additive request `SettleTurn { session_id, turn_id, model?, transcript_path? }` dispatches to `settle_codex_turn`; its answer is `not_configured` (no write) or `codex_turn_settled { disposition }`, a log label. `PROTOCOL_VERSION` stays 25: a v25 daemon built before this change rejects `SettleTurn` as malformed (the hook logs and moves on) and answers a Codex `Admit` `not_configured`, so after upgrading an enrolled install must restart a long-lived daemon to be gated.
- Hook: `prompt_id := turn_id`; a block uses the Claude block JSON. Fail-closed rows are the Claude rows (daemon unreachable, unusable answer). A blank or reserved session is skipped; a subagent prompt (`agent_id`) is not admitted and is logged; marker + `LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER=1` blocks with an explicit reason. `Stop` sends `SettleTurn` before Finalize / owner Stop, within 5 s, never printing and never blocking, and skips a subagent, a payload with no `turn_id` and a bad session.
- Codex hooks run only after the operator trusts them in Codex's `/hooks`; untrusted hooks are skipped silently and leave an enrolled install ungated.
- Known limits (orchestrator rulings, v0.0.5):
  - **Stale daemon.** `PROTOCOL_VERSION` stays 25 (the v24 to v25 bump belongs to C1 alone). A long-lived v25 daemon built before C3 answers a Codex `Admit` `not_configured` and rejects `SettleTurn`, so an enrolled install is silently ungated until the daemon restarts. C5's `doctor` will probe this (a Codex `Check` under the marker is `not_configured` on a stale daemon, a real answer on a current one).
  - **No turn id.** A Codex prompt without `turn_id` is admitted without a `prompt_id`; the hold is never mapped to a turn, so neither `SettleTurn` nor C4 orphan recovery can settle it by turn id, and it expires after the hold TTL.
  - **Subagents.** A subagent prompt or Stop (`agent_id` present) is neither admitted nor settled in v0.0.5 (known gap, C6).
  - **No heartbeat.** Codex has no PostToolUse hold renewal here; a turn longer than the 1 h hold expires and C4 orphan recovery settles it.
  - **Serial daemon.** `SettleTurn` runs `settle_codex_turn` inside the daemon's serial request loop with the ledger borrowed, like the Claude `Stop` settlement; the rollout read is bounded by the reader's caps (C2). It is not measured outside the ledger borrow in this slice.

### Codex orphan recovery and true-up (HORO-2284, C4, v0.0.5, dark)

Both run inside `settle_codex_turn`, after the `Stop`'s own outcome and behind the same budget-file and marker gate, so with the marker absent neither reads nor writes anything. Neither changes the result of the `Stop` that triggers it; a recovery step that cannot complete is retried idempotently at the lane's next settle.

- **Orphan recovery.** A hold whose `Stop` never arrived (Codex killed) expires after an hour and keeps its prompt mapping. At the lane's next settle every `expired`, unsettled hold with a mapping is settled late, in its own row and period, by its own turn ids: the measured cents, else the hold amount with `usage_known=false`. Never zero. At most 16 per settle; the rest wait for the next one.
- **True-up.** The final `token_usage_record` can still sit in Codex's writer queue when `Stop` settles, so a measured hold can settle short (undetectable by the reader). At the next settle the lane's newest settled hold with `usage_known=true` is measured again, provided every turn mapped to it now has a `task_complete` line. The delta `measured - (settled_amount + prior true-ups)` is booked as a charge keyed `codex-trueup:<lane>|<hold id>|c<cumulative cents>`. The baseline sums every true-up already booked for the hold by key prefix across periods, plus what a prune folded into `shared_charge_floor`. A smaller measurement books nothing; nothing is ever refunded; a replay books 0. The Codex reader is keyed by `turn_id` and has no timestamp watermark, so the objection that rejected reconciliation for Claude does not apply.
- **Key identity.** The key names the hold's row id, not a turn id: a turn that joined the hold later (a smaller id sorting first) would otherwise change the key and let a replay book twice.
- **Prune.** The `codex-trueup:` family (prefix up to the last `|`) is folded into `shared_charge_floor` in the prune transaction, exactly as `subagent-transcript:` is. The `subagent-transcript:` filter and every other row are unchanged.

Residuals, disclosed:

- **Period.** A true-up books to the pool the hold was admitted into, even when that month has since closed, because that is the period the spend ran in; it can therefore add spend to an already-closed month's totals. It is skipped, writing nothing, when the hold row or its pool has been pruned, or when the configured zone has changed so that the hold's creation time no longer maps to its pool. C2's post-hoc charge for an unadmitted turn still books at settle time (the deviation recorded above, unchanged).
- A hold that settled at the flat hold amount (`usage_known=false`, including every Codex turn while no `gpt-*` price is configured) is never trued up. It stays at the flat hold; a true-up never refunds a flat hold that was larger than the real spend.
- A record flushed after the last settle of a lane, or after the hold's row was pruned, is never booked.
- The last turn of a killed session that is never resumed stays unsettled: the under-count is bounded by its real spend (the same class as Claude's killed-host residual).
- Only the lane's newest measured settled hold is re-measured per settle; an older hold with a late record that was not the newest at the time of a settle is not trued up.
- The model and rollout path used are the `Stop` payload's, host-claimed as for the settlement itself.
- **Serialized settles.** The true-up reads its baseline (settled amount plus prior true-ups) and then books the charge in separate ledger calls, which is safe only if settles of one lane do not interleave. In the daemon this holds: the accept loop is single-threaded and serial (`crates/daemon/src/server.rs`, module header), so one `SettleTurn` finishes before the next starts. Another process writing the same ledger could interleave; the cumulative-keyed idempotency key then protects a repeat of the same measurement but not two different ones, so the baseline read and the charge must move into one transaction before the daemon is ever made concurrent.
- **A still-running turn can be settled early.** Orphan recovery settles a hold that expired after one hour with no `Stop`, which includes a turn that is still running (a tool-free turn longer than an hour). It then settles on the partial measurement available. The true-up can correct that only while the hold is the lane's newest measured settled hold; once a later hold settles, the earlier one is not re-measured and any further spend on that turn is not booked.
- **Failures are isolated.** A hold whose late settle fails is counted (`SharedBudgetRuntime::codex_recovery_errors`, volatile) and skipped; the remaining holds and the true-up still run and the failed hold is retried at the next settle. The 16-hold cap counts only holds with a turn mapping.
- The transcript path is host-claimed: the `task_complete` check refuses a non-regular file and opens without blocking, as the reader does.

## Consequences

- Once slices S4-S8 land (design intent), refusal before spend at prompt
  admission becomes enforceable locally, with an honest "cooperative" label
  instead of a hard-cap claim.
- Overshoot beyond the hold is possible in hooks-only mode and is accepted
  until S11.
- HORO-2197 attributes the *legacy* per-session counters (tool calls, streak,
  task binding) to the `(session, agent)` executor, so a subagent no longer
  inflates its parent's. It deliberately leaves this budget's holds,
  heartbeat and settlement keyed by the host's real `session_id`: that route
  already carries `(session, agent | none)` with `agent_absent | agent |
  unattributed` executor kinds. See `docs/execution-owner-contract.md`, which also records the mixed-version
  behaviour (v21 CLI -> v22 daemon fails open; v22 CLI -> v21 daemon loses the
  first `ToolInvoked`), the estimator-history effect (pre-change receipts
  counted subagent calls into the parent; later ones do not) and the
  unreclaimed derived-lane counter rows.
- Tests of the month window pin tzdb behaviour (Asuncion 2023-10 gap, Havana
  2026-11 fold); a jiff bump that changes those rules must update them
  deliberately.

## Not decided here

Release GO/NO-GO, remote upload, enabling gateway `governor_held` (S11, designed, not built; not enabled in v0.0.4 per the founder scope decision above), enabling a Codex budget gate (designed, not built; same), any provider-bill
reconciliation, and any change to the ADR-0023 production-feed ban.
