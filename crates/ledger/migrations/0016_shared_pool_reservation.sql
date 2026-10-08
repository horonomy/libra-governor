-- The flat shared-quota-pool reservation ledger (HORO-1763): atomically
-- reserve shared quota pool capacity across concurrent sessions and
-- settle without double count.
--
-- This is deliberately a SEPARATE table set from `task_budgets` /
-- `reservations` (migration 0006), not a reuse of them with a nullable
-- `pool_id` column bolted on: a shared pool has no task, no plan, no
-- Completion Reserve, and no account hierarchy (HORO-1694 stays latent --
-- see `libra_governor_domain::quota_window`'s `QuotaSubject::SharedPool`
-- module docs). Overloading the task-reservation tables with an optional
-- pool concept would make every existing task-path query need to reason
-- about a case that can never apply to it -- the AC4 backward-compat
-- requirement is satisfied more directly by never touching those tables
-- at all.
--
-- `quota_pools.capacity` is fixed at first creation, mirroring
-- `task_budgets.hard_limit`'s "the limit in force at admission is the
-- limit for the life of the envelope" discipline (see
-- `libra_governor_ledger::shared_pool::ensure_pool`).
--
-- `shared_pool_reservations` mirrors `reservations`'s shape exactly where
-- the concepts coincide (state lifecycle, `expires_at` for crash/restart
-- reconciliation) and drops what does not apply to a flat pool (class,
-- drawn_from_reserve, plan_id, account_id/lease_kind). `amount`/
-- `settled_amount` are INTEGER, not REAL: a quota pool's unit (tokens,
-- requests, opaque provider credit's base integer, USD cents) is always a
-- whole-number base integer in this contract (see `QuotaAmount.value:
-- u64`), so summing as INTEGER avoids the floating-point accumulation
-- drift the task ledger's REAL amounts accept at a much smaller
-- historical scale. `settled_amount >= 0` is enforced the same way
-- `amount >= 0` already is -- a caller-supplied actual-usage figure is
-- rejected by the ledger layer before it ever reaches this column (see
-- `checked_i64`), and the CHECK is a second, independent backstop against
-- a negative value ever being written at all.
--
-- Idempotency-key uniqueness is scoped to `(pool_id, principal_id,
-- idempotency_key)`, NOT `(pool_id, idempotency_key)` alone: a shared
-- pool is reserved by MANY principals, and an idempotency key is a
-- caller-chosen string with no global-uniqueness guarantee across
-- actors. Scoping only to the pool would let one principal's replay
-- lookup collide with a different principal's row -- returning someone
-- else's reservation (its principal_id, session_id, amount) to a caller
-- who never created it, rather than creating or finding that caller's
-- own idempotent row. Scoping to the principal as well makes that
-- collision structurally impossible rather than merely unlikely.
--
-- `shared_pool_provider_snapshots` holds only the LATEST ingested gauge
-- reading per pool (`pool_id` is its own primary key, upserted on
-- ingest) -- AC3 requires a gauge snapshot never be summed as spend, which
-- this schema makes structurally impossible rather than merely
-- policy-enforced: there is nowhere to sum, only one row to read.
-- `disclosed`/`used_value`/`declared_limit` mirror
-- `libra_governor_domain::quota_window::GaugeReading`'s two states
-- (`Undisclosed` vs `Used { used, limit }`) without re-deriving that
-- distinction under new names.
--
-- Privacy: every column here is a numeric amount, a state/unit
-- discriminator, an id, a caller-chosen idempotency key, or a principal/
-- session identifier string -- never prompt or tool-output content,
-- matching the invariant documented in 0001_init.sql.

CREATE TABLE IF NOT EXISTS quota_pools (
    pool_id TEXT PRIMARY KEY,
    unit_json TEXT NOT NULL,
    capacity INTEGER NOT NULL CHECK (capacity >= 0),
    schema_version TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS shared_pool_reservations (
    id TEXT PRIMARY KEY,
    pool_id TEXT NOT NULL REFERENCES quota_pools (pool_id),
    principal_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK (amount >= 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'settled', 'released', 'expired')),
    settled_amount INTEGER CHECK (settled_amount IS NULL OR settled_amount >= 0),
    usage_known INTEGER,
    idempotency_key TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    settled_at TEXT,
    released_at TEXT,
    schema_version TEXT NOT NULL
);

-- Idempotent reserve retries collide here rather than creating a second
-- hold -- scoped to the principal as well as the pool (see module docs
-- above) so a key collision across two different actors can never merge
-- their requests.
CREATE UNIQUE INDEX IF NOT EXISTS idx_shared_pool_reservations_pool_principal_idempotency
    ON shared_pool_reservations (pool_id, principal_id, idempotency_key);
-- Serves the admission sub-selects (SUM over active/settled per pool).
CREATE INDEX IF NOT EXISTS idx_shared_pool_reservations_pool_state
    ON shared_pool_reservations (pool_id, state);
-- Serves the startup/periodic stale-reservation reconciliation sweep.
CREATE INDEX IF NOT EXISTS idx_shared_pool_reservations_state_expires
    ON shared_pool_reservations (state, expires_at);

CREATE TABLE IF NOT EXISTS shared_pool_provider_snapshots (
    pool_id TEXT PRIMARY KEY REFERENCES quota_pools (pool_id),
    observed_at TEXT NOT NULL,
    valid_until TEXT,
    disclosed INTEGER NOT NULL,
    used_value INTEGER,
    declared_limit INTEGER,
    confidence TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
