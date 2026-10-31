> **Status: design proposal, not shipped.** HORO-2261 (Codex shared-budget gate). Line numbers refer to main aea0894. Nothing here is approved for enablement: switching on any enforcement mode, its guarantee label and every item listed as Level 2 need an explicit founder decision. READY_FOR_RELEASE stays false.

# Codex shared-budget admission, accounting and enforcement: design

Repo: `libra-governor` at `origin/main` aea0894 (worktree `libra-governor-v0.0.4-HORO-2250-fix-stale_admit_replay`).
Read-only design. Line numbers refer to aea0894. Status: design intent only; nothing here is shipped.

## 0. Decision (lead)

1. **Build a dark, hooks-only Codex gate on the existing ADR-0026 pool.** Codex prompts take the same
   `turn_hold_usd_cents` hold in the same `metered:local-budget/monthly-YYYY-MM` pool as Claude Code (D3: one
   global hard budget). The Codex `Stop` settles that hold turn-scoped from the rollout, using the reader that
   already exists (`observe_codex_turn`). The guarantee label stays **admission-bounded (cooperative)**, with
   the extra Codex caveats listed in section 3.
2. **Namespace Codex lanes by provider, in the daemon.** Claude keys stay byte-identical. Codex lanes get a
   reserved-prefix derived session key, following the `legacy_executor.rs` precedent.
3. **Enrolment is a marker file (`<state_dir>/shared-budget.codex`), absent by default.** Both the hook (by
   stat only) and the daemon read it. It is not a key in `shared-budget.json`, because that file is
   `deny_unknown_fields` (`crates/daemon/src/shared_budget_file.rs:51-59`): a new key would make an older
   daemon report `config_invalid` and start denying **Claude** prompts.
4. **No OpenAI gateway now.** It is a Level-2 founder decision (new egress, new credential path, new trust
   boundary), and the Claude S11 `governor_held` it would mirror does not exist yet.
5. **Codex subagents are explicitly NOT COVERED** in the first slices. They are named in `NOT_COVERED`,
   `status` and `doctor`, never silently dropped.
6. **Turning it on is a founder decision (Level 2).** Precedent: `crates/cli/src/agent/run.rs:33-40`
   ("activation of unverified economic effects" was excluded from the HORO-1714 authorization) and ADR-0026:867
   ("Not decided here: ... a Codex budget gate").

## 1. What Codex exposes: verified / inferred / unknown

Evidence classes. **V-src**: verified against a schema, docs or source. **V-run**: verified by running a real
binary (sandboxed, no auth). **V-fix**: a real serializer output fixture. **I**: inferred. **U**: unknown.

