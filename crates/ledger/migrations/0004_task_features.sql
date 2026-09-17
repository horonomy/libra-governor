-- Task-class bucketed estimation, added for HORO-1130 (activates real
-- task-class bucketing over preflight-knowable features).
--
-- `plans.task_features_json` stores the `TaskFeatures` (HORO-1130)
-- derived at preflight time, alongside `estimate_json` (0003), so a
-- later `Finalize` request -- which has no access to the original
-- prompt or reconnaissance output -- can copy it onto the finalized
-- receipt without re-deriving it. Nullable: a plan inserted before this
-- migration has no features.
--
-- `receipts.task_features_json` is the persisted copy on the finalized
-- receipt itself, read back by `receipts_for_estimation` to bucket
-- future preflights' local history. Nullable: a pre-MVP-2 receipt
-- genuinely has none -- see `libra_governor_domain::ExecutionReceipt`
-- docs. This is non-destructive: existing rows get `NULL`, not a
-- fabricated value, and `receipts_for_estimation` treats `NULL` as
-- "cannot match a bucketed tier, still counts toward the global tier".
--
-- Privacy: `TaskFeatures` is itself a structural guarantee against
-- storing raw prompt text or raw file paths -- see
-- `libra_governor_domain::task_features` module docs. This column adds
-- no new privacy surface beyond what that type already enforces.

ALTER TABLE plans ADD COLUMN task_features_json TEXT;
ALTER TABLE receipts ADD COLUMN task_features_json TEXT;
