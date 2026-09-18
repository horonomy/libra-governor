# MVP 3.0 Release Gate Evidence (HORO-1146)

This directory holds the real, committed evidence for the MVP 3.0 release gate,
following the same pattern established by `experiments/mvp1_validation/`
(HORO-1127) and `experiments/mvp2_calibration/` (HORO-1132): real subprocesses
of the real compiled `libra-governor` binary, real on-disk SQLite ledgers, real
daemon/gateway processes, no in-process mocking of the product code under test,
no fabricated numbers.

Branch: `mvp-3.0/HORO-1146/release_gate`, based on `origin/main` at merge of
HORO-1137 (Policy), HORO-1139 (replanning), HORO-1141 (Completion Reserve),
HORO-1144 (provider gateway).

## Load-bearing methodology note: why some evidence is Rust tests, not CLI runs

Before anything else: **the shipped `libra-governor` CLI binary cannot select
a non-default admission policy or start the enforcement gateway.**
`libra-governor daemon run` (`crates/cli/src/daemon_cmd.rs`) hardcodes
`policy: default_admission_policy()` (the `balanced` preset) and
`gateway: None`, with no CLI flag, no config file, and no environment
variable to override either. This is a real, confirmed gap in the product's
CLI surface (see "Defects and gaps found" below) — not a harness limitation
we worked around silently.

Consequently, scenarios that require `ConstraintMode::Hard`/`Approval`, a
non-`balanced` preset, or the gateway are **not reachable by driving the CLI
binary as a subprocess**. For those scenarios, this evidence set uses real
Rust integration tests that call the *identical* dispatch functions the real
daemon and gateway processes call at runtime —
`libra_governor_daemon::handle_connection` (the same function `daemon run`'s
accept loop calls) and `libra_governor_gateway::server::run_gateway_on` (the
same function a real gateway process would run) — against a real SQLite
ledger on disk, with only the policy/gateway *configuration* substituted in
Rust rather than selected via a CLI flag that does not exist. This is real
evidence of real code behavior; it is **not** evidence that the shipped CLI
itself exposes this configurability, and that distinction is preserved
throughout this document rather than blurred.

Every scenario below states explicitly which kind of evidence it is:

- **CLI-E2E**: a real subprocess of the compiled `libra-governor` binary,
  driven via `experiments/mvp3_gate/run_gate_matrix.py` (same pattern as
  `experiments/mvp1_validation/run_validation_matrix.py`).
- **Rust-integration**: a real `#[test]` in `crates/*/tests/*.rs`, run via
  `cargo test`, calling the real production dispatch functions with real
  on-disk state, just with policy/gateway config Rust constructs directly.
- **Rust-domain-unit**: a real `#[test]` calling one pure domain function
  directly (e.g. `should_replan`) — real code, but not exercised end-to-end
  through the daemon dispatch. Used only where the research below found the
  function genuinely is not wired into the live daemon at all (scenario 6).

## Scenario matrix and evidence

| # | Scenario | Evidence kind | Result file |
|---|---|---|---|
| 1 | Balanced policy, on-budget completion | CLI-E2E | `results/cli_scenarios_1_5_9.json` (`s1_...`) |
| 2 | Deadline-first Elastic under a tight deadline | Rust-integration (new) | `results/scenario2_deadline_first_and_scenario8_concurrent_subagents.txt` |
| 3 | Strict Hard budget ceiling denies/caps | Rust-integration (existing, reused) | `results/scenario3_strict_hard_deny.txt` |
| 4 | Approval-required flow | Rust-integration (existing, reused) | `results/scenario4_approval_required.txt` |
| 5 | Material unexpected event triggers a real replan | CLI-E2E | `results/cli_scenarios_1_5_9.json` (`s5_...`) |
| 6 | Continuing is cheaper than replanning (decline) | Rust-domain-unit (existing, reused) | `results/scenario6_decline_to_replan_cheaper.txt` |
| 7 | Optional budget exhausted, Completion Reserve protected | Rust-integration (existing, reused) | `results/scenario7_completion_reserve_protected.txt` |
| 8 | Concurrent subagents near a budget boundary | Rust-integration (new, through `LedgerSpendAuthority`) | `results/scenario2_deadline_first_and_scenario8_concurrent_subagents.txt` |
| 9 | Daemon crash mid-reservation, restart, reconcile | CLI-E2E + Rust-integration | `results/cli_scenarios_1_5_9.json` (`s9_...`) + `results/scenario9_rust_level_ledger_reconciliation.txt` |
| 10 | Provider retry / stream interruption, no double-charge | Rust-integration (existing, reused) | `results/scenario10_provider_retry_and_stream_interruption.txt` |
| 11 | BYOK hard enforcement, zero upstream requests | Rust-integration (existing, reused) | `results/scenario11_byok_hard_enforcement.txt` |
| 12 | Subscription/quota mode, honest weaker guarantees | Rust-integration (existing, reused) | `results/scenario12_subscription_mode_weaker_guarantees.txt` |

