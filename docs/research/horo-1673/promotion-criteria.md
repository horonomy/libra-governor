# HORO-1673 — Pre-registered shadow-promotion criteria

Authored: 2026-10-03, as the first commit of HORO-1673, **before** any
dogfood row for this ticket is collected. Verified by repo-wide search
(`grep -rni "pre-regist|preregist|promotion crit|promote.*shadow|shadow.*promot"`
across every `.md`/`.rs` file, zero hits) that no prior pre-registration
document existed anywhere in this repository. These criteria are
authored now, by this gate, for this gate — not reconstructed after
seeing results. ADR-0011 already named this ticket as the place this
review happens: "Promotion to enforcement is HORO-1673's own reviewable
work, gated on the evidence this ticket starts collecting."

**No threshold below may be lowered after data collection begins.** If
the real data collected later in this ticket fails a threshold, the
correct response is to report the failure and keep `Shadow` behavior —
never to revise this document to fit the data.

## 1. Minimum qualifying sample counts (per arm, distinct floors)

| Arm | Floor | Source |
|---|---|---|
| Calibration pairs (`calibration_pairs()`, receipts × plans) | 30 | Reuses `REQUIRED_CALIBRATION_PAIRS` (`crates/estimator/src/calibration.rs`) — the existing campaign-wide floor, not a new number |
| Shadow decision points, aggregate across all trajectories | 30 | Set above `MIN_REPLAY_SAMPLES = 5` (`crates/domain/src/replay.rs`), which is the floor for a *single* trajectory's regret calculation, not for a promotion decision |
| Replay-eligible decision points (non-NULL `policy_json`/`pins_json`, post-migration-0014) | 30 | Same reasoning as above |
| Classifiable Stop/Degrade proposals (for false-stop/false-degrade rate) | 20 | Below the 30-pair floor deliberately — these are rarer events; 20 is the minimum denominator below which an N-of-M rate is not reportable at all per `docs/research/horo-1154/minimum-evidence-threshold.md`'s raw-denominator discipline |
| Observations before any early-warning lead-time figure is reported | 10 | A single lucky/unlucky observation must not produce a reported lead time |

## 2. Qualifying-pair predicate

A `(Estimate, ExecutionReceipt)` pair counts toward the 30-pair
calibration floor only if **all** of:

```
actual_duration_secs >= 120
AND tool_calls >= 5
AND task_features.is_some()
```

Pairs produced by mechanically driving hook cycles with no real human
work behind them (near-zero duration, one tool call) do not qualify.
Report `N_total` (all receipts) and `N_qualifying` (passing this
predicate) separately, always. A calibration figure (MAE, coverage) may
only be computed over `N_qualifying`, never `N_total`.

## 3. False-stop / false-degrade tolerance

- **False-stop rate**: a `ProposedAction::Stop` shadow proposal is
  "false" if the task's actual receipt shows the task completed within
  its budget. Tolerable rate for promotion: **≤ 10%**, denominator floor
  per item 1 (20 classifiable proposals minimum).
- **False-degrade rate**: analogous definition for
  `ProposedAction::Degrade`. Tolerable rate for promotion: **≤ 15%**
  (degrade is a softer action than stop; a higher false-positive
  tolerance is intentional, not sloppiness — state this if cited later).
- Both rates must be reported as `k of n`, never as a bare percentage.

## 4. Early-warning lead time

- Minimum required lead time for promotion: **≥ 60 seconds** of real
  wall-clock warning before a task would have become budget-infeasible,
  computed as `receipt.recorded_at − first_refusal.decided_at`
  (`FirstRefusal`, `crates/domain/src/replay.rs`).
- A lead-time figure may only be reported once the 10-observation floor
  (item 1) is met. Below that floor, report "insufficient observations
  to compute lead time," not a number from fewer samples.

## 5. Candidate-vs-baseline improvement (calibration)

The progressive estimator (`remaining_bucketed`) must beat **both**
named baselines, not just one, at the 30-pair floor:

- **P90 empirical coverage**: candidate coverage must be ≥ both
  baseline-0 (raw unconditioned quantiles) and baseline-1
  (`RemainingEstimate::from_bucketed`, the existing deterministic-
  widening baseline) coverage, at every elapsed fraction measured
  (`[0.25, 0.5, 0.75]`).
- **P50 mean absolute error**: candidate MAE must be at least **10%
  lower** (relative reduction) than both baseline-0 and baseline-1 MAE,
  at every elapsed fraction measured.
- If the candidate beats one baseline but not the other, that is a
  **mixed result** and must be reported as such — not averaged or
  explained away. A mixed result does not meet this criterion.

## 6. Decision regret improvement

`AggregateRegret` (`crates/domain/src/replay.rs`) over comparable
decision points (`comparable_regret`) must show the production policy's
mean regret ≤ the best deterministic-widening-baseline alternative's
mean regret, at a sample floor of **30 comparable decision points**
(same floor as item 1 — do not evaluate regret at the 5-point
single-trajectory floor).

## 7. Shadow stays shadow by default

If **any** criterion above is not met — including simply not reaching
the sample floor — the correct and expected outcome is: **STOP/DEGRADE
proposals remain `Shadow`**. This is not a failure of the gate; it is
this document working as intended. The gate records the exact evidence
deficit per arm (which floor was not reached, by how much) rather than
a vague "not enough data."

## 8. DROP criteria for the progressive estimator itself

Promotion-to-enforcement and DROP are different questions. DROP the
progressive estimator (`remaining_bucketed`) entirely — not merely
decline to promote STOP/DEGRADE — if, across **3 or more independent
gate runs** (not 3 pairs within one run) each reaching the 30-pair
floor:

- it fails to beat baseline-1 on **both** P50 MAE and P90 coverage
  (i.e. the existing named deterministic-widening baseline is at least
  as good), **or**
- the false-stop rate exceeds its tolerance (item 3) with no improving
  trend across those runs.

A single gate run — this one — can at most report evidence toward this
determination. It cannot by itself trigger a DROP verdict under this
criterion; that requires the 3-run history stated above. This ticket's
own run should state plainly which side of that line it falls on and
why.

## Provenance

These criteria were authored by Claude Code (Sonnet 5) as part of
HORO-1673, in a dedicated commit preceding all other HORO-1673 work,
per the ticket's own requirement that shadow promotion not be evaluated
against criteria invented after the fact. See the HORO-1673 PR for the
commit hash of this file's introduction.
