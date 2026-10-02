-- Hierarchical resource accounts and lease custody (HORO-1668).
--
-- Extends the task-level Completion Reserve ledger (0006) into a tree:
-- organization -> principal -> task -> session -> agent -> nested-agent
-- sublease. Not every deployment has every level; a task account with no
-- principal/org ancestor is fully valid local accounting (see ADR-0008).
--
-- `resource_accounts` is the account tree. The task level is deliberately
-- NOT given its own capacity/reserve columns -- `task_budgets` already
-- owns those numbers (one source of truth, no second copy to drift), so
-- `granted_capacity`/`protected_reserve` are only ever read for a task
-- account through `v_account_capacity`, which case-analyzes level='task'
-- vs everything else in one place. The CHECK constraint makes storing a
-- second copy at the task level unrepresentable, not just discouraged.
--
-- Custody lineage (the `parent_account_id` edge here) is Libra-minted,
-- proven only by the act of leasing -- a parent account asked for a
-- sublease, so an edge exists. This is a DIFFERENT tree from
-- `economic_rollup::AgentLineage` (provider-proven agent parentage, which
-- is an all-roots forest today since neither Claude Code nor Codex expose
-- a trustworthy `parent_agent_id`). The two may legitimately disagree; see
-- ADR-0008 "Custody lineage is not provider lineage". `provider_lineage_status`
-- and `execution_dimension_key_json` are recorded here as non-load-bearing
-- truth when an `ExecutionIdentity` is genuinely available (today, always
-- NULL from the daemon path -- nothing persists or transmits the envelope
-- yet, see ADR-0008).
--
-- Account id minting needs no CAS/version column (see
-- `libra_governor_ledger::reservation` module docs for why `BEGIN
-- IMMEDIATE` alone is sufficient): a task account's id is deterministically
-- its own task uuid string, and every other level is minted via
-- `INSERT ... ON CONFLICT(parent_account_id, level, natural_key) DO
-- NOTHING` then a SELECT of whichever row won -- the same idiom
-- `initialize_task_budget` already uses for `task_budgets`.
--
-- `authority_source` records where an allocation's authority came from.
-- `remote_lease_authority` is typed now, refused at construction in
-- v0.0.3 -- the protocol/domain seam for a future remote lease exists
-- without building the SaaS (ticket non-goal). `enforcement_scope` is
-- always 'local_device' today; 'remote_authoritative' has no producer.
--
-- Privacy: no column here stores raw prompt text or raw tool output,
-- matching the invariant in 0001_init.sql.
CREATE TABLE IF NOT EXISTS resource_accounts (
    account_id          TEXT PRIMARY KEY,
    level               TEXT NOT NULL CHECK (level IN
                          ('organization','principal','task','session','agent')),
    parent_account_id   TEXT REFERENCES resource_accounts (account_id),
    resource_kind       TEXT NOT NULL,
    natural_key         TEXT NOT NULL,
    task_id             TEXT REFERENCES tasks (task_id),
    granted_capacity    REAL CHECK (granted_capacity IS NULL OR granted_capacity >= 0),
    protected_reserve   REAL CHECK (protected_reserve IS NULL OR protected_reserve >= 0),
    funding_lease_id    TEXT,
    authority_source    TEXT CHECK (authority_source IS NULL OR authority_source IN
                          ('local_user_config','imported_corporate_snapshot',
                           'remote_lease_authority')),
    enforcement_scope   TEXT NOT NULL CHECK (enforcement_scope IN
                          ('local_device','remote_authoritative')),
    provider_lineage_status TEXT,
    execution_dimension_key_json TEXT,
    provenance          TEXT NOT NULL CHECK (provenance IN ('native','legacy_backfill_0011')),
    account_schema_version TEXT NOT NULL,
    state               TEXT NOT NULL CHECK (state IN ('open','closed','expired')),
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    closed_at           TEXT,
    CHECK (level <> 'task'
           OR (granted_capacity IS NULL AND protected_reserve IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_resource_accounts_natural
    ON resource_accounts (parent_account_id, level, natural_key);
CREATE INDEX IF NOT EXISTS idx_resource_accounts_parent
    ON resource_accounts (parent_account_id);
CREATE INDEX IF NOT EXISTS idx_resource_accounts_task_level
    ON resource_accounts (task_id, level);

-- The single place the task-vs-other-level capacity case analysis lives.
CREATE VIEW IF NOT EXISTS v_account_capacity AS
  SELECT a.account_id, a.level, a.parent_account_id, a.resource_kind, a.state,
         CASE WHEN a.level = 'task' THEN b.hard_limit        ELSE a.granted_capacity  END
           AS granted_capacity,
         CASE WHEN a.level = 'task' THEN b.completion_reserve ELSE a.protected_reserve END
           AS protected_reserve
  FROM resource_accounts a
  LEFT JOIN task_budgets b ON b.task_id = a.task_id AND a.level = 'task';

-- Evolve `reservations` into the lease table. ADD COLUMN with a NULL
-- default is the only form SQLite allows alongside a REFERENCES clause.
ALTER TABLE reservations ADD COLUMN account_id TEXT
    REFERENCES resource_accounts (account_id);
ALTER TABLE reservations ADD COLUMN grants_account_id TEXT
    REFERENCES resource_accounts (account_id);
ALTER TABLE reservations ADD COLUMN lease_kind TEXT;
ALTER TABLE reservations ADD COLUMN settled_after_expiry INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reservations ADD COLUMN legacy_pre_0011 INTEGER NOT NULL DEFAULT 0;

-- Backfill: one task-level account per existing task_budgets row, with
-- account_id == the task's own uuid string (deterministic, no generation
-- needed, and keeps COALESCE(account_id, task_id) identity-preserving).
INSERT INTO resource_accounts (
    account_id, level, parent_account_id, resource_kind, natural_key, task_id,
    granted_capacity, protected_reserve, funding_lease_id, authority_source,
    enforcement_scope, provider_lineage_status, execution_dimension_key_json,
    provenance, account_schema_version, state, created_at, updated_at, closed_at)
SELECT task_id, 'task', NULL, resource_kind, task_id, task_id,
       NULL, NULL, NULL, 'local_user_config',
       'local_device', NULL, NULL,
       'legacy_backfill_0011', 'resource-account-v1', 'open',
       created_at, updated_at, NULL
FROM task_budgets;

UPDATE reservations
   SET account_id = task_id,
       lease_kind = 'work_hold',
       legacy_pre_0011 = 1
 WHERE account_id IS NULL;

-- Idempotency: expression index so a forgotten account_id cannot silently
-- break dedupe (SQLite treats NULLs as distinct from each other). For
-- legacy rows account_id == task_id after the backfill above, so the
-- pre-0011 dedupe guarantee is preserved byte-for-byte.
DROP INDEX IF EXISTS idx_reservations_task_idempotency;
CREATE UNIQUE INDEX IF NOT EXISTS idx_reservations_account_idempotency
    ON reservations (COALESCE(account_id, task_id), idempotency_key);

CREATE INDEX IF NOT EXISTS idx_reservations_account_state
    ON reservations (account_id, state);
CREATE INDEX IF NOT EXISTS idx_reservations_grants_account
    ON reservations (grants_account_id);