### Scenario detail

**1. Balanced policy, on-budget completion (CLI-E2E).** Real daemon
subprocess, real `hook user-prompt-submit` → 3× `hook post-tool-use` →
`hook stop`, real SQLite receipt written. See "Defects and gaps found" —
this scenario also surfaced the cold-start-Deny finding.

**2. Deadline-first under a tight deadline (Rust-integration, new test).**
`crates/daemon/tests/mvp3_gate_evidence.rs::deadline_first_confidence_floor_denies_cold_start_but_the_deadline_pressure_shape_admits_at_low_confidence`.
Two real admissions through the real daemon dispatch: the literal
`Policy::deadline_first` preset with a 120s hard deadline first (real
finding: it denies a cold-start task on the confidence floor, see below),
then the same deadline-pressure shape (`ConstraintMode::Hard` time /
`ConstraintMode::Elastic` resource) with `Confidence::Low` admits correctly
under the tight deadline.

**3/4. Strict Hard deny / Approval-required (Rust-integration, existing
tests reused).** `admission_deny_writes_no_reservation_and_leaves_the_reserve_untouched`
and `admission_approval_required_writes_no_reservation` in
`crates/daemon/tests/reservation_integration.rs` — pre-existing HORO-1141
evidence, re-run here as part of this gate rather than duplicated.

**5. Material replan trigger (CLI-E2E).** Real daemon subprocess: preflight,
then 5× `hook post-tool-use Bash` (same tool 5 times, crossing the
`PossibleToolLoop` streak threshold of 4 — `crates/domain/src/replan.rs`).
Real `replan_events` row written (`trigger: "possible_tool_loop"`, detail
`"Bash invoked 4 times in a row"`), real `statusline` reporting
`replanned 1x`.

