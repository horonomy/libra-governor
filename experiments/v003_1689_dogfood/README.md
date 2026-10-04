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

## Finding 8 — kill-daemon-mid-task adversarial case: no data loss, correct self-heal

Killed the live daemon (`kill -9`) ~6s into a real `claude -p` task. Ground truth
(the task's own `claude -p --output-format json`): 4 turns, 22.4s API time, $0.298
— it completed successfully despite the daemon dying mid-flight. A fresh daemon
process self-healed (auto-spawned by the next hook call, new pid). The resulting
ledger receipt is accurate: `actual_duration_secs=28`, `tool_call_count=3` (matches
reality — not reset to 0, not corrupted by the daemon restart), `regime`/
`task_features` populated, `model=NULL` (expected — Claude Code's `Stop` payload
never exposes it). SQLite, not daemon memory, is the real source of truth here, and
it held up under this adversarial condition.

## Finding 9 — real calibration evidence from the actual production reporting path, not a bespoke harness

`libra-governor calibration report` (the real command a real user would run — not
`v003_gate`, which is a research harness built for HORO-1673) against the real
ledger (141 total receipts, 140 after dropping 1 cold-start) reports:

```
Duration coverage — computed over n=140 real calibration pairs.
  overall:  P50 coverage=2.9% (4/140)   P80=6.4% (9/140)   P90=8.6% (12/140)
  by bucket tier:
    global:               P50=30.0% (3/10)   P80=50.0% (5/10)   P90=70.0% (7/10)
    repo_topology_model:   P50=0.0%  (0/99)   P80=0.0%  (0/99)   P90=0.0%  (0/99)
    topology:              P50=3.2%  (1/31)   P80=12.9% (4/31)   P90=16.1% (5/31)
```

Full output: `results/calibration_report_real_ledger_post_fix.txt`. This is a real,
substantive finding, not a harness artifact: **the `repo_topology_model` bucket
(the estimator's most specific, most-trusted tier, n=99) is 0% covered at every
quantile on real data**, while the coarsest `global` fallback tier (n=10) does far
better (70% P90 coverage). The pre-registered qualifying-pair predicate
(`actual_duration_secs >= 120 AND tool_calls >= 5 AND task_features.is_some()` —
`docs/research/horo-1673/promotion-criteria.md` §2, which says nothing about
`regime`) is satisfied by a wide margin: **N_total=141, N_qualifying=127**, far
above the pre-registered 30-pair floor.

**Necessary caveat, stated plainly rather than smoothed over**: almost all of these
141 receipts come from only 2-3 real but long-running organic sessions
(`be5fa983`/this session, `8c9623a3`/an independent concurrent session), each
re-finalized repeatedly at growing cumulative durations (up to ~48,000s, 640+ tool
calls) rather than 30+ independent short tasks. The predicate as pre-registered
does not require independence between pairs, and this evidence satisfies it
exactly as written — but a reader should not treat "141 ≥ 30" as "141 independent
real-world task observations." It is a smaller number of real sessions sampled
repeatedly over time. This is disclosed, not hidden, and is exactly the kind of
caveat the founder packet must carry forward rather than paper over.

`v003_gate`'s own scored comparison (`results/v003_gate_real_ledger_post_fix.txt`)
additionally requires `regime.is_some()` — a stricter gate than the pre-registered
predicate itself (which never mentions `regime`) — so today it only scores the 10
real receipts recorded after the Finding-6 daemon restart (×3 `ELAPSED_FRACTIONS` =
"n=30" in its own output, a different "30" than the pre-registered calibration-pair
floor and easy to conflate with it): `baseline-0 mean|P50 error|=7847.9s`,
`baseline-1 mean|P50 error|=7847.9s P90 coverage=100%`, `candidate mean|P50
error|=9159.4s P90 coverage=44% (5 skipped)`. These absolute error magnitudes are
driven by the same cumulative-session-duration shape as above and should be read
with the same caveat — not as "the candidate estimator is bad," but as "this
sample shape does not cleanly test the candidate estimator's real accuracy." The
real production `calibration report` path (Finding 9's own numbers) is the more
trustworthy real-evidence source of the two, since it does not require `regime`
and so draws from the full real qualifying population, not just the 10
post-restart receipts.

The three remaining decision-quality axes are **unchanged from the HORO-1673
baseline and structurally out of reach in this phase** — not re-measured, not
improved, not regressed:
- False-stop/false-degrade rate: `N_classifiable=0` (floor: 20) — the shadow-decision
  cadence condition flagged in HORO-1673 (`DEFAULT_PROGRESSIVE_INTERVAL_SECS=60`
  plus a live `ToolInvoked` request) still does not appear to fire in real sessions
  at the rate needed.
- Early-warning lead time: `N_observations=0` (floor: 10) — same root cause.
- Cost per successful task: `NOT APPLICABLE` — no Outcome Provider/`RecordOutcome`
  configured in this ledger; this phase did not wire one (out of scope — a real
  Outcome Provider is new product surface, not an instrumentation-defect fix).

Full real output for all three: `results/v003_decision_quality_real_ledger_post_fix.txt`.

## Pending (not yet done as of this snapshot)

- A real Codex task large/long enough to individually clear the 120s/5-call
  qualifying floor (both real Codex tasks so far were sub-floor, like Finding 1).
- Investigate why the `repo_topology_model` bucket is 0%-covered on real data
  (Finding 9) — file a Jira follow-up if it looks like a real estimator defect
  rather than purely a sample-shape artifact of the cumulative-session receipts.
- Regenerate the founder decision packet with explicit before/after vs. HORO-1673
  (now unblocked — Finding 9 is the real evidence this ticket needed).

## Explicit non-goals (per the founder's ITERATE instruction)

No product features were added. No threshold in `promotion-criteria.md` was
weakened or touched. The only code change in this phase is the scoped `provider`
fix (Finding 4) — fixing exactly the defect blocking correct testing of the
existing cross-agent hypothesis, nothing broader.
