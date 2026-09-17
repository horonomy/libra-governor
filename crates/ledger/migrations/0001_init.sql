-- Initial schema for the Libra Governor local trajectory store.
--
-- Privacy: no column here stores raw prompt text or raw tool output
-- content. `events.payload_json` stores only the normalized
-- `ExecutionEventKind` fields (session ids, tool names, booleans) --
-- see `libra-governor-domain`'s crate-level privacy invariant docs.

CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    external_ref_kind TEXT,
    external_ref_value TEXT,
    created_at TEXT NOT NULL,
    -- Updated in the same transaction as each event insert; lets a reader
    -- find recently active tasks without scanning `events`.
    last_event_at TEXT
);

CREATE TABLE IF NOT EXISTS contracts (
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    revision INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, revision)
);

CREATE TABLE IF NOT EXISTS contract_criteria (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    description TEXT NOT NULL,
    required INTEGER NOT NULL,
    PRIMARY KEY (task_id, revision, ordinal),
    FOREIGN KEY (task_id, revision) REFERENCES contracts (task_id, revision)
);

-- `id` is the event's idempotency key: re-applying an insert with an id
-- that already exists is a no-op (see LedgerStore::insert_event).
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    occurred_at TEXT NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_task_id_occurred_at
    ON events (task_id, occurred_at);

CREATE TABLE IF NOT EXISTS plans (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    recon_snapshot_ref TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (task_id, contract_revision) REFERENCES contracts (task_id, revision)
);

CREATE INDEX IF NOT EXISTS idx_plans_task_id ON plans (task_id);

CREATE TABLE IF NOT EXISTS receipts (
    task_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    actual_duration_secs INTEGER NOT NULL,
    actual_usage_json TEXT NOT NULL,
    outcome_json TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    PRIMARY KEY (task_id, plan_id),
    FOREIGN KEY (plan_id) REFERENCES plans (id)
);