**6. Decline to replan because continuing is cheaper (Rust-domain-unit,
existing test reused).** `should_replan_says_no_when_continuing_is_cheaper_than_replanning`
in `crates/domain/src/replan.rs`. **Disclosed limitation, not hidden**: per
the research behind this gate, `should_replan`/the cost-benefit gate is
fully implemented and fully unit-tested but is **not called anywhere in
`crates/daemon`** — `handle_tool_invoked` deliberately skips it for the
deterministic tier (see that function's own doc comment: the deterministic
tier's recompute cost is negligible and always clears the gate). So there is
no live E2E path to exercise "decline to replan" through the daemon today;
this scenario's evidence is the real, correct pure-function behavior, not an
end-to-end proof that a running daemon currently uses it on this trigger
path.

**7. Optional headroom exhausted, Completion Reserve protected
(Rust-integration, existing tests reused).**
`exhausted_headroom_refuses_even_though_the_completion_reserve_still_holds_capacity`
(`crates/gateway/tests/proxy_lifecycle.rs`),
`the_completion_reserve_is_never_drawn_by_gateway_traffic`
(`crates/daemon/tests/gateway_enforcement.rs`), and the ledger-level
`concurrent_optional_reservations_never_double_spend_the_shared_envelope`
(`crates/ledger/tests/reservation_concurrency.rs`, 8 real OS threads, 800
tokens optional headroom exactly exhausted, reserve untouched).

**8. Concurrent subagents near a budget boundary (Rust-integration, new
test).**
`crates/daemon/tests/mvp3_gate_evidence.rs::concurrent_subagents_near_a_tight_hard_ceiling_through_the_real_ledger_spend_authority`.
12 real OS threads call `LedgerSpendAuthority::authorize` — the exact
production trait implementation the real HTTP gateway server calls on every
`/v1/messages` request — concurrently, against one already-admitted task
with a 1600-token hard ceiling. **Disclosed limitation**: this drives the
real spend-authorization layer under real concurrency, but does not spin up
an actual HTTP listener; see the metrics table for the real numbers this
produced, and `results/scenario10_...`/`scenario11_...` for the
HTTP-layer-level gateway evidence that corroborates the layer this test
skips.

**9. Daemon crash mid-reservation, restart, reconciliation (CLI-E2E +
Rust-integration).** CLI-E2E half: real `daemon run` subprocess, real
preflight (denied on cold-start confidence, see finding below), real
material-event replan (5× same-tool `post-tool-use`, same mechanism as
scenario 5) which reserves capacity for the task despite the Deny (**a
second real finding**, see below), real `SIGKILL`, and a **disclosed,
non-silent time simulation**: the reservation's TTL is a real 900s
(`DEFAULT_RESERVATION_TTL_SECS`, hardcoded, no CLI override), which this
harness cannot honestly wait out, so the already-real, already-committed
reservation row's own `expires_at` column is rewritten to a past timestamp
via direct SQL — simulating elapsed wall-clock time on a real row, not
fabricating the reservation itself. A fresh `hook user-prompt-submit`
respawns the daemon (real), which runs `reconcile_stale_reservations` at
startup (real), and the reservation is really reclaimed to `state=expired`
with `completion_reserve` really restored. Rust-integration half:
`crates/ledger/tests/reservation_concurrency.rs::a_reservation_left_active_by_a_crashed_process_is_reclaimed_on_reconciliation`
corroborates the same mechanism at the ledger-crate level without the SQL
time-jump.

**10. Provider retry / stream interruption (Rust-integration, existing
tests reused).**
`a_stream_cut_off_mid_flight_settles_at_the_last_observed_usage`,
`a_client_retry_is_a_fresh_reservation_not_a_double_settlement`,
`an_upstream_401_triggers_exactly_one_credential_refresh_retry`
(`crates/gateway/tests/proxy_lifecycle.rs`), and
`settling_the_same_reservation_twice_does_not_double_charge`
(`crates/daemon/tests/gateway_enforcement.rs`) — a real gateway HTTP server
(`libra_governor_gateway::server::run_gateway_on`) against a real fake
upstream (`crates/gateway/tests/fake_upstream.rs`) that actually cuts an SSE
stream mid-flight.

**11. BYOK hard enforcement, zero upstream requests (Rust-integration,
existing tests reused).**
`a_request_with_no_credential_is_refused_and_never_reaches_the_upstream`,
`a_wrong_capability_token_is_refused`,
`disagreeing_dual_auth_headers_are_refused_without_any_upstream_call`
(`crates/gateway/tests/auth_and_routing.rs`) — each asserts
`gw.upstream.received().is_empty()` against the real fake upstream server, a
real running gateway, real HTTP requests.

**12. Subscription/quota mode, honest weaker guarantees (Rust-integration,
existing tests reused).**
`pass_through_subscription_mode_forwards_the_callers_own_credential_unchanged`
(`crates/gateway/tests/auth_and_routing.rs`) proves a real request in
`GatewayCredentialMode::PassThroughSubscription` really forwards the
caller's own credential unchanged (no Governor custody). Domain-level
`subscription_tier_names_the_concrete_reason_it_cannot_price`
(`crates/domain/src/capability.rs`) proves
`EnforcementCapabilities::for_tier(GatewayObservedQuota)` really reports
`MonetaryEnforcement::NotAvailable { reason: NoMonetaryCap::... }` — not
silently promoted to `Enforced`.

## Defects and gaps found (not suppressed)

This gate surfaced three real, load-bearing findings during evidence
generation, discovered incidentally while running scenarios 1, 2, and 9 —
not sought out separately:

1. **The default `balanced` admission policy denies every cold-start task.**
   `Policy::balanced` (and `deadline_first`, and `strict_budget`) all
   require `min_confidence: Confidence::Medium`, but a cold-start estimate
   (no local receipt history — the honest state of a brand-new install, or
   any never-before-seen repo/prompt combination) is always
   `Confidence::Low`. Confirmed via `daemon.log` in scenario 1 and 9's real
   runs: `admission decision Deny([ConfidenceBelowThreshold { actual: Low,
   required: Medium }]) — no reservation made`. Work still proceeds — hooks
   are advisory-only by design (ADR 0001) — but every first-ever preflight
   under the real, shipped default policy reports a real Deny in its
   advisory text, which will read as "broken" to a new user before any
   local history exists.
2. **A material-event replan reserves capacity even for a task whose
   original admission was Denied.** Confirmed in scenario 5 and 9: the
   initial preflight was `Deny`ed (finding #1), yet the subsequent
   `PossibleToolLoop` replan still wrote a real, active
   `ReservationClass::RequiredWork` reservation for 70,000 tokens for that
   same (denied) task. `handle_tool_invoked`'s replan path does not appear
   to re-check the task's original admission outcome before reserving.
   Real behavior, real risk surface — reported here for the coordinator's
   GO/ITERATE/PIVOT/KILL judgment, not assessed as a severity level by this
   harness.
3. **No CLI/config surface for policy selection or the gateway** (see
   "Load-bearing methodology note" above) — the shipped binary can only
   ever run the hardcoded `balanced` policy with no gateway. This is why
   8 of 12 required scenarios needed Rust-integration evidence instead of
   CLI-E2E evidence.

## What is honestly NOT production-equivalent

- Scenario 8's concurrency test drives the real `LedgerSpendAuthority`
  layer, not an actual HTTP listener — see its scenario detail above.
- Scenario 9's TTL expiry is real-row-real-mechanism but time-jumped via a
  direct SQL edit rather than a genuine 900-second wait, disclosed inline
  both in the harness code and here.
- No real end-user traffic exists yet (pre-launch product) — every scenario
  here is a local, single-machine approximation of concurrent/production
  load, not a claim of production-scale evidence. This mirrors
  `experiments/mvp2_calibration/README.md`'s same honest disclosure for
  calibration data.
- Estimator calibration status is real and current (see metrics table) and
  is expected to still say "insufficient data" pre-launch — this is the
  correct, honest result per HORO-1132's design decision, not a gap in this
  gate's evidence.

## Reproduce

```bash
export CARGO_TARGET_DIR=/tmp/libra-horo1146-target-$$
cargo build --workspace

# CLI-E2E scenarios (1, 5, 9's CLI half)
python3 experiments/mvp3_gate/run_gate_matrix.py \
  --binary "$CARGO_TARGET_DIR/debug/libra-governor" \
  --work-root /tmp/mvp3-gate-work \
  --out experiments/mvp3_gate/results/cli_scenarios_1_5_9.json

# Rust-integration / Rust-domain-unit scenarios (2,3,4,6,7,8,9-rust,10,11,12)
cargo test -p libra-governor-daemon --test mvp3_gate_evidence -- --nocapture
cargo test -p libra-governor-daemon --test reservation_integration -- --nocapture
cargo test -p libra-governor-daemon --test gateway_enforcement -- --nocapture
cargo test -p libra-governor-ledger --test reservation_concurrency -- --nocapture
cargo test -p libra-governor-gateway --test proxy_lifecycle -- --nocapture
cargo test -p libra-governor-gateway --test auth_and_routing -- --nocapture
cargo test -p libra-governor-gateway --test ssrf_and_config -- --nocapture
cargo test -p libra-governor-domain --lib -- --nocapture

# Full CI-equivalent gate
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

See `results/metrics_summary.md` and `results/security_review.md` for the
full metrics table and security findings, each traceable to a specific file
in `results/`.
