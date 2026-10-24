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
in S5*). `PROTOCOL_VERSION` is 21. The S6 Claude Code hook gate and the S10
retention/export commands landed on `main` separately and are recorded in
their own commits and in *Retention and export* below.
**S7 (HORO-2216) delivers the installer and the settlement hooks**:
`libra-governor install` writes `PreToolUse`, `SubagentStop` and `SessionEnd`
entries beside the original three, and `hook subagent-stop` /
`hook session-end` send `ChargeAgentTranscript` / `SettleTail` (see
*Installer and settlement hooks delivered in S7*).
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
| Edited, valid | New cap applies to the next decision. A lower cap never revokes existing holds; it denies new ones while `remaining < hold` |
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
   the file, and at `SubagentStop` a subagent's file may hold no assistant
   line yet. Multi-second inversions are covered by a ten-minute slack; larger
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
newer records. The last message of a session is lost unless something runs
after the final Stop: see `SettleTail`.

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
`Stop` counted (the final message of a session) as a settled `agent_absent`
row (`charge_source = turn_hold`, `usage_known = true`) with the same
binding, de-duplication, future-bound and watermark; idempotent; nothing is
charged when nothing is measurable (a tail has no hold to fall back to; the
response flags `unmeasured`/`binding_mismatch`). It is refused with
`no_settled_lane` unless the lane already has a settled hold or a watermark
(a session never admitted or settled under this budget must not be billed its
whole history); its lower bound is the same as a `Stop`'s: the lane's
watermark, else the latest settle time (never the earliest hold, since
earlier unmeasured settlements were already booked at the hold amount). The charge row and its watermark are written in **one ledger
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

- *Duplicate Finalize after the next Admit (M1).* `Finalize` carries no
  idempotency key. Scenario: Stop #1 settles hold H1; the user prompts and
  Admit creates H2; a duplicate of Stop #1 arrives and settles H2 early from
  an empty window (the hold amount), booking a turn that has not happened.
  Proposed mitigation: key the settlement by hold id / persist the
  transcript high-water mark on the request so a replayed Stop that carries
  no new records settles nothing.
- *S10 prune re-charge (M3).* `shared-budget prune` deletes whole closed
  pools, including the rows the watermark and the subagent already-charged
  sum are read from. Scenario: a subagent file charged in a pruned month is
  swept again in the current month and its full cumulative is charged again
  (over-charge); a lane whose marks were pruned falls back to its oldest
  remaining hold. Proposed mitigation: an `event_time` horizon clamp so a
  charge older than the retained pools is dropped or booked to a fixed
  archive row, and keeping watermark rows out of the prune.
- Flush lag still costs a flat hold when an entire Stop window is not yet
  flushed (it settles at the hold amount); the next Stop recovers the records
  but the hold was already booked (an over-count).
- The post-`/compact` transcript is unverified.

**Not done in S5** (S7 delivered the installer and `SessionEnd` wiring, S8
the `status` command and `doctor` checks): no gateway path (S11). `Finalize` itself returns
nothing about the settlement; it is observable through `Status`, the ledger
and the log.

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
- The legacy `statusline` line gains ` | budget $12.30/$500 (2%)` (settled
  plus held against the cap) with a trailing ` WARN` for a soft-limit warning
  or an overrun, **only when a budget file exists**; without one the line is
  byte-for-byte unchanged, and any failure drops the segment. The shared-host
  `statusline provider` document is not changed.
- `doctor` reports the budget file state, whether the six hooks are wired in
  an all-tools group, daemon reachability, clock anomaly, flagged settlement
  counts, any per-executor reconciliation discrepancy, and, by presence only,
  whether `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set (in its own
  environment and in the `settings.json` env block; the value and every other
  variable are never read into the report). It also states what the budget
  does not cover.
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
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` setting (presence only, reporting
only).

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

Release GO/NO-GO, remote upload, gateway `governor_held` (S11, not built), a Codex budget gate, any provider-bill
reconciliation, and any change to the ADR-0023 production-feed ban.
