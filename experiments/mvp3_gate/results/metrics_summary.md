# MVP 3.0 Release Gate — Metrics Summary

Every number below is traceable to a specific file in this directory. See
`../README.md` for the scenario-to-evidence map and the "not
production-equivalent" disclosures this table depends on.

## Task completion / admission metrics

| Metric | Value | Source |
|---|---|---|
| Task completion rate (CLI-E2E scenarios run) | 3/3 (100%) — scenarios 1, 5, 9 all completed their `hook stop` finalize step and produced a receipt | `cli_scenarios_1_5_9.json` |
| On-budget / on-time completion | N/A this run — every task here was cold-start (no P90 baseline exists yet to compare "actual" against); `hook stop` output literally reports `Inside P90: n/a (cold start)` | `cli_scenarios_1_5_9.json` (`s1_...` detail field) |
| Unfinished spend (reservations left un-settled at task end) | 0 — every reservation created during these scenarios was either settled, released, or (scenario 9) correctly expired and reclaimed by reconciliation; no orphaned active reservation remains in any scenario's ledger | `cli_scenarios_1_5_9.json`, `scenario9_rust_level_ledger_reconciliation.txt` |
| Admission Admit rate on cold-start tasks under the shipped default (`balanced`) policy | **FIXED — now 2/2 (100%)**. Originally **0/2 (0%)**: both scenario 1 and scenario 9's initial preflights were real `Deny`s on the confidence floor alone (defect #1, see README "Defects and gaps found" #1). `Policy::balanced`'s `min_confidence` was lowered from `Medium` to `Low` (`crates/domain/src/policy.rs`); re-running `run_gate_matrix.py` after the fix shows both preflights now `Admit`. Regression test: `policy::tests::balanced_admits_a_cold_start_low_confidence_estimate_on_confidence_alone`. | `cli_scenarios_1_5_9.json` (re-run post-fix); `defect1_balanced_admits_cold_start.txt` |
| Admission Admit rate, deadline-first shape at achievable confidence | 1/1 (100%) once `Confidence::Low` floor used instead of the preset's `Medium` | `scenario2_deadline_first_and_scenario8_concurrent_subagents.txt` |
| Admission Deny rate, `strict_budget` cold start | 1/1 (100%) — deterministic, by design | `scenario3_strict_hard_deny.txt` |
| Admission ApprovalRequired correctly reachable | 1/1 (100%) | `scenario4_approval_required.txt` |

## Replan metrics

| Metric | Value | Source |
|---|---|---|
| Number of automatic replans triggered (real, CLI-E2E) | 1 (`PossibleToolLoop`, "Bash invoked 4 times in a row") | `cli_scenarios_1_5_9.json` (`s5_...`), `scenario9_rust_level_ledger_reconciliation` context |
| Replan visible via statusline | Yes — `libra: task 90297147 \| plan c692ecfe \| preflight: low \| recon: 0.0s \| remaining P90: unknown \| replanned 1x` | `cli_scenarios_1_5_9.json` |
| Admission/replan overhead, wall-clock | Preflight + 5 real `hook post-tool-use` calls + `statusline` completed in well under 1 second end-to-end (hook subprocess spawns dominate; no artificial delay observed) — exact per-call timing not separately instrumented in this run | `cli_scenarios_1_5_9.json` |
| Admission/replan overhead, tokens/cost | N/A — no gateway/provider spend occurred in the CLI-E2E scenarios (no real LLM calls made; hooks are local-only) | n/a by construction |
| Unnecessary replans / interruptions correctly declined (cost/benefit gate) | 1/1 real pure-function case confirmed correct (`net_gain < min_gain` → no replan) — **not exercised end-to-end through the daemon**, see README finding: the gate is not currently wired into `handle_tool_invoked` | `scenario6_decline_to_replan_cheaper.txt` |
| Replan reserving capacity despite the originating admission being Denied | **Observed in 2/2 real replan-triggering runs** (scenarios 5 and 9) — see README "Defects and gaps found" #2 | `cli_scenarios_1_5_9.json` (`s9_...` detail: reservation of 70,000 tokens created for a task whose preflight was Denied) |

## Budget / reservation integrity metrics

