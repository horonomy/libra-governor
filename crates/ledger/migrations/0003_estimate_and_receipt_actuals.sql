-- Estimator output storage and actual-execution capture, added for
-- HORO-1126 (preflight estimates, actual capture, Execution Receipts).
--
-- `plans.estimate_json` stores the `Estimate` (HORO-1126) computed at
-- preflight time, so `hook stop` can compare the ORIGINAL estimate
-- against actuals at finalization without re-running the estimator.
-- Nullable: a plan inserted before this migration (impossible in a fresh
-- MVP 1.0 database, but not in one upgraded in place) has no estimate.
--
-- `tool_call_counts` is a cheap, fire-and-forget per-session counter
-- incremented by `hook post-tool-use`. Keyed by `session_id` (not
-- `task_id`) so incrementing it never needs a session -> task lookup on
-- the hot path of every tool call.
--
-- `receipts.tool_call_count` / `model` / `provider` extend the receipt
-- schema to match `libra_governor_domain::ExecutionReceipt`'s new
-- fields. `model` and `provider` are nullable: Claude Code's hook
-- payloads expose `model` but never `provider` (see
-- `ExecutionReceipt::provider` docs) — this is an honest "unknown", not
-- a default value that could be mistaken for real data.
--
-- Privacy: none of these additions store raw prompt text or raw tool
-- output, matching the invariant documented in 0001_init.sql and
-- `libra-governor-domain`.

ALTER TABLE plans ADD COLUMN estimate_json TEXT;

CREATE TABLE IF NOT EXISTS tool_call_counts (
    session_id TEXT PRIMARY KEY,
    count INTEGER NOT NULL DEFAULT 0
);

ALTER TABLE receipts ADD COLUMN tool_call_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE receipts ADD COLUMN model TEXT;
ALTER TABLE receipts ADD COLUMN provider TEXT;
