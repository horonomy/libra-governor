# v0.0.3 Counterfactual Policy Replay (HORO-1670)

For every task with recorded `shadow_runtime_decisions` rows, replays
each of the four named policy presets (`balanced`, `deadline_first`,
`cost_first`, `strict_budget`) against that task's own recorded decision
points and reports, per candidate: how often it would have agreed with
the actually-recorded policy, how often it would have been more/less
restrictive, and how many trajectories it would have violated the
recorded quality floor on (reported separately — never folded into a
disagreement count; see `PolicyComparison::QualityFloorViolated` in
`libra_governor_domain::replay`).

## Why this is a Rust example, not a Python subprocess script

Same rationale as `v003_gate` (HORO-1669): this is an offline backtest
over in-process domain types (`libra_governor_domain::replay`,
`libra_governor_ledger::RecordedShadowDecision`), not reachable from
outside the Rust workspace without either a new CLI subcommand (out of
scope — this ticket adds no protocol surface) or reimplementing replay
logic against a JSON dump.

## Running it

```bash
cargo run -p libra-governor-daemon --example v003_replay -- /path/to/ledger.sqlite3 [--task <uuid>]
```

## Leakage-free by construction

Replay reads each decision point's recorded `RemainingWorkEstimate`
verbatim — this binary never calls `remaining_bucketed`, never reads
receipt history, and never reconstructs a decision point from anything
but its own `shadow_runtime_decisions` row. See
`libra_governor_domain::replay`'s module docs for the full rationale.

## Version pinning

A row recorded before migration `0014` (practically, every row recorded
before this ticket's daemon wiring landed) has no `policy_json`/
`pins_json` and is skipped outright (`ReplayEligibility::Unpinned`) —
never guessed. A fresh checkout or CI run has zero eligible rows; the
harness reports `INSUFFICIENT` in that case, which is the honest, correct
result, not a harness failure. See `results/` for the most recent real
run's output.
