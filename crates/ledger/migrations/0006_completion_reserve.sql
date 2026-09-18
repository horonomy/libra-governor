-- The atomic reservation ledger and Completion Reserve persistence,
-- added for HORO-1141 (enforce Completion Reserve and atomic resource
-- reservations across concurrent Claude Code work).
--
-- `task_budgets` is the per-task resource envelope: the hard limit
-- copied from the admitting Policy's `resource.hard_ceiling` (one source
-- of truth, no separate limit configuration), and the live, protected
-- `completion_reserve` -- the amount required completion work
-- (tests/build/lint, final verification, bounded failure recovery) is
-- guaranteed regardless of how much optional/exploration work has
-- already spent. `policy_json` persists the exact Policy in force at
-- admission so a later admission/replan decision, and startup
-- reconciliation after a daemon restart, stays reproducible without
-- re-reading daemon configuration.
--
-- `reservations` is the atomic reservation ledger itself. Amounts are
-- stored as a `REAL` value plus a `resource_kind` discriminator --
-- deliberately NOT the JSON-blob shape `receipts.actual_usage_json`
-- uses -- because the ticket's own formula
--   available = hard_limit - settled_spend - active_reservations - completion_reserve
-- must be computable as a single SQL SUM() over this table; a JSON
-- amount cannot be summed in SQL. `REAL` represents every realistic
-- `UsdCents`/`Tokens`/`QuotaPercent` value exactly at this scale.
--
-- `idempotency_key` (unique per `task_id`) is the structural half of
-- "idempotent retry/recovery": a replayed reserve request with the same
-- key collides against `idx_reservations_task_idempotency` instead of
-- creating a second reservation.
--
-- `drawn_from_reserve` records, at reserve time, how much of this
-- reservation's `amount` was drawn out of the task's protected
-- `completion_reserve` (only ever nonzero for `RequiredWork` once
-- ordinary headroom is exhausted -- see
-- `libra_governor_domain::completion_reserve_for` and
-- `libra_governor_ledger::reservation` module docs). It is historical
-- receipt evidence, not a live claim: settling, releasing, or expiring a
-- reservation restores the appropriate amount back onto
-- `task_budgets.completion_reserve` in the same transaction.
--
-- `expires_at` bounds how long a reservation may stay `active` before
-- startup/periodic reconciliation (`expire_stale_reservations`) reclaims
-- it -- the crash/restart recovery path: a reservation issued by a
-- process that then crashed leaves its row `active` forever otherwise.
--
-- Privacy: none of these additions store raw prompt text or raw tool
-- output, matching the invariant documented in 0001_init.sql and
-- `libra-governor-domain`. Every column here is a numeric amount, a
-- resource-kind/class/state discriminator, an id, or a caller-chosen
-- idempotency key string -- never prompt or tool-output content.

CREATE TABLE IF NOT EXISTS task_budgets (
    task_id TEXT PRIMARY KEY REFERENCES tasks (task_id),
    resource_kind TEXT NOT NULL,
    hard_limit REAL NOT NULL CHECK (hard_limit >= 0),
    initial_completion_reserve REAL NOT NULL CHECK (initial_completion_reserve >= 0),
    completion_reserve REAL NOT NULL CHECK (completion_reserve >= 0),
    completion_reserve_basis TEXT NOT NULL,
    policy_json TEXT NOT NULL,
    policy_schema_version TEXT NOT NULL,
    reservation_schema_version TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reservations (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    session_id TEXT NOT NULL,
    plan_id TEXT REFERENCES plans (id),
    class TEXT NOT NULL CHECK (class IN ('required_work', 'optional_work')),
    resource_kind TEXT NOT NULL,
    amount REAL NOT NULL CHECK (amount >= 0),
    drawn_from_reserve REAL NOT NULL DEFAULT 0 CHECK (drawn_from_reserve >= 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'settled', 'released', 'expired')),
    settled_amount REAL,
    usage_known INTEGER,
    idempotency_key TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    settled_at TEXT,
    released_at TEXT
);

-- Idempotent reserve retries collide here rather than creating a second
-- reservation row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_reservations_task_idempotency
    ON reservations (task_id, idempotency_key);
-- Serves the `available` headroom sub-selects (SUM over active/settled).
CREATE INDEX IF NOT EXISTS idx_reservations_task_state
    ON reservations (task_id, state);
-- Serves the startup/periodic stale-reservation reconciliation sweep.
CREATE INDEX IF NOT EXISTS idx_reservations_state_expires
    ON reservations (state, expires_at);

ALTER TABLE receipts ADD COLUMN reservation_evidence_json TEXT;
