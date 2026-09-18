-- Provider-gateway request provenance, added for HORO-1144 (add
-- provider gateway enforcement for hard metered budgets without making
-- MCP the enforcement boundary).
--
-- One row per terminal transition of the gateway's per-request state
-- machine: what was asked for, what was decided, what it was reserved
-- at, what it actually cost, and which prices priced it. This is the
-- evidence behind "record provider/model/pricing-version provenance" and
-- "expose why a request was denied/approval-gated without leaking
-- secrets".
--
-- Privacy: this table extends 0001_init.sql's structural guarantee to
-- the one component that touches data leaving the machine. There is no
-- column for a request body, a response body, a header, a prompt, or
-- tool output -- every column below is a numeric amount, a token count,
-- an enum tag, an identifier, a status code, or a timestamp. A future
-- "just store the body for debugging" is therefore a schema change with
-- a review attached, not a one-field addition. There is likewise no
-- credential column, and none is needed: the upstream credential never
-- leaves the daemon's memory (see
-- `libra_governor_gateway::credential`).
--
-- `task_id` is deliberately NULLABLE and carries NO foreign key, unlike
-- every other task reference in this schema. The rows an auditor most
-- wants -- a request refused for a missing task binding, ambiguous
-- credentials, a Host-header mismatch, or an unroutable path -- have no
-- task at all, and a NOT NULL or FK-constrained column would make
-- exactly those rows un-insertable while `PRAGMA foreign_keys = ON`.
-- The same reasoning applies to `session_id`, `model`, and
-- `reservation_id`: a request rejected before admission never acquired
-- any of them.
--
-- `reserved_amount`/`settled_amount` mirror `reservations`' storage
-- shape (a REAL value plus a `resource_kind` discriminator) rather than
-- a JSON blob, for the same reason 0006 gave: so spend can be summed in
-- SQL.
--
-- `usage_known` distinguishes a settlement from the provider's own
-- reported token counts from the conservative reserved-amount fallback
-- -- the same honesty flag `reservations.usage_known` carries.
--
-- `bound_violated` records that the provider reported more output tokens
-- than the request's own `max_tokens` declared. That should be
-- impossible; it is recorded rather than clamped so a violated
-- assumption is visible instead of silently absorbed into the numbers.

CREATE TABLE IF NOT EXISTS gateway_requests (
    id TEXT PRIMARY KEY,
    -- Nullable, un-FK'd: see the header comment above.
    task_id TEXT,
    session_id TEXT,
    route TEXT,
    model TEXT,
    tier TEXT NOT NULL,
    decision TEXT NOT NULL,
    -- Amounts and limits only -- never anything derived from a body.
    decision_detail_json TEXT,
    reservation_id TEXT,
    reserved_amount REAL,
    settled_amount REAL,
    resource_kind TEXT,
    usage_known INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER,
    cache_creation_input_tokens INTEGER,
    cache_read_input_tokens INTEGER,
    output_tokens INTEGER,
    max_tokens INTEGER,
    bound_violated INTEGER NOT NULL DEFAULT 0,
    pricing_version TEXT NOT NULL,
    upstream_status INTEGER,
    terminal_state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    closed_at TEXT
);

-- Serves "what did this task spend through the gateway".
CREATE INDEX IF NOT EXISTS idx_gateway_requests_task
    ON gateway_requests (task_id, created_at);
-- Serves "why are requests being refused", the statusline's own question.
CREATE INDEX IF NOT EXISTS idx_gateway_requests_decision
    ON gateway_requests (decision, created_at);
