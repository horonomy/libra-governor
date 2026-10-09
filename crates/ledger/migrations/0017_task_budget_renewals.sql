-- Bounded, auditable task-budget renewal grants (HORO-1727).
--
-- `task_budget_renewals` is append-only by construction, not merely by
-- convention: the `BEFORE UPDATE`/`BEFORE DELETE` triggers below abort
-- any attempt to mutate or remove a row. A renewal grant is a fact about
-- what was authorized and why at a specific moment -- correcting a
-- mistaken grant means recording a NEW fact (e.g. a future ticket's
-- revocation path appending its own row), never rewriting history.
--
-- Deliberately a separate table from `task_budgets`, mirroring migration
-- 0016's own "do not bolt an optional concept onto the existing table"
-- discipline: `task_budgets.hard_limit` stays immutable forever (see
-- `libra_governor_domain::TaskBudget::hard_limit`'s own docs), and the
-- *sum* of this table's `amount` column is the one and only
-- `renewed_capacity` a read site adds to it (see
-- `libra_governor_domain::TaskBudget::effective_hard_limit`). There is
-- deliberately no denormalized running-total column on `task_budgets`
-- itself -- that would be a second place this number could drift from
-- the ledger of grants that produced it.
--
-- `UNIQUE(task_id, idempotency_key)` gives idempotent replay: resending
-- the same grant request returns the existing row rather than granting
-- twice (see `LedgerStore::grant_renewal`).
--
-- `settled_at_grant`/`active_at_grant`/`effective_before`/
-- `effective_after`/`quota_status_json` are the audit snapshot taken at
-- the moment this grant was evaluated -- not re-derivable later, since
-- settled/active spend keeps moving after the grant. Recording them here
-- is what lets a future reviewer answer "was this grant justified at the
-- time" without needing a second, separately-retained audit log.
--
-- `authority_json` stores `libra_governor_domain::RenewalAuthority`'s
-- serialized shape -- today always `{"kind":"operator",...}`, since that
-- is the mechanism's only variant (see that type's own module docs for
-- why).
--
-- Privacy: `reason` is operator-authored justification text, never
-- prompt or tool-output content -- matching the invariant in
-- 0001_init.sql. Every other column here is a numeric amount, a state/
-- unit discriminator, an id, a caller-chosen idempotency key, or a
-- serialized domain value already subject to that same invariant
-- elsewhere in this schema.
CREATE TABLE IF NOT EXISTS task_budget_renewals (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    amount REAL NOT NULL CHECK (amount > 0),
    authority_json TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    reason TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    settled_at_grant REAL NOT NULL,
    active_at_grant REAL NOT NULL,
    effective_before REAL NOT NULL,
    effective_after REAL NOT NULL,
    quota_status_json TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    granted_at TEXT NOT NULL,
    UNIQUE (task_id, idempotency_key)
);

-- Serves `granted_capacity(task_id)` (the `SUM(amount)` every effective-
-- ceiling read site performs) and `grant_renewal`'s own count-against-
-- `max_renewals` gate.
CREATE INDEX IF NOT EXISTS idx_task_budget_renewals_task
    ON task_budget_renewals (task_id);

CREATE TRIGGER IF NOT EXISTS trg_task_budget_renewals_no_update
BEFORE UPDATE ON task_budget_renewals
BEGIN
    SELECT RAISE(ABORT, 'task_budget_renewals is append-only: UPDATE is forbidden');
END;

CREATE TRIGGER IF NOT EXISTS trg_task_budget_renewals_no_delete
BEFORE DELETE ON task_budget_renewals
BEGIN
    SELECT RAISE(ABORT, 'task_budget_renewals is append-only: DELETE is forbidden');
END;
