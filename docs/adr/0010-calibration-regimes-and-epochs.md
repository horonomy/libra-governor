# ADR 0010: Calibration regimes and epochs (HORO-1671)

## Status

Accepted. v0.0.3 "Execution Economic Truth" campaign, Phase D.

## Context

Libra's estimator (HORO-1130/1132) trusts its own history by bucket tier
and raw sample count. It has no concept that the *execution regime* a
sample was produced under — model, harness, pricing version, enforcement
tier, estimator/feature schema — might have changed. A model swap, a
pricing-table rotation, or an estimator logic change can silently inherit
stale High confidence from a regime that no longer applies. This ticket
closes that gap: historical success must not manufacture false confidence
once the regime it was earned under has moved on.

## Decision

### `RegimeKey` is a separate type from `EconomicAttribution`

`EconomicAttribution`/`UnknownReason` (HORO-1666) answer *who owns an
economic fact* and are pinned at `ECONOMIC_ATTRIBUTION_CONTRACT_VERSION =
1`. `RegimeKey` answers a different question — *was this sample produced
under comparable conditions to the current one* — and needs its own
`MixedWithinTask` cause with no analogue there. Widening `UnknownReason`
to carry it would widen that pinned contract's wire vocabulary for an
unrelated concern. The two types are deliberately not unified.

### The positive-evidence rule

`RegimeKey::comparison` treats a dimension as evidence of regime change
**only** when both sides are `DimensionValue::Known` and differ.
`DimensionValue::Unavailable` on either side — for *any* reason, including
`MixedWithinTask` — is absence of evidence, never evidence of absence.

