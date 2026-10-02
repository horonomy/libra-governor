# v0.0.3 Progressive-Estimator Benchmark Gate (HORO-1669 Stage C)

Compares three remaining-duration predictors against every real, locally
recorded `(Estimate, ExecutionReceipt)` pair:

- **baseline-0**: the plan's own unconditioned total quantiles, taken
  as-is — the naive "ignore elapsed time" predictor.
- **baseline-1**: [`RemainingEstimate::from_bucketed`], the existing
  named deterministic-widening baseline (untouched by HORO-1669).
- **candidate**: `remaining_bucketed`, HORO-1669's conditional-quantile
  progressive estimator — genuinely conditioned on elapsed time via
  leave-one-out history.

## Why this is a Rust example, not a Python subprocess script

Unlike `v001_gate`/`v002_gate` (real end-to-end dogfood scenarios driving
the compiled binary as a subprocess), this is an offline statistical
backtest over in-process estimator/ledger types
(`libra_governor_estimator::remaining_bucketed`,
`libra_governor_domain::RemainingEstimate`, `libra_governor_ledger::CalibrationPair`).
None of those are reachable from outside the Rust workspace without
either a new CLI subcommand (out of scope — this ticket adds no protocol
surface) or duplicating comparison logic in Python against JSON dumps.
`crates/daemon/examples/v003_gate.rs` runs directly against a real
ledger file, reusing the production types with no reimplementation risk.

## Running it

```bash
cargo run -p libra-governor-daemon --example v003_gate -- /path/to/ledger.sqlite3
```

Gated on the same `REQUIRED_CALIBRATION_PAIRS = 30` floor as every other
calibration report in this repo. A fresh checkout or CI run has zero real
calibration pairs — the harness reports `INSUFFICIENT`, which is the
honest, correct result, not a harness failure. See `results/` for the
most recent real run's output once one exists.

## Metrics

Per predictor, at elapsed fractions `[0.25, 0.5, 0.75]` of each pair's
real total duration: mean absolute error of the P50 prediction against
the real remaining duration, and P90 empirical coverage (the fraction of
cases where the real remaining duration fell at or under the predicted
P90 — the same notion `duration_coverage` already reports for the
un-conditioned case, applied here per elapsed fraction instead of once
per pair).