| Metric | Value | Source |
|---|---|---|
| Hard-budget violation/overshoot rate (exact-enforcement cases) | **0/1** — the one real hard-ceiling test (`strict_budget` Deny) correctly wrote **zero** reservations; no overshoot possible because nothing was ever reserved | `scenario3_strict_hard_deny.txt` |
| Hard-budget refusal at the real ledger (gateway path) | 0 violations across `a_request_beyond_the_hard_budget_is_refused_by_the_real_ledger` and `exhausted_headroom_refuses_even_though_the_completion_reserve_still_holds_capacity` | `scenario7_completion_reserve_protected.txt` |
| Completion Reserve survival — ever drawn into by optional work | **0/0 — never**, across every scenario that exercised it: `concurrent_optional_reservations_never_double_spend_the_shared_envelope` (8 real threads, 800 tokens optional headroom exactly exhausted, reserve = initial reserve throughout), `the_completion_reserve_is_never_drawn_by_gateway_traffic`, and the concurrent-subagents test (12 threads, `completion_reserve == initial_completion_reserve` asserted and held after real contention) | `scenario7_completion_reserve_protected.txt`, `scenario2_deadline_first_and_scenario8_concurrent_subagents.txt` |
| Concurrent subagents near a budget boundary — outcome split | N=12 real OS threads, 200 tokens each, against a 1600-token hard ceiling with a 1000-token already-committed plan reservation → **granted=3, denied=9**, `Σ active == granted × 200` exactly (no double-spend), reserve untouched | `scenario2_deadline_first_and_scenario8_concurrent_subagents.txt` |
| Optional-headroom exhaustion, ledger-level | 8 threads × 200 tokens vs. 800-token headroom → exactly 4 granted, 4 `Insufficient` (pre-existing HORO-1141 evidence, re-run here) | `scenario7_completion_reserve_protected.txt` |
| Daemon crash mid-reservation → restart → reconciliation | 1/1 real reservation reclaimed correctly (`state: active → expired`), `completion_reserve` restored to `initial_completion_reserve` exactly | `cli_scenarios_1_5_9.json` (`s9_...`) |
| Double-settlement prevention | 0 double-charges across `settling_the_same_reservation_twice_does_not_double_charge` and `a_client_retry_is_a_fresh_reservation_not_a_double_settlement` | `scenario10_provider_retry_and_stream_interruption.txt` |
| Stream-interruption settlement correctness | Settles at last-observed usage, never double-charges, confirmed by real fake-upstream stream truncation against a real running gateway | `scenario10_provider_retry_and_stream_interruption.txt` |
| BYOK hard enforcement — upstream requests on a refused call | 0/0 — every refused request (`a_request_with_no_credential_is_refused_and_never_reaches_the_upstream`, `a_wrong_capability_token_is_refused`, `disagreeing_dual_auth_headers_are_refused_without_any_upstream_call`) confirmed `gw.upstream.received().is_empty()` | `scenario11_byok_hard_enforcement.txt` |
| Subscription mode enforcement claim | `MonetaryEnforcement::NotAvailable` reported (never silently promoted to `Enforced`); caller's own credential forwarded unchanged (no Governor custody) | `scenario12_subscription_mode_weaker_guarantees.txt` |

## Estimator calibration status

Real, current run (`libra-governor calibration report`) against this gate's
own accumulated local ledger state:

```
insufficient data: n=0, need >= 30 non-cold-start calibration pairs. This is
the honest, expected result for a pre-launch product with no real trajectory
history yet.
```

Source: `calibration_report.txt`. This is the honest, expected result per
HORO-1132's design decision — see `experiments/mvp2_calibration/README.md`
for the same finding at MVP 2.0's gate. `n=0` here (vs. `n=2` at MVP 2.0's
gate) because this run's cold-start preflights were correctly excluded
("Dropped 1 locally recorded receipt(s): no usable estimate") — this run did
not happen to produce a non-cold-start receipt pair; it does not indicate a
regression, since the floor (`>= 30` pairs) was never remotely close to met
at either gate.

## Full CI-equivalent gate

| Check | Result | Source |
|---|---|---|
| `cargo fmt --all -- --check` | PASS (exit 0) | `fmt_check.txt` |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS (exit 0, 0 warnings) | `clippy_check.txt` |
| `cargo build --workspace` | PASS (exit 0) | `build_workspace.txt` |
| `cargo test --workspace` | **PASS — 411 tests passed, 0 failed, across every crate and every test binary in the workspace** (`libra-governor-cli`, `-daemon`, `-domain`, `-estimator`, `-gateway`, `-ledger`, `-protocol` — lib and integration tests) | `full_workspace_test_run.txt` |

Per-binary breakdown (all green): cli-lib 28, `hook_cli_integration` 8,
daemon-lib 31, `daemon_unreachable` 2, `gateway_enforcement` 9,
`mvp3_gate_evidence` 3 (new), `preflight_integration` 3, `replan_integration`
3, `reservation_integration` 6, domain-lib 96, estimator-lib 32, gateway-lib
68, `auth_and_routing` 19, `fake_upstream` 4, `proxy_lifecycle` 27,
`ssrf_and_config` 13, ledger-lib 50, `reservation_concurrency` 3,
`trajectory` 6. Total: 411.

**Note on evidence-gathering methodology**: the full-workspace test run was
executed via several staggered `cargo test --workspace` invocations due to a
known machine-level issue (shared `CARGO_TARGET_DIR` cross-worktree
contention causing slow first-launch validation per target — documented in
`/Users/bryant/CLAUDE.md`'s "Incident: Disk Pressure from Per-Worktree Rust
Build Artifacts"), not because of any test flakiness: every test binary that
completed in any invocation reported the identical pass count and zero
failures on every retry.
