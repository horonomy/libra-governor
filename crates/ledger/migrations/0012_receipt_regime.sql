-- Execution-regime provenance on finalized receipts, added for HORO-1671
-- (regime-aware calibration confidence and drift detection).
--
-- `receipts.regime_json` stores the `RegimeProvenance` (model/harness/
-- pricing/enforcement-tier/schema identity, plus reported-only topology/
-- cache-class) computed once at finalize time from the task's own
-- gateway requests and whatever the harness reported. Nullable: a
-- pre-HORO-1671 receipt genuinely has none -- see
-- `libra_governor_domain::ExecutionReceipt` docs. This is
-- non-destructive: existing rows get `NULL`, not a fabricated regime,
-- and the estimator treats `NULL` as "comparable to everything" (the
-- positive-evidence rule), never as a silent regime match or mismatch.
--
-- Privacy: `RegimeProvenance` is itself a structural guarantee against
-- raw prompt/path content -- every `DimensionValue::Known` token is a
-- short, closed-vocabulary identifier (a model name, a pricing version,
-- a schema tag). This column adds no new privacy surface beyond what
-- that type already enforces.

ALTER TABLE receipts ADD COLUMN regime_json TEXT;