| Fact | Class | Evidence |
|---|---|---|
| Hook events exist: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PermissionRequest, PostToolUse, Pre/PostCompact, SubagentStart/Stop, Stop, Interrupt | V-src | `docs/adr/0004-agent-adapter-contract.md:27-33`; official hooks doc (learn.chatgpt.com/docs/hooks) |
| UserPromptSubmit payload: `session_id, cwd, prompt, model, turn_id, permission_mode, hook_event_name, transcript_path` (+ optional `agent_id/agent_type`) | V-src | ADR-0004:35-37; `integrations/codex/README.md:52-54` |
| Stop payload: `session_id, cwd, model, turn_id, stop_hook_active, last_assistant_message, permission_mode, transcript_path` | V-src | ADR-0004:41-42; README:58-59 |
| PostToolUse: `... tool_use_id, model, turn_id, transcript_path` | V-src | ADR-0004:38-40 |
| **No `prompt_id`** on Codex payloads; `turn_id` is the per-turn host id | V-src (by absence) | ADR-0004:35-42; `crates/cli/src/agent/normalize.rs:275-293` (Codex test: `prompt_id: None`) |
| UserPromptSubmit can block with `{"decision":"block","reason":..}` or exit 2 | V-src (docs) | hooks doc; same JSON as `render_prompt_block` (`crates/cli/src/agent/render.rs:94-100`) |
| A blocked prompt never reaches the model / is kept out of history | I (from an upstream commit description, #14626) | not exercised on a real host here |
| PreToolUse deny: `hookSpecificOutput.permissionDecision:"deny"` | V-src (docs) | hooks doc; same shape as `render_pre_tool_deny` (render.rs:104-116) |
| SessionEnd/Interrupt timeout 1 s default, max 3 s | V-src | ADR-0004:98-103; README:68-70 |
| SubagentStart/Stop carry `turn_id, agent_id, agent_type`; SubagentStop carries `agent_transcript_path`; subagent hooks use the **parent** `session_id` | V-src (docs) | hooks doc; `docs/native-agent-lifecycle-characterization.md:11-13,29` |
| Hooks only run after `/hooks` trust (content hash); untrusted hooks are skipped silently | V-src + V-run | README:132-146; ADR-0004:140-152 |
| `hooks.json` top level is `{description?, hooks}`; a wrong shape disables every hook (fail-open) | V-run (Codex 0.154.0) | ADR-0004:118-138 |
| A real `UserPromptSubmit` reached `codex-hook` and the daemon bound a task to the real Codex session id | V-run (0.154.0, no auth, every model call 401) | ADR-0004:207-214 |
| Rollout line `token_usage_record.payload{thread_id, turn_id, session_id, root_turn_id, response_id, usage{input,cached_input,cache_write_input,output,reasoning_output,total}, turn_token_usage, thread_token_usage}` | V-fix (codex-cli 0.160.0 serializer output) | `crates/daemon/tests/fixtures/codex-rollout-token-usage.jsonl:2,6`; `crates/daemon/src/usage/codex_turn.rs:160-175` |
| `event_msg/task_started` marks the turn start; `task_complete` is written **after** the Stop hook returns | V-src (openai/codex `run_turn` / `on_task_finished`) | codex_turn.rs:22-62 |
| The final `token_usage_record` may still sit in the rollout writer's mpsc channel at Stop (undetectable under-count) | V-src | codex_turn.rs:63-118; `docs/execution-owner-contract.md:133-144` |
| `output_tokens` includes `reasoning_output_tokens`; `cached_input <= input` | V-fix + reader invariant | codex_turn.rs:225-258 |
| Rollout usage records carry **no `model`** | V-fix (no model field in fixture) | fixture lines 2, 6 |
| Pinned pricing table has **no OpenAI models** | V (code) | `crates/gateway/src/pricing.rs:41,97-255` |
| Hook `turn_id` (UserPromptSubmit and Stop) == rollout `token_usage_record.turn_id` | **U** (I: same id space) | fixtures use synthetic `turn-1` (`crates/cli/tests/fixtures/agent/codex/*.json`) |
| UserPromptSubmit `turn_id` == Stop `turn_id` for the same turn | **U** (I) | docs call both "active Codex turn id" |
| Hook `session_id` == rollout `session_id`/`thread_id` | **U** (I; the fixture has session_id == thread_id) | fixture line 2 |
| Hook `transcript_path` is the rollout file the reader expects | **U** (I) | `fixtures/agent/codex/stop.json` **omits `transcript_path`** although its `_source` lists it |
| Where subagent usage lands (parent rollout with its own `turn_id` and `root_turn_id`=parent, a separate agent transcript, or both) | **U** | codex_turn.rs:973-998 is synthetic; the docs' `agent_transcript_path` suggests a separate file. **Double-count risk** |
| UserPromptSubmit fires for subagent turns | U | docs are silent |
| `session_id` stable across resume/compact/fork | U | ADR-0004:220-222; lifecycle doc :26-31 |
| Statusline surface | V-src absent | README:212 |
| Custom model provider is `wire_api="responses"` only; the Libra gateway speaks only `/v1/messages` | V-src | ADR-0004:169-174; README:211 |
| Auth mode (API key vs ChatGPT login) of a given user | U per user | ChatGPT-login traffic does not go to `api.openai.com` with an API key, so a gateway cannot cover it without custody of the OAuth credential (I) |

> C0 update (2026-10-31): the rows above for hook `turn_id` equality, UserPromptSubmit and Stop `turn_id`, `session_id` equality, `transcript_path` and the subagent usage location are now source-derived (V-src, not run on a real host) in `docs/design/codex-c0-source-verification.md`, which also lists which of its claims were re-read by a second reader and which are single-read. `task_complete` ordering is not covered by C0, and the final-record flush ordering stays the pessimistic reading above until a runtime probe exists.

Repo behaviour today ("Codex ungated"):
- Prompt gate is Claude-only (`crates/cli/src/agent/run.rs:263`).
- PreToolUse returns early for Codex (run.rs:436-439).
- Settlement hooks are Claude-only (`crates/cli/src/agent/settlement.rs:242-243,286`).
- The flush wait is Claude-only (`crates/cli/src/agent/flush_wait.rs:140-141`).
- Capability row: `crates/domain/src/agent.rs:263-274`, pinned at :494-508.
- `NOT_COVERED` text: `crates/cli/src/shared_budget_view.rs:48`.
- Tests: `crates/cli/tests/shared_budget_hooks.rs:1041-1055` and `crates/cli/tests/shared_budget_golden_journey.rs:1080`.
- **Latent hazard.** A Codex `Stop` sends legacy `Finalize{provider:"codex", transcript_path}`. With a budget
  file present the daemon runs the **Claude** `settle_turn` and the subagent sweep on the Codex session and
  rollout (`crates/daemon/src/server.rs:2330-2366`). This is a no-op today only because Codex never holds.
  If Codex admitted naively, every Codex turn would hit the Claude reader (`sessionId` field), settle at the
  flat hold and be flagged. Section 4 removes this path for Codex.

## 2. Identity and session-ownership contract (provider namespacing)

**Problem.** Shared-budget rows key on the raw `session_id` under one principal. The places that do this:
- `hold_key`, `crates/daemon/src/shared_budget.rs:333-336`
- `shared_lane_holds` / `hold_for_prompt` in `settle_turn`, shared_budget.rs:1290-1318
- `heartbeat`, shared_budget.rs:2291-2323
- the `shared_hold_prompts` PK `(principal, session, prompt)`, migration 0024

`Admit`/`Check` carry no provider (`crates/protocol/src/shared_budget.rs:35-48`), although ADR-0026:155
specifies the executor key as `(host_id, provider, session_id, agent_id|null)`. A Codex session id that equals
a Claude one, or a mis-routed Stop, would settle the other provider's hold with the wrong reader.

**Contract (daemon-owned, mirroring `crates/daemon/src/legacy_executor.rs:1-50`):**

| Wire (`provider`, `session_id`) | Budget lane session key |
|---|---|
| `None` or `"claude-code"` | the raw `session_id`, byte for byte. No migration; existing rows and Claude bytes are unchanged |
| `"codex"` | `shared-budget-provider-v1:` + JSON `["codex", session_id]` (injective) |
| any other value | `Refused(UnsupportedProvider)` (new refusal variant). Never fall back to Claude |

- The prefix `shared-budget-provider-v1:` joins `execution-owner-v1:` and `legacy-executor-v1:` as a reserved
  prefix. `validate_identity` (shared_budget.rs:403-420) refuses it as a client-supplied session
  (`ReservedSessionScope`), so no native session can collide with a derived key.
- One function, `budget_lane(session_id, provider) -> Result<String, Refusal>`, is applied at **every** entry
  point. All of them must change in the same slice:
  - `Admit`, `Check`, the new `SettleTurn`
  - the tool-event `heartbeat`: `handle_tool_invoked_for_lane` passes the raw `session_id` today
    (server.rs:1888). It must pass the derived key when `ToolInvoked.provider == "codex"`. Otherwise Codex
    holds are never renewed and expire mid-turn after `HOLD_TTL_SECS = 3600` (shared_budget.rs:78), which
    frees capacity without a charge and breaks D3.6.
  - the per-executor rows in `Status`/`status`/`export`. Export already pseudonymises the session id, and the
    prefix exposes the provider, which is intended.
- `agent_id`. A Codex `UserPromptSubmit` with `agent_id` present is a subagent turn. It is **not admitted** in
  C1-C4: the hook proceeds, logs it and bumps a volatile `Status` counter `codex_subagent_turns_unadmitted`
  (section 7: not covered). Main-lane Codex turns are `agent_absent`. A missing agent never means root;
  ADR-0026:153-164 is unchanged.
- **Turn identity.** Codex `turn_id` fills the HORO-2250 `prompt_id` role on Admit: one host id names one
  turn. That brings in, unchanged:
  - the occurrence contract (ADR-0026:482-491)
  - `shared_hold_prompts`
  - the `key_conflict` replay refusal
  - `release_and_forget_unstarted_shared`

  The CLI maps `prompt_id := turn_id` **only for Codex**. Claude keeps its own `prompt_id`.
- Host id. The shared budget is per host and per OS user (`SHARED_BUDGET_SCOPE`,
  `crates/protocol/src/shared_budget.rs:23`), so `host_id` is implicit. No change.

## 3. Admission options

| Option | What is enforceable | What is only estimated | Honest guarantee label | Verdict |
|---|---|---|---|---|
| **A. Pre-turn hook gate** (UserPromptSubmit Admit, plus Stop settle) | Refuse a new Codex turn when `remaining < turn_hold` (or the config is invalid, or a clock anomaly). Cooperative: same-user processes can edit the file or untrust the hooks | USD from rollout tokens x list price (or operator overrides); in-turn spend is visible only at Stop; an overshoot beyond the hold is possible | `admission-bounded (cooperative)`. Codex caveats: **hooks run only once trusted via `/hooks`, and an untrusted or moved binary is ungated silently**; `doctor` cannot confirm trust without `doctor --verify-trust`; block semantics are documented but not host-verified | **Recommended (C1-C4), dark** |
| **B. Gateway/proxy** (`model_providers.<id>.base_url` -> Libra, `wire_api="responses"`) | Per-request refuse-before-forward. Overshoot bounded by one request's output cap. Usage from `response.completed.usage` per `response_id` | List-price USD; real bill unknown (tiers, discounts) | `per-request hard cap (governor_held)`, **only for API-key users who route their provider through Libra**. Bypassable by editing Codex config (same user). ChatGPT-login users are not covered | **Level-2, not sliced.** Needs a new Responses wire implementation, a new egress destination (`api.openai.com`; static guard `crates/cli/tests/egress_static_guard.rs`), API-key custody in the gateway, and a gateway-down fail-open/closed policy. Do it after Claude's S11 so `governor_held` semantics are shared |
| **C. Post-hoc reconciliation only** (charge at Stop, never deny) | Nothing pre-spend. Spend reduces the remaining capacity Claude admission sees | Same as A | `metered (post-hoc), not admission-bounded` | Rejected as the end state (does not meet D3's single hard budget for Codex). **Useful as a fallback mode inside A**: a Codex turn with no hold (admit skipped, or a hold not found) is still charged post-hoc, so spend is never silently dropped |

Fail-open/closed at admission (Codex, enrolled). Mirror ADR-0026:173:
- daemon unreachable -> **block** the prompt;
- tool time -> allow (no PreToolUse in C1-C4).

This works only because enrolment is a **hook-visible marker file**: the hook knows Codex is enrolled without
asking the daemon. The same stat-only discipline is already used for `shared-budget.json`
(`crates/cli/src/agent/budget_gate.rs:83`). When the marker is absent, Codex behaviour is byte-identical to
today. When the marker is present but `shared-budget.json` is absent, the daemon answers `not_configured`
and nothing changes.

## 4. Reservation, settlement, idempotency, recovery, double-count safety

**Reservation.** `Admit{session_id, agent_id:None, prompt_id:Some(turn_id), provider:Some("codex")}`
reserves `turn_hold_usd_cents` in the current month's pool through the existing `admit` (shared_budget.rs:610-772).
This path brings, unchanged:
- one hold per lane, with renewal on re-prompt;
- the `key_conflict` refusal for a replayed turn id;
- `CapExhausted` when `remaining < hold`.

**Settlement: a new `SharedBudgetRequest::SettleTurn` (protocol v25).** Fields:
`{session_id, provider, turn_id, model: Option, transcript_path: Option}`. The Codex `stop` hook sends it
**before** its route-specific Finalize or owner Stop, so settlement does not depend on which route is active
(it also works with `LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER=1`, where legacy Finalize never runs; run.rs:797).
The daemon's legacy `Finalize` **skips** `settle_turn` and the subagent sweep when `provider == "codex"`
(guard at server.rs:2330), so the Claude reader never sees a rollout. Claude's `Finalize` path is untouched.

Daemon `settle_codex_turn` (it deliberately differs from Claude's `settle_turn` in two ways):
1. **Mapped hold only.** Resolve the lane. `hold_for_prompt(lane, turn_id)` (the ledger call used at
   shared_budget.rs:657 and :1309) names the hold this turn joined.
   - Mapped hold is already final -> `AlreadySettled`, no write.
   - **No mapping -> never touch the pending hold.** On a Codex lane every hold postdates migration 0024, so
     no mapping means this turn was never admitted (for example a subagent turn firing Stop with the
     parent's `session_id` and no `agent_id`, which the docs do not rule out). It takes the post-hoc
     `codex-turn:` charge path below.
   - There is **no first-prompt-key fallback and no "newest pending hold" fallback** for Codex. Claude's
     `settle_turn` does use them (shared_budget.rs:1309-1318, `pending_hold` at :1219). For Codex they would
     let a foreign Stop settle the parent's hold and make the parent's real Stop a replay.
2. **Every turn of the hold.** `admit` renews a live hold and records the new prompt against it
   (`renew_shared_for_prompt`, shared_budget.rs:675). So an interrupted T1 (no Stop) followed by T2 leaves
   **both** turn ids mapped to hold H. The settlement is the sum of `observe_codex_turn` over **every turn id
   mapped to H** (new ledger lookup: reservation -> prompt ids, scanning `shared_hold_prompts` by session;
   built in C2, not C4). If any mapped turn is unmeasurable, H settles at `max(priced_sum, hold)`:
   - it is recorded as measured only when `priced_sum > hold`;
   - otherwise the hold amount, `usage_known=false`, flagged.

   This is the same rule as `future_timestamp` (ADR-0026:280-298). Claude avoids the problem through its
   watermark window; a turn-scoped reader would silently lose T1.
3. Measure each turn with `observe_codex_turn(path, raw_session_id, turn_id, BUDGET_SCAN_CAP_BYTES)`
   (codex_turn.rs:424). It is turn-scoped by `turn_id`, never `root_turn_id`, and never session-wide.
4. Price each turn with a new `price_codex_turn(TurnUsage, model, PricingTable+overrides) -> Option<cents>`:
   - the four disjoint tiers from `CodexRawUsage::into_disjoint` (`crates/daemon/src/usage.rs:392-403`): fresh
     input, cache-write -> cache_creation, cache-read, output;
   - **output already includes reasoning; do not add `reasoning_output_tokens`**;
   - pico-dollar sum, rounded up once per turn (same rule as ADR-0026:231-235);
   - `model` comes from the Stop payload, because the rollout has none. That is host-claimed (I).
5. Outcomes:

| Observation | Booked | `usage_known` | Reason flag |
|---|---|---|---|
| Measured and priced | priced cents (may exceed the hold; the overrun is recorded unclamped) | true | none |
| Model absent or not in table/overrides | hold amount | false | `unpriced_model` (named in `Status.unpriced_models`) |
| `Inconsistent` (half-written tail, session/thread mismatch, running-total mismatch) | hold amount | false | `transcript_inconsistent` |
| `TurnIncomplete` / `ScanCapReached` / `NoUsageInWindow` / `Unreadable` / no path | hold amount | false | existing labels |
| Final record still unflushed (undetectable) | priced short (**under-count**) | true | none at Stop; closed by the true-up in C4 |
| Invalid config file at Stop | hold amount; overrides not used | false | `config_invalid` |
| No hold mapped to this `turn_id`, enrolled, budget valid (admission skipped, a subagent Stop, or the hold already reclaimed) | **post-hoc charge** of the measured cents (else the hold amount) via `book_charge`, key `codex-turn:<lane>|<turn_id>`, executor `agent_absent`, `charge_source=turn_hold`, `is_charge=1` | as measured | `unadmitted_turn` |

**Default honesty.** With no `pricing_overrides` for `gpt-*` models, **every Codex turn settles at the flat
hold with `usage_known=false`**. `status`/`doctor` must say so. Adding OpenAI list prices to the pinned table
needs a cited price source and a `PRICING_VERSION` bump. It is a separate ticket and must not use invented
numbers.

**Idempotency.**
- Hold settle is once per reservation (`AlreadyFinal`).
- A replayed Stop is caught by the turn-id mapping.
- A post-hoc charge is keyed by the turn id.
- The true-up (C4) is cumulative-delta keyed by `codex-trueup:<lane>|<turn>|c<cumulative>`, summed by prefix,
  never refunded (same rule as `ChargeAgentTranscript`, ADR-0026:392-402). Its prefix must also be folded
  into `shared_charge_floor` on `prune` (the HORO-2250 M3 guard, ADR-0026:547-556). Otherwise a pruned
  month's true-up re-books.

**Crash and restart.**
- Hook killed after the grant: a retry with the same turn id reuses the live hold. A post-grant failure is
  already handled atomically (`release_and_forget_unstarted_shared`, ADR-0026:504-515).
- Daemon restart between Admit and Stop: the hold and the prompt mapping are durable, so Stop settles normally.
- Stop never arrives (Codex killed): the hold expires after 1 h. **C4 orphan recovery**: at the lane's next
  `SettleTurn`, every `expired`, unsettled hold with a turn mapping is measured by its own turn id (now likely
  complete, since `task_complete` exists) and settled late in its own row and period, at measured cents or
  else the hold amount. Never zero. Residual: the last turn of a killed session that is never resumed stays
  unsettled (under-count bounded by its real spend). Same class as Claude's killed-host residual
  (ADR-0026:733-735). Disclose it.
- The undetectable unflushed final record (codex_turn.rs:93-118). **C4 true-up**: on the next `SettleTurn` or
  `Admit` for the lane, re-measure the previous settled turn when its `task_complete` line is present; book
  `max(0, measured - (settled_amount + prior true-ups))`. ADR-0026:639-645 rejected reconciliation for
  Claude because of the timestamp watermark. That objection does not apply: the Codex reader is keyed by
  `turn_id` and has no watermark.

**Double-count safety.**
- Hook vs hook: one settle per hold plus turn-id keys.
- Parent vs subagent: Codex subagent spend is not charged in C1-C4, so it cannot be double-counted. It is
  disclosed as not covered. C5 must decide one owner per record: either `turn_id != parent turn` records in
  the parent rollout, or `agent_transcript_path`, never both, keyed by `response_id`.
- Hook vs future gateway: the **cross-source key is `response_id`**, present on every `token_usage_record`
  (fixture line 2). Rule for the B slice: per lane, the metering source is fixed at admission (`hooks` or
  `gateway`). A gateway-metered lane books per-response charges `codex-resp:<response_id>`, and its Stop
  `SettleTurn` releases the hold against those charges instead of re-reading the rollout. A hooks-metered lane
  never accepts gateway charges. Either way a `response_id` is booked at most once.
- Codex vs Claude: impossible by construction (namespaced lanes; Codex `Finalize` skips Claude settlement).

## 5. Sharing the single global hard budget with Claude Code (D1/D3)

- **Same pool, same cap, same hold.** Codex holds and charges land in
  `metered:local-budget/monthly-YYYY-MM`. Every accepted Codex reservation consumes global availability.
  Claude's next `Admit` sees Codex's holds and spend, and vice versa (ADR-0026:73-87).
- The **same `turn_hold_usd_cents`** applies. A per-provider hold would be a new `shared-budget.json` key. It
  is rejected for now (the `deny_unknown_fields` skew above), and it is also an operator-policy question.
- **Soft limits** (per-session / per-agent, warn-only, never mint, never deny) apply unchanged to Codex lanes.
  Warnings reach Codex through `additionalContext`. No per-provider soft limit is added: D1/D3 decided
  per-executor, and adding one is scope creep.
- No over-allocation (D3.6) still holds through the ledger's IMMEDIATE transactions. Codex adds lanes, not a
  new admission rule.
- Rollover: a Codex hold belongs to its admission month. A late settle lands in the hold's row. A post-hoc or
  true-up charge books to its event time (the `token_usage_record` timestamp), with the HORO-2250 retention
  horizon applied.
- Status/doctor: the per-executor table shows Codex lanes, and the pool total reconciles across providers.
  `NOT_COVERED` is reworded from "Codex has no budget gate" to the remaining Codex gaps: subagents; untrusted
  hooks are ungated; no PreToolUse check; unpriced by default.

## 6. Decision levels: what needs the founder, what is engineering, what ships dark

**Engineering (Level 0/1; record the design in an ADR-0026 amendment or a new ADR-0027).** All of these leave
product guarantees unchanged while dark:
- the provider-namespaced lanes;
- protocol v25 (`provider` on Admit/Check, the `SettleTurn` variant, `UnsupportedProvider`);
- reuse of `observe_codex_turn`;
- `price_codex_turn`;
- the Codex `Finalize` skip;
- the heartbeat derivation;
- orphan recovery and true-up;
- tests, `doctor`/`status` wording, and the C0 source verification.

**Dark / off by default.** Codex participates only when `<state_dir>/shared-budget.codex` exists **and**
`shared-budget.json` is valid. With the marker absent:
- Codex hook output is byte-identical to today, and the existing pins stay as regression tests:
  shared_budget_hooks.rs:1041-1055, agent.rs:494-508, shared_budget_view.rs:48, golden_journey.rs:1080.
- The daemon answers `not_configured` to any Codex-provider request and writes nothing.

Everything in C1-C4 is validated offline:
- synthetic and the real-serializer rollout fixture (`codex-rollout-token-usage.jsonl`);
- real hook processes against a real daemon in a temp state dir (`TestOwnedLive`, never `ProductionObserved`).

**Founder Level-2 (one packet):**
1. **Activation.** Supporting or documenting the marker, or defaulting Codex participation on. This activates
   unverified economic effects; the HORO-1714 authorization excluded exactly that (run.rs:33-40). ADR-0026:867
   leaves "a Codex budget gate" undecided.
2. **Codex fail-closed at prompt admission** when the daemon is unreachable or the answer is unusable (new
   user-visible blocking for Codex users), and the `owner-route + enrolment => block` rule.
3. **Financial policy.** Unadmitted Codex spend is charged post-hoc into the shared cap and so reduces Claude
   Code's admission capacity. Also confirm that ChatGPT-plan Codex usage counts at list-price-equivalent USD.
   ADR-0026:112-116 already defines USD that way; this is a confirmation, not a new semantic.
4. **Gateway option B.** A new egress destination, a new upstream credential path, a new enforcement trust
   boundary, and a gateway-down policy.
5. **Real-host evidence authority.** Any sandboxed real `codex` run (even with a fake loopback upstream)
   supersedes the HORO-1714 no-retry decision. Owner action is required
   (`docs/native-agent-lifecycle-characterization.md:81-91`).

Not founder-level but needs data: OpenAI list prices in the pinned table need a cited source and a
`PRICING_VERSION` bump.

## 7. Slices, files, tests, order, tickets

Dependency order: **C0 -> C1 -> C2 -> C3 -> C4 -> C5**. C6, C7 and C8 are gated (evidence or founder).
Each slice is one PR of about 500 lines or less, merged with a merge commit.

### C0. Offline source verification of Codex ids and block semantics (docs only, no host run)
- **Jira:** `[Codex] Verify hook turn_id/session_id vs rollout ids and UserPromptSubmit block semantics from openai/codex source`
- Read openai/codex at the tag matching the installed 0.160.0 (the version of the fixture):
  - the hook payload builders for UserPromptSubmit and Stop (`turn_id` source);
  - `record_observed_response_completed` / `TokenUsageRecord` (`turn_id`, `session_id` source);
  - the UserPromptSubmit `decision:block` handling (upstream #14626);
  - where subagent token usage is persisted;
  - whether Stop fires for subagent turns.
- Files: `integrations/codex/README.md`, `docs/native-agent-lifecycle-characterization.md`. Fix
  `crates/cli/tests/fixtures/agent/codex/stop.json` (add `transcript_path`, which its `_source` claims).
- **AC:**
  - each question answered V-src (with the commit SHA and path) or explicitly U;
  - no real Codex binary executed;
  - the fixture fix keeps `agent_contract_v2.rs` green.
- **Exit gate for C2's measured path:** if hook `turn_id` != rollout `turn_id`, C2 ships with settlement at
  the flat hold only, and the measured reader stays off.

### C1. Provider-namespaced shared-budget lanes (daemon + protocol v25; Claude byte-identical)
- **Jira:** `[Shared budget] Namespace budget lanes by provider and stop Claude settlement from reading Codex rollouts`
- Files:
  - `crates/protocol/src/shared_budget.rs`: `provider: Option<String>` on Admit/Check with
    `skip_serializing_if`; `SettleTurn`; `UnsupportedProvider`;
  - `crates/protocol/src/lib.rs` (`PROTOCOL_VERSION` 25);
  - `crates/daemon/src/shared_budget.rs`: `budget_lane()`, reserved prefix in `validate_identity`,
    `heartbeat`;
  - `crates/daemon/src/server.rs`: dispatch, heartbeat key at :1888, Codex skip in Finalize at :2330;
  - ADR amendment.
- **Failing-first tests** (written and red on main first):
  1. A Claude `Admit` creates hold H for session S. A `Finalize{session:S, provider:"codex", transcript}`
     currently settles H (bug). After the fix H is untouched and no Claude reader runs.
  2. Claude and Codex lanes with the **same** raw session id hold independently. A Claude Stop never
     settles the Codex hold.
  3. A client session starting with `shared-budget-provider-v1:` is refused and writes nothing.
  4. `ToolInvoked{provider:"codex"}` renews only the Codex lane. A Codex hold with no heartbeat expires.
  5. Unknown provider is refused and writes nothing.
  6. A Claude `Admit` JSON is byte-identical to v24 (serde golden). The `agent_contract.rs` goldens are
     unchanged.
  7. Version skew: a v25 Codex-provider Admit to a v24 daemon yields an error, the hook retries once, and
     the daemon restarts (`stale_daemon_error`, budget_gate.rs:362).
- Before C2 lands, the daemon answers every Codex-provider `SettleTurn`/`Admit`/`Check` with
  `not_configured` and writes nothing, so C1 is inert and reviewable on its own.
- **AC:**
  - no Claude-visible byte or row changes;
  - no migration;
  - Codex requests are inert without the marker (C2 adds the marker check; C1 has no hook caller).

### C2. Codex turn settlement in the daemon (dark)
- **Jira:** `[Shared budget] Settle Codex turn holds turn-scoped from the rollout (dark, marker-gated)`
- Files:
  - `crates/daemon/src/shared_budget.rs`: `settle_codex_turn`, the post-hoc unadmitted charge, the marker
    read, new `Status` counters `codex_subagent_turns_unadmitted` and `unadmitted_turns` (volatile, like
    the others);
  - `crates/daemon/src/usage/priced.rs` or a new `usage/codex_priced.rs`: `price_codex_turn`;
  - `crates/ledger/src/shared_pool.rs`: reservation -> mapped prompt (turn) ids lookup;
  - new `crates/daemon/tests/shared_budget_codex.rs`.
- **Tests** (fixture-driven; fault injection by file shape and ledger reopen):
  - measured + overrides = exact cents, rounded up once, with reasoning not double-added (fixture turn 2:
    input 2000, cached 1500, cache-write 400, output 80);
  - default (no gpt overrides): flat hold, `usage_known=false`, `unpriced_model`;
  - half-written tail -> flat hold;
  - rollout `session_id` mismatch -> flat hold, not imported;
  - running-total mismatch -> flat hold;
  - `turn_id` absent from the rollout -> flat hold;
  - replayed `SettleTurn` -> no write;
  - `SettleTurn(T1)` after `Admit(T2)` -> T2's hold untouched (HORO-2250 analog);
  - **interrupt then re-prompt** (Admit T1, no Stop, Admit T2 renews H, `SettleTurn(T2)`) -> H books T1+T2
    usage (red on a naive single-turn reader);
  - **unmapped Stop while another turn's hold is pending** -> that hold is untouched, and a post-hoc
    `codex-turn:` charge is booked (red on a `pending_hold` fallback);
  - late settle of an expired hold lands in its own row and period;
  - no hold -> post-hoc charge, idempotent on retry;
  - invalid config -> flat hold and no overrides;
  - marker absent -> `not_configured` with zero rows (compare the whole ledger);
  - daemon restart (reopen ledger) between Admit and SettleTurn -> settles once.
- **AC:**
  - every Codex turn with a hold settles exactly once;
  - none is booked at 0 when unmeasured;
  - no `root_turn_id` or session-wide count is ever used.

### C3. Codex hook wiring behind the marker
- **Jira:** `[Codex] Admit and settle Codex prompts against the local shared budget when enrolled (dark)`
- Files:
  - `crates/cli/src/agent/run.rs`: Codex admission at :263, using `prompt_id := turn_id` and
    `provider:"codex"`; SettleTurn in `run_turn_completed` before Finalize or owner Stop;
  - `crates/cli/src/agent/budget_gate.rs`: `codex_enrolled()` stat, `provider` parameter;
  - `crates/cli/tests/shared_budget_hooks.rs`.
- **Tests:**
  - **marker absent: the existing Codex test at :1041-1055 stays byte-identical**;
  - marker present and exhausted -> `{"decision":"block"}`;
  - allowed -> context plus a soft-limit warning line;
  - daemon unreachable -> block;
  - `agent_id` present -> no Admit, proceed, logged;
  - owner-route flag + marker -> block with an explicit reason;
  - Stop sends SettleTurn with `turn_id` + `transcript_path`, never prints to stdout, never blocks (5 s
    bound);
  - reserved or blank session -> skip.
- **AC:** the Claude hook goldens are unchanged; the Codex path is unchanged without the marker.

### C4. Orphan recovery and true-up
- **Jira:** `[Shared budget] Recover unsettled and under-measured Codex turns at the next settle`
- Files:
  - `crates/daemon/src/shared_budget.rs`;
  - `crates/ledger/src/shared_pool.rs`: fold the `codex-trueup:` prefix into `shared_charge_floor` on prune.
    The reservation -> turn ids lookup already exists from C2. The true-up is **per turn id within the hold**;
  - the evidence-adapter no-eviction guard, if prune changes.
- **Tests:**
  - killed turn (hold expired, no Stop) settled at the next SettleTurn by its own turn id;
  - a final record appended after settle -> the true-up books exactly the delta, and a replay books 0;
  - a smaller re-measure books nothing (no refund);
  - prune then replay -> 0 (floor).
- **AC:** never a refund; never double-books a turn; residuals documented.

### C5. Operator surfaces and docs
- **Jira:** `[Shared budget] Report Codex enrolment, coverage gaps and pricing state in status/doctor`
- Files:
  - `crates/cli/src/shared_budget_view.rs` (`NOT_COVERED`), `crates/cli/src/doctor_cmd.rs`;
  - `crates/domain/src/agent.rs`: the row stays Unavailable until activation, and the detail notes the
    experimental marker;
  - `integrations/codex/README.md`, ADR.
- **AC:** `doctor` reports:
  - the marker state;
  - Codex hooks wired, and their trust (likely-not-trusted / unknown);
  - **Warn when no gpt pricing overrides exist** (every turn will be booked at the flat hold);
  - Error for owner-route + marker;
  - the subagent and PreToolUse gaps.

  `status` labels are unchanged (`admission-bounded-cooperative`).

### Gated follow-ups (not sliced in detail)
- **C6** Codex subagent charging. Needs C0's answer on where subagent usage lands; one owner per
  `response_id`.
- **C7** Codex PreToolUse `Check` and a Codex flush probe. Needs evidence of deny handling and of flush lag.
- **C8** OpenAI Responses gateway route (option B). Founder Level-2.
- **C9** OpenAI list prices in the pinned table. Needs a cited source.

### Codex affected-only QA (per slice; restrict nextest discovery, see ~/CLAUDE.md rule 5)
- daemon:
  - `--test shared_budget_core`, `--test shared_budget_settlement`, new `--test shared_budget_codex`;
  - `--lib` for the `usage::codex_turn` / pricing units;
  - `--test execution_owner` (owner-route unaffected).
- cli:
  - `--test shared_budget_hooks`, `--test codex_hooks_integration`;
  - `--test agent_contract` (Claude byte-exact) and `--test agent_contract_v2`;
  - `--test legacy_executor_attribution`, `--test shared_budget_golden_journey`;
  - `--test codex_execution_owner_consumer`.
- protocol: serde round-trip and golden tests in `crates/protocol`.
- Manual (only after a founder authorizes real-host evidence): `scripts/codex-smoke.sh` with a fake loopback
  Responses upstream, checking `turn_id` equality, the block effect (the upstream gets zero requests) and
  rollout flush timing at Stop.

## 8. Risks, open questions, first verification

**Risks:**
1. **Id mismatch** (hook `turn_id`/`session_id` vs rollout). Every settle would be unmeasured, at the flat
   hold. A Codex turn can span many model calls, so a flat hold can **under-count** heavily. This is the top
   risk, and C0 retires it.
2. **Unstable rollout format.** The official hooks doc says the transcript "isn't a stable interface". Drift
   makes the reader return `Inconsistent`, so turns settle at the flat hold. Mitigation: the strict reader
   plus doctor and status counters.
3. **Silent ungating.** Untrusted or re-hashed hooks do not run at all, and plain `doctor` cannot prove trust.
4. **Overshoot is larger than Claude's.** Long agentic turns overshoot the per-turn hold. The cooperative
   label covers this, but the operator must size the hold.
5. **Unpriced by default.** Until overrides or C9, accounting is flat-hold-per-turn.
6. **Subagents** are not covered, and C6 carries a double-count risk.
7. Heartbeat depends on `PostToolUse` (async). A tool-free turn longer than 1 h expires its hold; C4
   recovers it late.
8. **Version skew** v24 <-> v25. Claude bytes are unchanged; an enrolled Codex prompt fails closed until the
   daemon restarts.
9. Codex modes (exec, app-server, IDE extension) and resume/compact/fork id stability are U.

**Open questions:**
- Should enrolment eventually move into `shared-budget.json` `schema_version: 2`, with a coordinated upgrade?
- Should a per-provider `turn_hold` exist? This is operator policy, not now.

**Single best first verification (no paid run, no host execution, no authorization needed):** C0's source
read of openai/codex at the tag matching codex-cli 0.160.0. It must answer:
- (a) Is the `turn_id` on the UserPromptSubmit and Stop hook payloads the same value written as
  `token_usage_record.payload.turn_id`?
- (b) Does the hook `session_id` equal the rollout `session_id`?
- (c) Does `{"decision":"block"}` from UserPromptSubmit prevent the model request?

Admission keying, the replay guard and the measured settlement all depend on (a) and (b). The block
guarantee depends on (c).
