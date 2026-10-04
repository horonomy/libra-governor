# HORO-1689 — Real DogFood Evidence (v0.0.3 ITERATE phase)

**Status: in progress, not yet gated.** This directory holds *real* evidence only —
actual `claude -p`/`codex exec` invocations and the actual production ledger's own
receipts — kept physically separate from HORO-1673's synthetic/unit/integration
evidence (`experiments/v003_1673_gate/`, a different, immutable baseline — see that
directory's own README; nothing there is edited by this ticket).

Founder's ITERATE instruction (verbatim, paraphrased context in Jira HORO-1689):
accept HORO-1673 as complete, but its own packet reported INSUFFICIENT real-DogFood
evidence — zero real DogFood data, no real second-window/Codex arm exercised. This
ticket is the narrow evidence-acquisition iteration to close exactly those gaps, not
a feature-expansion ticket. `docs/research/horo-1673/promotion-criteria.md`
(`bf21105`) is NOT edited — same pre-registered floors apply.

## What's real here vs. what's reconstructed

- `raw/task{1,3,4}.json` / `.time` — real `claude -p --output-format json` ground
  truth for three real (non-toy-trivial, but short) coding tasks run against scratch
  repos under `/tmp/libra-dogfood-{1,3,4}`. `task2` (a 4th real task, a queue
  implementation) exceeded a 280s wrapper timeout and was killed — see "Adversarial
  case" below; it has no normal ground-truth JSON because the process never exited.
- `raw/receipts_snapshot_2026-10-03T2000Z.psv` — a pipe-separated dump of every row
  in the **real production ledger** (`/Users/bryant/.local/state/libra-governor/ledger.sqlite3`)
  at the time of this snapshot. Every row here came from a real `claude -p` or
  real interactive Claude Code session — none are synthetic/fixture data.
- `raw/session_tasks_snapshot_2026-10-03T2000Z.psv` — the `session_id -> task_id`
  mapping needed to join a `claude -p` ground-truth `session_id` back to the
  ledger's own `task_id`.
- `raw/economics_explain_<task_id>.json` — Libra's own reconstructed figures for
  tasks 3 and 4, via the real production `economics explain --task <id> --json`
  path (not synthetic).

## Finding 1 — prompt "scope" does not reliably control real duration

All four real dogfood toy tasks (inventory module, bank-account simulation, the
earlier two from the first batch) were deliberately written to be larger in scope
than a trivial fix, on the assumption that would reliably clear the pre-registered
120s/5-tool-call qualifying-pair floor. **It did not.** Real wall durations: 31s/3
tools, 36s/4 tools, 27s/3 tools (plus one earlier ~1 min case) — every one of them
under the floor. Ground truth (`claude -p`'s own `duration_api_ms`/`num_turns`) agrees
closely with the ledger's own `actual_duration_secs`/`tool_call_count` (e.g. task 3:
29.2s API / 5 turns vs. ledger's 36s / 4 tool calls — the ~7s gap is process
startup/hook overhead, not disagreement). **This falsifies the assumption that a
larger greenfield Python toy prompt reliably produces a qualifying pair** — the
model is simply fast at small, well-specified, non-interactive codegen tasks
regardless of nominal "scope." Toy single-shot `claude -p` tasks are not a reliable
generator for this floor and were abandoned as a strategy after this result (see
Finding 2 for what actually works).

## Finding 2 — real, organic multi-turn engineering sessions clear the floor easily, and already have

Two **real, ordinary interactive Claude Code sessions already running during this
same campaign** — one of them *this very session* (`task_id` `be5fa983-…`), and one
independent, concurrent session (`task_id` `8c9623a3-…`, a different `session_id`,
doing unrelated real engineering work elsewhere on this machine) — organically
produced, with zero deliberate gaming:

- `be5fa983-…`: 4 receipts, durations 164s/18 tools, 209s/22 tools, 277s/28 tools,
  574s/40 tools — **all 4 qualify** (≥120s, ≥5 tool calls, `task_features` present).
- `8c9623a3-…`: 17 receipts spanning 355s/19 tools up to 1944s/87 tools as the
  session's cumulative duration/tool-count grew across repeated `Stop` events —
  **all 17 qualify**.

**21 real qualifying calibration pairs exist as of this snapshot, out of the
pre-registered floor of 30** — purely from genuine engineering work, with no
contrived task needed. This is the single most important finding of this phase:
the floor is achievable from real dogfood usage alone; it does not require
inventing synthetic-feeling "make it take longer" tasks. The remaining gap (9 more
pairs) is expected to close from continued real work in this and other real
sessions, not from more toy generators.

## Finding 3 — real adversarial case: a killed task's reservation is honestly reported, never silently settled, and correctly expires

Real dogfood task 2 (queue implementation) exceeded its 280s wrapper timeout and was
SIGKILLed mid-execution — unplanned, but a genuine adversarial case (item 4's
"adversarial/failure cases where appropriate"). Its reservation
(`task_id=0291afb7-b3ff-40ac-be39-2ece34ec2b83`, 75000 tokens, `work_hold`) was
checked via `economics explain --task 0291afb7-… --json` immediately after the kill
and confirmed **active and unsettled** (`settled=0.0`) — the system does not
silently drop or fabricate a settlement for a task that never reached its own
`Stop` hook. **Re-checked again ~2h later** (well past the reservation's TTL,
`raw/economics_explain_0291afb7-crashed-task-post-expiry.json`): the reservation
correctly transitioned to `released.expired` (75000 tokens, `expired_count: 1`),
`active_leases.count` dropped to `0`, and — critically — `settled.observed` and
`settled.assumed` are both `null`/`count_assumed: 0`, i.e. the system never
fabricates a completed settlement for a task that crashed. `expire_stale_reservations`/
reconciliation works correctly on this real crash case.

## Finding 4 — real defect: `model`/`provider` were always `None` on every real receipt

All 11 real receipts at the time of first inspection had **both** `model` and
`provider` NULL. Root-caused and partially fixed in this same worktree/PR (not a
new feature — see Jira **HORO-1690** for the full root cause and the narrower
follow-up left open): Claude Code's real `Stop` hook payload does not expose a
`model` field at all (a stale doc-comment claim was simply wrong — verified against
Claude Code's own hook documentation), and `provider` was hardcoded `None` even
though the calling CLI entry point (`hook stop` vs. `codex-hook stop`) already knows
which host it is via the existing `AgentKind` enum. This PR threads that
already-known value through so every *future* real receipt distinguishes the
Claude Code arm from the Codex arm — the minimum fix needed to make this ticket's
cross-agent/provider-evidence requirement testable at all. Real `model` capture
(e.g. `"claude-sonnet-5"` vs whatever Codex reports) still requires a new
`SessionStart` hook wired end-to-end — tracked as HORO-1690's own remaining AC, not
done here, since it is new hook-wiring, not a fix to the existing hypothesis.

## Finding 5 — without a gateway, Libra's own "actual usage" reconstruction is a flat assumed figure, not an observed one

`economics explain` for both task 3 and task 4 reports **identical** `settled`
figures (`"assumed": {"value": 75000.0, "basis": "libra_reservation_hold"}`,
`"observed": null`) despite their real ground-truth costs differing
($0.3188 vs $0.2987, confirmed via `claude -p`'s own `total_cost_usd`). This is
**documented, intentional behavior**, not a bug — the code's own comment explains
settlement conservatively falls back to the reserved amount because Claude Code's
hook payloads expose no token/cost figure, rather than fabricating one. But it is a
real, load-bearing limitation for any claim that Libra's own ledger can be compared
against ground truth on the *cost* dimension without a gateway configured: today it
cannot — only the *duration*/*tool-call-count* dimension (Finding 1) has real
ground-truth agreement. This is recorded as a finding, not "fixed" — fixing it
would require standing up the enforcement gateway, which is new scope, not a
defect in the existing hypothesis.

## Finding 6 — real defect: `v003_gate`/`calibration_pairs()` could not parse the real ledger's own oldest history at all

Running `v003_gate` against the real production ledger for the first time (this
phase's whole point) immediately panicked: `calibration_pairs query must succeed
against a valid ledger schema: Sqlite(InvalidQuery)`. Root-caused to three separate,
real schema-drift issues in the *actual* ledger, none hypothetical:

1. The ledger's single oldest row (2026-09-17, pre-dating this campaign's current
   HORO-1130/1671 migrations) has `task_features_json` stored as an **empty
   string**, not SQL `NULL` — `parse_task_features`/`parse_regime`/the inline
   `estimate_json` parse in `calibration_pairs()` all treated "non-NULL" as "must be
   valid JSON", so this one real historical row aborted the entire query.
2. That same row's `estimate_json` (a real pre-HORO-1130 cold-start estimate) has
   neither `feature_schema_version` nor `bucket_tier` — both fields added by
   HORO-1130, after this estimate was written — and neither had `#[serde(default)]`
   (unlike `regime`, which already does, for the identical pre-HORO-1671 reason).
3. `v003_gate.rs` itself mislabeled `calibration_pairs()`'s second return value
   (`dropped`) as `"total receipts"` in its own printed output — a correct real
   ledger with only 1 dropped row out of 36 total would print "(of 1 total
   receipts)", reading as if almost no real data existed.

All three fixed (see commits on this branch); `cargo test --workspace` (706 tests)
still green. **Separately**, once the gate could actually run, it reported `n=0,
skipped=0` for every one of its three predictors despite 35 real qualifying
pairs — root-caused to a *fourth*, more consequential real defect: `handle_finalize`
never attached a `regime` to any `ExecutionReceipt` at all (`receipts.regime_json`
was `NULL` on literally every real row, confirmed via direct query), even though
`current_regime()` was already computed and used at preflight/tool-invoked time
elsewhere in the same file. The gate's comparison loop requires `pair.regime` to be
`Some` and silently `continue`s otherwise — so with it always `None`, *no* real
receipt could ever contribute a scored comparison, regardless of how many
qualifying pairs existed. Fixed by attaching `current_regime(...)` at finalize time
too, matching the preflight/tool-invoked pattern. This is the single most consequential
fix in this phase: without it, the gate could never produce a real n>0 result from
any amount of real dogfood evidence, ever — exactly the kind of instrumentation gap
item 6 of the founder's ITERATE instruction anticipated.

**Verified fixed** (PR #65 merged as `1c290b0`): the live daemon
(`/Users/bryant/.cargo/shared-target/debug/libra-governor`) was rebuilt from merged
`main` and restarted. A real `hook user-prompt-submit` → `hook stop` round trip
produced a receipt (`task_id=3f5ca53f-...`) with `model`, `provider`, and
`regime_json` all populated, confirmed by direct query against the real production
ledger. Every receipt recorded *before* the restart (the entire HORO-1673-era
history, including all 21 qualifying pairs from Finding 2) still has
`regime_json = NULL` and stays that way permanently — frozen historical data, not
retroactively fixed. The gate must accumulate *newly recorded* real receipts,
post-redeploy, before it can report a real n>0 comparison.

## Finding 7 — real Codex evidence: `codex exec` requires either interactive hook-trust or `--dangerously-bypass-hook-trust`, and its `Stop` payload *does* expose `model` (unlike Claude Code's)

Installing Codex hooks (`libra-governor install --agent codex`, wiring
`~/.codex/hooks.json`) is **not sufficient** on its own: Codex's hook system
requires a one-time interactive trust step (`/hooks` inside an interactive `codex`
session, recorded by content hash) before hooks actually execute. A first real
`codex exec --skip-git-repo-check` run (to-do-list CLI task) completed successfully
but produced **zero** new ledger receipt — confirmed via `grep -i codex
daemon.log` showing no hook activity at all — an honest, real negative result, not
a bug: untrusted hooks are silently skipped by Codex, by design. Non-interactive
automation has no way to complete the interactive trust flow.

Codex exposes `--dangerously-bypass-hook-trust`, documented as "intended only for
automation that already vets hook sources" — applicable here since the hook
command is this repository's own, already-reviewed `libra-governor codex-hook`
binary. Re-running with that flag (two real tasks: URL shortener, LRU cache) did
produce real receipts:

| task_id | duration | tool_calls | model | provider |
|---|---|---|---|---|
| `6efdd0c8-...` | 66s | 4 | `gpt-5.6-sol` | `codex` |
| `2a84b5b2-...` | 33s | 3 | `gpt-5.6-sol` | `codex` |

This is real, direct falsification of the `TurnCompletedPayload` doc comment's
"treated the same way for Codex until proven otherwise" hedge (see Finding 4):
**Codex's real `Stop` payload does expose `model`** (`"gpt-5.6-sol"`, correctly
captured with zero additional code changes — `model` was already wired through for
whichever host's payload carries it), while Claude Code's genuinely does not. The
`provider` fix (Finding 4) is what made this cross-agent distinction visible and
queryable at all. Neither Codex task individually qualifies under the 120s/5-call
floor (consistent with Finding 1 — small greenfield Python tasks finish fast
regardless of which agent/provider executes them), but both are real,
non-fabricated cross-agent/provider evidence, satisfying the ITERATE instruction's
item 4 requirement for a real Codex arm. Ground truth for the third real Codex run
(LRU cache, `--json` JSONL event stream) independently confirms real token usage
(`input_tokens: 66639, cached_input_tokens: 62720, output_tokens: 1042`) — Codex's
own `--json` mode is this arm's equivalent of `claude -p --output-format json`.

## Pending (not yet done as of this snapshot)

- Accumulate ~9 more real qualifying pairs (organic work, not contrived — see
  Finding 2) *after* the daemon restart, and rerun `v003_gate` to get a real,
  non-zero-n scored comparison with `regime` actually populated.
- Kill-daemon-mid-task adversarial case (not yet attempted).
- A real Codex task large/long enough to individually clear the 120s/5-call
  qualifying floor (both real Codex tasks so far were sub-floor, like Finding 1).
- Regenerate the founder decision packet with explicit before/after vs. HORO-1673,
  once the gate has been run against this real, post-fix data.

## Explicit non-goals (per the founder's ITERATE instruction)

No product features were added. No threshold in `promotion-criteria.md` was
weakened or touched. The only code change in this phase is the scoped `provider`
fix (Finding 4) — fixing exactly the defect blocking correct testing of the
existing cross-agent hypothesis, nothing broader.
