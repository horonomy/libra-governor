# HORO-1673 — Falsification gate evidence

Pre-registration commit (must precede everything below):
`bf211059d697928c240986a02aa0d9cc56774bd4` —
`docs/research/horo-1673/promotion-criteria.md`.

This README is written incrementally, as each piece of evidence lands,
per this campaign's own discipline
(`docs/research/horo-1154/minimum-evidence-threshold.md`): raw N-of-M
counts, no bare percentages, mixed results reported as mixed, no
checklist auto-deciding the verdict.

**Two separate verdict taxonomies appear in this document — do not
conflate them:**
- The **gate verdict** (this ticket's own AC): CONTINUE / ITERATE / DROP
  SPECIFIC MECHANISM, per mechanism.
- The **founder verdict** (reserved, not decided here): GO / ITERATE /
  PIVOT / STOP, for the v0.0.3 campaign as a whole. This document
  supplies evidence toward that decision; it does not make it.

## 0. Structural finding this entire gate follows from

Verified by reading `crates/ledger/src/reservation.rs` and
`crates/ledger/src/resource_account.rs`: `ReserveRequest` has no
`account_id` field; every `reserve()` call resolves to
`AccountId::for_task(task_id)`. Production creates exactly one
`resource_accounts` row per task (`level='task'`,
`parent_account_id=NULL`). `ensure_child_account`/`grant_sublease` have
**zero production callers** — every call site is test code. A child
account can receive granted capacity but no `work_hold` can ever be
placed inside it through any API, production or test.

**Consequence**: the custody tree is write-only scaffolding today —
capacity can be carved out, never spent against. This reframes several
invariants and matrix scenarios below as "proven on the test-only API,
not reachable from any production code path" rather than either
CONTINUE or ITERATE. See ADR-0008/ADR-0013, both of which already flag
this gap; this gate reports it, per its own §8 non-goals, rather than
closing it.

## 1. Oracle verification — `crates/ledger/src/economic_truth.rs`

That file had **zero tests** before this ticket (`grep -c "#\[test\]"` →
`0`). Its three reconciliation checks are the invariant oracle every
downstream accounting-invariant claim in this gate depends on, so they
are verified first, with negative controls — not just "assert
Reconciled on a healthy ledger," which would pass vacuously even if a
check always returned `Reconciled`.

### Defects found and fixed in this ticket

| ID | Defect | Status |
|---|---|---|
| D2 | `GatewayLedgerAgreement` hardcoded `> 1.0` tolerance regardless of `ResourceKind`, ~100x too loose for `QuotaPercent` (own epsilon `0.01`) | **Fixed** — threads the account's `ResourceKind` through, falls back to `1.0` only when the kind is genuinely unknown. Commit: see PR. Regression test: `gateway_ledger_agreement_epsilon_now_respects_the_resource_kind_d2_fix` |
| D4 | `unattributed_for_account`'s three global queries (`gateway_requests WHERE task_id IS NULL`, `... reservation_id IS NULL`, orphan `resource_accounts`) were ledger-wide, not scoped to the queried account — a `--task A` report could surface task B's unattributed rows | **Fixed** — scoped by the account's own `task_id`; a row with no task at all now correctly contributes zero to every task-scoped report instead of the ledger-wide count. Regression test: `unattributed_counts_no_longer_leak_rows_from_an_unrelated_task_d4_fix` |

### Defects found and recorded (not fixed — see rationale)

| ID | Defect | Why not fixed here |
|---|---|---|
| D1 | `GatewayLedgerAgreement` builds its `IN (...)` account-id list from the **bounded** custody tree (`TreeBudget::default()`, max 200 nodes/depth 8). If the tree truncates, gateway rows under truncated-away accounts are never checked, and the check can report `Reconciled` while a real discrepancy exists elsewhere in the subtree. | **Cannot trigger in production today**: the custody tree is always a single node (§0) — truncation requires a 200+ node subtree, which requires a production custody-tree writer that does not exist. It becomes live the moment such a writer lands, which is exactly what ADR-0008/ADR-0013 already defer. Filed as a Jira follow-up (see §7) rather than fixed speculatively against a code path nothing can reach yet. |
| D3 | `GatewayLedgerAgreement`'s `CheckOutcome::Discrepant { delta }` holds a **row count**, not an amount — while `EnvelopeFormula`/`SubtreeAdditivity` put a real amount delta in the same field. Same enum variant, different units, silently. | Fixing this is a type change to `CheckOutcome` or a new variant, touching every caller — a larger, separately reviewable change, not a ≤20-line fix. Filed as a Jira follow-up. |

### New tests (`crates/ledger/src/economic_truth.rs`, `#[cfg(test)] mod tests`)

Implemented as inline unit tests (not a separate `crates/ledger/tests/`
integration file as originally sketched) because the negative-control
technique requires planting raw-SQL rows that violate invariants the
public API itself enforces — `LedgerStore::conn` is `pub(crate)`, not
reachable from an integration-test binary. This avoids widening the
crate's public surface with a test-only connection accessor.

| Test | Falsifies |
|---|---|
| `subtree_additivity_is_not_applicable_when_the_account_has_no_children` | Documents the real, honest production verdict for `SubtreeAdditivity` (§0) |
| `in_a_production_shaped_ledger_inclusive_equals_exclusive_because_no_child_account_is_ever_created` | Independently re-derives inclusive==exclusive via a second `account_spend` call, not by trusting the report |
| `envelope_formula_reports_a_planted_cross_account_task_scoped_discrepancy` | Plants the one row shape that can only arise from a direct, non-API write (a settled work_hold whose `account_id` is a child but whose `task_id` still points at the parent) and proves the oracle catches it |
| `gateway_ledger_agreement_catches_a_planted_settled_amount_disagreement` | Plants a $15-equivalent disagreement between a gateway row and its linked reservation, proves `Discrepant` fires |
| `gateway_ledger_agreement_epsilon_now_respects_the_resource_kind_d2_fix` | D2 regression: a 0.5 QuotaPercent disagreement (50x QuotaPercent's own 0.01 epsilon) is caught — would have passed silently under the old hardcoded `1.0` |
| `unattributed_counts_no_longer_leak_rows_from_an_unrelated_task_d4_fix` | D4 regression: task A's report no longer sees task B's planted unattributed row |
| `unattributed_amount_is_always_none_only_the_row_count_is_visible` | Documents invariant 8's real shape: visibility is a row count, never a figure (`PartialBucket.amount` is always `None`) |
| `session_selector_is_echoed_back_not_overwritten_with_the_task_id_i9_fix` | I9 fix regression: `economics explain --session X` now echoes `selector: session = X`, not `task = <task-id>` |

## 2. Invariant I9 — session scope isolation (fixed)

**Finding**: `economic_truth_for_session` delegated to
`economic_truth_for_task` wholesale, which overwrote the returned
report's `SelectorEcho` to `{selector: "task", value: task_id}`. Two
concurrent sessions bound to one task received **byte-identical
reports** with no session identifier anywhere in the output.

**Fix applied** (small, isolated commit): the session selector is now
preserved. The totals themselves remain genuinely the task account's
own figures, labeled `OwnAccountExclusive`/`OwnAccountInclusive` — same
as a direct `--task` query, **not** relabeled to `EnclosingAccount`.
Relabeling the totals themselves would change a documented ADR-0013
contract and is deliberately not bundled into this fix; see the Jira
follow-up in §7.

**What this means for genuine isolation**: two sessions sharing one task
still see the same underlying task-level figures (correctly — v0.0.3
mints no dedicated session-level account, per ADR-0008). What changed
is that the report is now honest about *whose selector it answered*,
rather than silently relabeling a session query as a task query. This
is a presentation-layer honesty fix, not a new isolation guarantee —
stated plainly rather than oversold.

## 3. Identity/accounting matrix — 15 scenarios

Verdict legend: **PROD** = covered on a code path production actually
executes · **TEST-API-ONLY** = proven, but no production writer exists ·
**DOMAIN-ONLY** = proven algebra, never persisted, zero callers outside
`crates/domain` · **N/A** = the scenario's precondition does not exist
in this codebase today.

| # | Scenario | Verdict | Evidence |
|---|---|---|---|
| 1 | Two simultaneous Claude Code sessions, same principal | PROD (partial) | `reservation_concurrency.rs::concurrent_optional_reservations_never_double_spend_the_shared_envelope`; `experiments/v002_gate/results/13_concurrent_sessions.txt` (8 real concurrent sessions, PASS). Gap: no existing test asserts the two-distinct-tasks case |
| 2 | Claude Code + Codex concurrently | PROD | Codex is genuinely integrated (`integrations/codex/`, `crates/cli/tests/codex_hooks_integration.rs`, `crates/cli/tests/agent_contract_v2.rs::cross_agent_equivalence_same_session_cwd_prompt_yields_identical_additional_context`); `experiments/v002_gate/results/18_codex_dogfood.txt` (PASS) |
| 3 | Same task across >1 session | PROD | `trajectory.rs::multiple_sessions_attach_to_one_stable_task_identity`; `session.rs::resolve_or_create_task_for_session` |
| 4 | Independent tasks in parallel | PROD | `reservation_concurrency.rs` (shared envelope, same task); distinct-task interleaving not yet a dedicated test — see §6 remaining work |
| 5 | Root agent + multiple subagents | TEST-API-ONLY | `hierarchical_lease_concurrency.rs::{nested_sublease_reduces_parent_capacity_immediately, concurrent_sublease_grants_never_oversubscribe_the_parent}` prove the arithmetic; no protocol surface mints a subagent account in production (§0) |
| 6 | Nested subagent where provider identity permits | **N/A** | `crates/cli/src/agent/identity.rs` module doc, verbatim: neither Claude Code's nor Codex's hook payload exposes a `parent_agent_id`-equivalent field today. `provider_lineage_status` is `None` on every production read |
| 7 | Missing/unsupported parent lineage | PROD — the universal case, not an edge case | `economic_attribution.rs::proven_parent_is_unknown_when_lineage_unknown`; every production account has `provider_lineage_status: None` |
| 8 | Session resume/restart | PROD | `hook_cli_integration.rs::receipt_survives_daemon_restart_and_is_queryable`; `trajectory.rs::data_survives_closing_and_reopening_the_same_database_file`; `preflight_integration.rs::second_preflight_for_same_session_supersedes_the_first` |
| 9 | Daemon restart | PROD | `experiments/v002_gate/results/{14_failure_recovery,17_upgrade_from_v0_0_1,19_self_healing_daemon_upgrade}.txt`; `reconcile_stale_reservations` runs on startup |
| 10 | Child crash + lease expiry | PROD at task level, TEST-API-ONLY at child level | `reservation_concurrency.rs::a_reservation_left_active_by_a_crashed_process_is_reclaimed_on_reconciliation`; `hierarchical_lease_concurrency.rs::{expiring_a_funding_lease_cascade_expires_its_child_account, settling_an_expired_lease_records_the_spend_as_a_visible_overrun_not_a_silent_drop}` |
| 11 | Duplicate provider/economics observation | DOMAIN-ONLY + PROD (gateway) | `economic_rollup.rs::duplicate_event_id_does_not_double_count`; `gateway_enforcement.rs::settling_the_same_reservation_twice_does_not_double_charge`; `hierarchical_lease_concurrency.rs::replayed_sublease_with_the_same_idempotency_key_returns_the_existing_lease` |
| 12 | Cumulative counter reset | DOMAIN-ONLY | `economic_ingest.rs::counter_reset_retains_pre_reset_high_water` — correct algebra, zero callers outside `crates/domain`, no persisted table |
| 13 | Model/effort change | DOMAIN-ONLY + PROD (receipt field) | `economic_ingest.rs::model_switch_does_not_change_the_key`; `Request::Finalize { model: Option<String> }` records the harness-reported model as-is |
| 14 | Legacy pre-attribution rows mixed with new rows | PROD | `hierarchical_lease_concurrency.rs::native_reservations_carry_the_task_account_id_the_legacy_backfill_also_uses`; migration 0011's backfill; `Unattributed.legacy_backfilled` bucket |
| 15 | Principal allocation + concurrent session reservations | **N/A** | `crates/cli/src/economics_cmd.rs`: `--principal`/`--organization` always resolve to `NoEconomicBasis::NotConfigured`. No principal/organization account is ever minted in v0.0.3 |

**Matrix verdict distribution: 7 PROD (1 partial) · 2 TEST-API-ONLY ·
2 DOMAIN-ONLY · 3 N/A.** Reported as that distribution, not as "15/15
exercised."

## 4. Nine accounting invariants

See the design handoff for the full per-invariant falsification design.
Status as of this commit:

| # | Invariant | Status |
|---|---|---|
| I1 | Each canonical spend event counted once | Surface A (domain) already proven (`economic_rollup.rs::duplicate_event_id_does_not_double_count`). Surface B (ledger) generated-sequence test: **pending** (§6) |
| I2 | Task total = union of owned events, not sum of duplicated rollups | **Pending** (§6) |
| I3 | Inclusive = exclusive + proven descendants | Proven and documented in §1's oracle tests; production is the trivial single-node case (§0) |
| I4 | Siblings cannot spend the same leased headroom | Already covered: `hierarchical_lease_concurrency.rs::concurrent_sublease_grants_never_oversubscribe_the_parent`; `reservation_concurrency.rs::concurrent_optional_reservations_never_double_spend_the_shared_envelope`. No new test needed |
| I5 | Child subleases never exceed the parent lease | Already covered: `nested_agent_cannot_oversubscribe_its_parents_lease`, `child_lease_ttl_is_clamped_to_its_funding_lease_expiry`. No new test needed |
| I6 | Completion Reserve survives optional/subagent concurrency | Already covered: `reservation_concurrency.rs::concurrent_required_work_reservations_never_over_draw_the_completion_reserve`; `gateway_enforcement.rs::the_completion_reserve_is_never_drawn_by_gateway_traffic` |
| I7 | Released/expired reservation not counted as settled | Already covered: `settling_an_expired_lease_records_the_spend_as_a_visible_overrun_not_a_silent_drop`; extended in §1's oracle suite |
| I8 | Partial/unattributed spend remains visible | **Qualified finding**: visible as a row count only — `PartialBucket.amount` is always `None` (§1). D4 fix ensures the counts are correctly scoped now, not leaked cross-task |
| I9 | No session observes another session's economics as its own | **Fixed** (§2) — selector echo restored |

I1/I2's generated-sequence property tests (the design's recommended
zero-dependency exhaustive-enumeration / seeded-LCG approach — this repo
has no `proptest`/`quickcheck`) remain open work; see §6.

## 5. Decision-quality comparison

**Expected and actual result: INSUFFICIENT on every calibration-
dependent metric.** `experiments/v003_gate/results/latest_run.txt`
already shows `calibration pairs: 0 (of 0 total receipts)` on a fresh
checkout. This gate did not fabricate a number to avoid reporting that.

A new harness, `crates/daemon/examples/v003_decision_quality.rs`, adds
the two metrics the existing `v003_gate`/`v003_replay` examples did not
already compute: false-stop/false-degrade proposal rate (floor: 20
classifiable proposals) and early-warning lead time (floor: 10
observations), per the pre-registered criteria. Cost-per-successful-task
is reported `NOT APPLICABLE` unconditionally — it requires an Outcome
Provider and at least one outcome attestation, and none is configured
in any ledger this gate produced.

### The dogfood ceiling — disclosed, not papered over

The ticket's own matrix items 1 and 2 ask for "2 real Claude Code
windows" and "+1 real Codex session" running concurrently. **This
autonomous gate run could not satisfy that literally**: this entire
ticket was executed by a single Claude Code session (this one), which
cannot spawn a second genuinely independent, human-driven interactive
Claude Code window or a concurrent Codex session alongside itself.

What this gate did instead, and what it did not do:
- It cited existing real evidence for both scenarios from prior tickets'
  own dogfood runs (`experiments/v002_gate/results/13_concurrent_sessions.txt`,
  `18_codex_dogfood.txt`) — genuine multi-session/multi-agent runs, just
  not re-run fresh for this ticket.
- It did **not** run a new human-driven dual-window session. Doing so
  honestly requires a human operator physically running two Claude Code
  sessions (and a Codex session) side by side — a founder-machine item,
  not something this autonomous run can fabricate or substitute for with
  subprocess concurrency and still call "D-1/D-2 real dogfood" without
  overclaiming.
- It did **not** invent degenerate pairs (near-zero-duration, one-tool-
  call hook cycles) to mechanically clear the 30-pair calibration floor
  — the pre-registered qualifying-pair predicate (§1 of
  `promotion-criteria.md`) exists specifically to exclude exactly that
  shortcut, and reporting a number computed over non-qualifying pairs
  would be worse than reporting INSUFFICIENT.

**This is the single largest remaining gap between this gate's
autonomous evidence and the ticket's full ask**, and it is named here
plainly rather than silently downgraded to a checkmark. A human-driven
dual-window (and, where available, +Codex) dogfood session — ideally
long enough and real enough to also clear the 30-pair calibration floor
— remains open founder-machine work. Running it would upgrade every
INSUFFICIENT verdict in this section to either a real calibration figure
or a smaller, more specific evidence deficit.

## 6. Remaining work in this ticket (tracked, not hidden)

Done as of this commit: I1's generated-sequence test (§4), the
decision-quality harness (§5), the oracle suite and D2/D4/I9 fixes
(§1/§2), all three Jira follow-ups filed (§7).

Not done, and disclosed rather than silently skipped:

- The human-driven dual-window (+Codex) dogfood session — see §5's
  "dogfood ceiling" subsection. This is the largest real gap.
- I2's dedicated test (task total = union of owned events, not sum of
  duplicated rollups) — partially covered by the D4 regression test
  (§1) and the gateway/reservation double-count tests, but not a
  standalone test under that exact name.