This is the load-bearing rule. Today, `model`, `harness`,
`harness_version`, and `effort` are all genuinely latent:
`derive_task_features` never receives a model at preflight time, and
`ExecutionReceipt::provider` is always `None` at finalize (no hook
payload exposes it). Without the positive-evidence rule, every comparison
against an `Unavailable` dimension would read as a mismatch, and every
estimate would collapse to `Confidence::Low` the moment this ticket
shipped — exactly the false-drift failure mode the ticket exists to
prevent. The regression test
`todays_real_data_shape_produces_identical_confidence_to_pre_1671` in
`crates/estimator/src/regime.rs` pins this: with every dimension
`Unavailable` (today's real shape), confidence is byte-identical to what
`Confidence::from_evidence(tier, n)` already produced pre-HORO-1671.

### Comparability vs. bucketing vs. reported-only

Three distinct roles, easy to conflate:

- **Comparability** (`RegimeKey`): what the execution *runs with* —
  model, harness, pricing, enforcement tier, schema versions. Two samples
  that differ here are not safely poolable for confidence.
- **Bucketing** (`TaskFeatures`, unchanged by this ticket): what the task
  *is* — repo, topology. Already has its own hierarchical backoff ladder.
  Comparing on it here too would double-penalize the same fact.
- **Reported-only** (`RegimeProvenance`, not `RegimeKey`): `topology` (the
  bucket ladder's own dimension) and `cache_class` (a per-task
  session-shape property — a long conversation vs. a fresh one — not a
  property of the execution regime; comparing on it would cry drift
  between two consecutive tasks run under an identical model/pricing/tier).

### Comparability is not transitive; cohort identity is exact `Eq`

A (`model: Unknown`) can be comparable to both B (`model: X`) and C
(`model: Y`) while B and C are not comparable to each other. Cohort
*identity* therefore uses exact `Eq` on `Option<RegimeKey>` — reflexive
and transitive, a deterministic partition. Comparability is used only to
decide whether a cohort's evidence counts toward the *active* regime's
confidence. The two are never conflated in the implementation.

### Epochs are derived, never stored

A `CalibrationEpoch` is a *regime cohort*: every `CalibrationPair` grouped
by exact `Option<RegimeKey>` equality, computed fresh from
`LedgerStore::calibration_pairs`'s rows every time a report is built.
There is no epoch table, no write path, and therefore nothing to prune.
"Historical data is not deleted" is true by construction. Ordered by
`last_recorded_at` descending (ties broken by `Ord` on the key) — cohorts,
not maximal temporal runs: a user flipping between two regimes produces
exactly two cohorts with interleaved time ranges, not a churn of
micro-epochs.

### Two distinct sample counts

- `active_epoch.n` — only the cohort whose key is *exactly equal* to the
  current regime.
- `in_regime_n` — the sum over every cohort *comparable* to the current
  regime (via `RegimeKey::comparison`), including the pre-regime
  (`None`-keyed) cohort and any cohort carrying an `Unavailable`
  dimension. This is what feeds `Confidence::from_evidence` — unchanged,
  only its `n` argument is now the in-regime count rather than the raw
  bucket sample count.

These are not the same number and must never be conflated.

### Weighting: two constants, no decay function

Out-of-regime pairs still contribute to quantile *bounds*
(`OUT_OF_REGIME_BOUNDS_WEIGHT = 1.0` — a bound exists at all rather than
cold-starting on every regime change) but never to *confidence*
(`OUT_OF_REGIME_CONFIDENCE_WEIGHT = 0.0`). This is a literal, testable
answer to "whether old data contributes and at what weight" with nothing
to mis-calibrate — no half-life, no exponential decay.

### Drift is a separate, reported-only signal

`DriftVerdict` answers a different question from regime identity: did
the outcome distribution move even though no regime dimension changed?
One statistic — P90 exceedance rate over the most recent `DRIFT_WINDOW =
20` *usable* pairs (nominal 10%; `>= 30%` reported as `Drifting`) — reusing
`quantile_coverage_for`'s own P90-bound extraction, filtered to pairs
carrying a P90 bound exactly as `admission_replay` already does. This
does **not** feed back into live confidence in this ticket — a deliberate
boundary for a later ticket (HORO-1669 is the natural next consumer).

### Estimator version vs. estimator regime schema

`ESTIMATOR_VERSION` bumped `v3-tiered-confidence -> v4-regime-aware` for
traceability. `ESTIMATOR_REGIME_SCHEMA` (`"esr-v1"`) is a **separate**,
deliberately slower-moving constant — bump it only when the
quantile/bucketing logic that shapes the output *distribution* changes,
never for a confidence-labelling change. Conflating the two would make
this ticket's own version bump read as a regime change and
self-invalidate every pre-upgrade calibration sample on the very release
that introduces regime awareness.

### Daemon-observed regime facts today

`pricing_version`/`enforcement_tier` are real facts of the daemon's own
gateway configuration (`DaemonConfig::gateway`) when one is configured,
`Unavailable { NoGatewayConfigured }` otherwise — never the bare
build-time pricing constant asserted against nothing. `model`, `harness`,
`harness_version`, `effort` are `Unavailable` today for every agent (no
hook payload exposes them at preflight; `ExecutionReceipt::provider` is
always `None` at finalize) — typed, not faked, and will light up
naturally once a future ticket wires richer harness identity through
`Request::Finalize` (out of scope here — see HORO-1599/1667's own notes
on this same latency).

Per-task cache-class/topology facts from aggregating a task's own
`gateway_requests` rows are deferred — this ticket reports
`CacheClass::NoCacheObserved` rather than guessing, since building that
aggregate query is incremental follow-up work, not a correctness
requirement for the regime mechanism itself.

## Consequences

- Confidence can genuinely drop on a real regime change (model swap,
  pricing rollover) rather than silently inheriting stale evidence.
- Old epochs remain fully visible in the report, with their sample counts
  and quantile coverage intact — nothing is deleted or hidden.
- Drift detection is informational only in this ticket; a future ticket
  decides whether/how it feeds into a shadow STOP/DEGRADE decision
  (HORO-1669).
- Richer harness/model identity on the live path remains a separate,
  smaller follow-up — this ticket does not widen the protocol to fix it.
