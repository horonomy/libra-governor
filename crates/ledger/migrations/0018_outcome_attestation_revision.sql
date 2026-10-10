-- Binds each outcome attestation to the contract revision its plan was
-- drafted against, and makes `outcome_attestations` genuinely
-- append-only (ADR-0017, HORO-1727 Decision 2).
--
-- `contract_revision` is NULL for:
--
--   * Every row written before this migration (legacy rows -- never
--     backfilled, since the plan that would answer "which revision was
--     this about" may itself have been superseded by the time this
--     migration runs; a legacy row is never treated as bound to any
--     specific revision, only as an unbound signal `grant_renewal`'s
--     `UnboundLegacyCompletion` refusal reason reacts to).
--   * Any push with `plan_id: None` -- `RecordOutcome`'s CLI default.
--     Deliberately recorded unbound rather than resolved to "whichever
--     plan is currently in flight": that would silently rebuild the
--     exact "most recent wins" bug this migration exists to remove,
--     just moved from read-time (`promote_receipt_outcome`, deleted by
--     this same change) to write-time.
--
-- A row with `plan_id: Some(p)` resolves `contract_revision` from
-- `plans.contract_revision WHERE id = p AND task_id = ?` inside the same
-- transaction that inserts the row -- see
-- `LedgerStore::record_outcome_attestation`, which replaces
-- `insert_outcome_attestation` + `promote_receipt_outcome`'s two-separate-
-- statements shape with one `BEGIN IMMEDIATE` transaction that also
-- resolves the plan, detects a conflicting authoritative outcome for
-- that revision, and promotes only that revision's own receipts.
ALTER TABLE outcome_attestations ADD COLUMN contract_revision INTEGER;

-- Serves `record_outcome_attestation`'s own per-revision authoritative-
-- outcome resolution query (every attestation for one task+revision,
-- filtered to authoritative=1) and `grant_renewal`'s revision-scoped
-- `TaskAlreadyCompleted`/`ConflictingCompletionOutcomes` gates.
CREATE INDEX IF NOT EXISTS idx_outcome_attestations_task_revision
    ON outcome_attestations (task_id, contract_revision, authoritative);

-- Genuinely append-only, not append-only by convention: mirrors
-- migration 0017's `task_budget_renewals` trigger pattern exactly. A
-- correction to a recorded attestation is a new row (a future
-- revocation/correction path, not built here), never a mutation of an
-- existing one.
CREATE TRIGGER IF NOT EXISTS trg_outcome_attestations_no_update
BEFORE UPDATE ON outcome_attestations
BEGIN
    SELECT RAISE(ABORT, 'outcome_attestations is append-only: UPDATE is forbidden');
END;

CREATE TRIGGER IF NOT EXISTS trg_outcome_attestations_no_delete
BEFORE DELETE ON outcome_attestations
BEGIN
    SELECT RAISE(ABORT, 'outcome_attestations is append-only: DELETE is forbidden');
END;