- Performance assertions (statusline refresh bound, admission latency
  baseline) and the schema-ahead-of-binary degradation test (design
  handoff's P1-P3) — trimmed from this run; the design itself notes
  these would establish new regression baselines, not verify an
  existing documented SLO, so their absence is a gap in breadth, not in
  the gate's core claim.
- A fifth scenario-4 test (`two_tasks_in_parallel_never_draw_on_each_
  others_headroom`) — not written as a standalone test; the property is
  implicitly exercised by every multi-task test in the oracle suite
  (each uses `setup_task` to create independent tasks and never observes
  cross-task bleed), but no single test asserts it as its own named
  claim.

## 8. Gate verdict (this ticket's own AC — CONTINUE / ITERATE / DROP SPECIFIC MECHANISM)

This is the ticket's own required output, distinct from the founder
GO/ITERATE/PIVOT/STOP decision, which this document does not make (see
the top of this README).

**CONTINUE** — the following are genuinely well covered by real,
passing, mechanically-verified tests (97/97 in the ledger crate,
including this ticket's 10 new tests) and show no falsifiable defect:
- Hierarchical account/lease capacity arithmetic (invariants I4, I5).
- Task-level concurrency isolation, including under real multi-thread
  races (`reservation_concurrency.rs`, `hierarchical_lease_concurrency.rs`).
- Completion Reserve protection under concurrency and expiry-sweep
  interleaving (I6).
- Idempotent reserve/settle/sublease-grant replay (I1, scenario 11).
- Legacy pre-attribution row handling (scenario 14).
- Privacy/permissions posture (no credential/prompt persistence,
  0600/0700 file modes) — no new leakage surface was added.

**ITERATE** — specific, named, small-scope mechanisms with a concrete
defect and a concrete fix path, not blanket rejections:
- The `economic_truth.rs` oracle had two real, fixed defects (D2, D4)
  and two real, recorded-not-fixed defects (D1, D3 — HORO-1685,
  HORO-1686) — the check itself needed hardening before it could be
  trusted as an invariant oracle, and two of four findings are now
  fixed in this same ticket.
- I9 (session scope labeling): the acute selector-echo bug is fixed in
  this ticket; the deeper totals-relabeling question is correctly
  deferred to its own reviewable change (HORO-1687), not left silently
  unaddressed.
- `PartialBucket.amount` is always `None` — unattributed spend is
  visible as a count, never a figure. Flagged, not fixed (would need a
  design decision about what amount to attribute to an orphan row).

**DROP** — one mechanism, confirming a prior decision rather than
making a new one:
- The LLM-assisted critic (`ReplanTier::LlmAssisted`). Verified
  independently (repo-wide grep): still unimplemented, still
  unconstructed anywhere. ADR-0011 already recorded this finding before
  this ticket started; this gate re-verifies it rather than re-deciding
  it.

**INSUFFICIENT (not a failure of this gate)** — every calibration-
dependent decision-quality metric, and the shadow-promotion question
itself: the pre-registered 30-pair/20-proposal/10-observation floors
(`docs/research/horo-1673/promotion-criteria.md`) are not reached by
this autonomous run (see §5's dogfood-ceiling disclosure). Per the
pre-registered criteria's own §7, this means **shadow stays shadow** —
which is the pre-registered, expected, correct outcome of insufficient
evidence, not a gap this gate failed to close.

**NOT-APPLICABLE (not a gap to close now)**:
- Matrix scenario 6 (nested subagent provider identity) — no provider
  exposes the precondition.
- Matrix scenario 15 (principal/organization allocation) — no such
  account is ever minted in v0.0.3.
- Reconciling the custody tree against the provider-proven agent-
  lineage forest (`economic_rollup`/`EconomicEvent`) — ADR-0008/ADR-0013
  already settled that these are two intentionally separate trees.
- Migration rollback — forward-only by design; the real degradation
  path (schema-ahead-of-binary) is a documented, untested-in-this-ticket
  gap (trimmed, see §6), not a missing rollback mechanism.

## 7. Jira follow-ups filed

- **HORO-1685** — D1 (`GatewayLedgerAgreement` misses discrepancies
  under a truncated custody tree) — non-blocking: cannot trigger
  without a production custody-tree writer, which does not exist (§0).
- **HORO-1686** — D3 (`CheckOutcome::Discrepant.delta` mixes row-count
  and amount units across checks) — non-blocking: a type-level fix,
  separately reviewable.
- **HORO-1687** — I9's deeper fix (relabel session-path totals as
  `AmountScope::EnclosingAccount`, not just the selector) — would
  change a documented ADR-0013 contract; needs its own ADR note, not
  bundled into this gate's fix.
