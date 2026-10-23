-- Executor-keyed shared-pool rows (HORO-2194, local-first Shared-Pool MVP).
--
-- A shared-pool row may be attributed to an executor within its session:
-- the key is `(session_id, agent_id | NULL)`. All three columns are
-- nullable with no backfill, so every row written before this migration
-- (and every row written by a caller that supplies no attribution) stays
-- unattributed -- the migration never guesses an owner for legacy spend.
--
-- `agent_id` NULL is NOT a `root` lineage and NOT "unknown": whether the
-- absence means "the session's main executor" or "agent not known" is
-- carried by `executor_kind`:
--   agent_absent  -- main executor of the session (agent_id is NULL)
--   agent         -- a specific agent (agent_id is set)
--   unattributed  -- session known, agent unknown (agent_id is NULL)
-- `executor_kind` itself is NULL for legacy / unattributed-by-omission
-- rows; usage queries read NULL as `unattributed`.
--
-- The agent_id <-> executor_kind consistency rule is enforced by the
-- ledger layer (ALTER TABLE ADD COLUMN cannot add a table-level CHECK).
--
-- `charge_source` records how the spend reached the ledger.

ALTER TABLE shared_pool_reservations ADD COLUMN agent_id TEXT;
ALTER TABLE shared_pool_reservations ADD COLUMN executor_kind TEXT
    CHECK (executor_kind IS NULL OR executor_kind IN ('agent_absent', 'agent', 'unattributed'));
ALTER TABLE shared_pool_reservations ADD COLUMN charge_source TEXT
    CHECK (charge_source IS NULL OR charge_source IN ('turn_hold', 'subagent_transcript', 'gateway'));
