-- Local extension points -- Business Context Provider, Policy Webhook,
-- signed event delivery, and outcome push -- added for HORO-1174.
--
-- Privacy: no column here stores raw prompt text, raw tool output, a
-- request/response body, a header, or a secret. `business_context`'s
-- `advisory_criteria_json`/`external_refs_json` store only the small,
-- capped, structured provider response fields (see
-- `libra-governor-extension`'s wire layer) -- never a body. There is no
-- secret column anywhere in this file: the webhook HMAC secret never
-- leaves `libra-governor-extension::secret::WebhookSecret`'s memory (see
-- that module's docs, mirroring `libra-governor-gateway::credential`'s
-- discipline) and is never written to any table. This extends the same
-- structural guarantee `0001_init.sql` and `0007_gateway_requests.sql`
-- establish to every new surface this migration adds.
--
-- `business_context.task_id` carries a `REFERENCES tasks (task_id)`
-- (unlike `0007_gateway_requests.gateway_requests.task_id`): a fetched
-- business context is always attributed to a real, already-resolved task
-- at preflight time (there is no "context fetched before a task exists"
-- case the way there is a "request rejected before a task was bound"
-- case for the gateway), so the stricter FK is the honest shape here.
--
-- `outcome_attestations` intentionally carries NO FK on `task_id`: an
-- outcome push (`Request::RecordOutcome`) may legitimately name an
-- unknown/stale `task_id` (the provider's own bookkeeping may outlive
-- this daemon's ledger, or simply be wrong), and that is exactly the
-- `OutcomeRecordedOutcome::NoSuchTask` case the daemon must be able to
-- report rather than fail to write at all. `UNIQUE(task_id, source_id,
-- idempotency_key)` is the outcome-push idempotency key: a duplicate push
-- is a no-op (`INSERT OR IGNORE`), never a second attestation row.
--
-- `webhook_deliveries` has NO signature/timestamp/nonce column, and that
-- absence is deliberate, not an oversight: a signature is a function of
-- (secret, timestamp, nonce, body) and every one of those except the body
-- is minted fresh on every delivery *attempt*, never at enqueue time --
-- persisting a signature here would either go stale by the next retry or
-- have to be silently recomputed from a persisted timestamp/nonce that
-- then would not need to change per attempt, defeating replay protection
-- on retries. `payload_json` stores the exact serialized `EventEnvelope`
-- bytes so a retry resends byte-identical content; the signature over
-- those bytes is computed fresh in `libra-governor-extension::dispatcher`
-- for every attempt, never read back from this table.
--
-- `UNIQUE(event_kind, dedupe_key)` plus `INSERT OR IGNORE` is the
-- enqueue-time dedupe: a crash-restart replaying the same
-- `handle_preflight`/`handle_tool_invoked`/`handle_finalize` call cannot
-- double-enqueue the same admission/replan/approval/outcome event.

CREATE TABLE IF NOT EXISTS business_context (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    plan_id TEXT,
    provider_id TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    priority TEXT,
    deadline TEXT,
    cost_center TEXT,
    advisory_criteria_json TEXT,
    external_refs_json TEXT,
    applied INTEGER NOT NULL DEFAULT 0,
    received_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_business_context_task
    ON business_context (task_id, received_at);

CREATE TABLE IF NOT EXISTS outcome_attestations (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    plan_id TEXT,
    source TEXT NOT NULL,
    source_id TEXT,
    outcome_kind TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    authoritative INTEGER NOT NULL,
    attested_at TEXT NOT NULL,
    UNIQUE (task_id, source_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_outcome_attestations_task
    ON outcome_attestations (task_id, attested_at);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    event_id TEXT PRIMARY KEY,
    event_kind TEXT NOT NULL,
    dedupe_key TEXT NOT NULL,
    task_id TEXT,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    last_status INTEGER,
    last_error TEXT,
    created_at TEXT NOT NULL,
    delivered_at TEXT,
    UNIQUE (event_kind, dedupe_key)
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_pending
    ON webhook_deliveries (state, next_attempt_at);
