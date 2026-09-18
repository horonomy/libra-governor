-- Runtime re-estimation and replan persistence, added for HORO-1139
-- (runtime re-estimation and hysteresis-gated replanning).
--
-- `plans.replaces_plan_id` / `plans.replan_reason_json` mirror
-- `ExecutionPlan::replaces` / `ExecutionPlan::replan_reason`: a plan
-- produced by a replan links back to the plan it replaces and carries
-- the structured reason. Both nullable: an original preflight plan
-- (the overwhelming majority) sets neither.
--
-- `tool_call_counts` gains a same-tool streak tracker so the daemon can
-- detect a "possible loop" signal (the same tool invoked repeatedly,
-- back to back) cheaply, in the same fire-and-forget upsert
-- `hook post-tool-use` already does for the plain count
-- (`increment_tool_call_count`) -- see `libra_governor_domain::replan`
-- module docs for why this streak, not an explicit success/failure
-- field, is the genuinely available signal here.
--
-- `replan_events` is the durable, queryable history of every replan
-- (`ReplanRecord`): which plan it replaced, the structured reason, and
-- the recomputed remaining estimate -- so a task's replan history can be
-- inspected without reconstructing it from `plans` rows alone.
--
-- `replan_state` is per-task hysteresis bookkeeping (auto-replan count,
-- last-replan timestamp) so cooldown/max-replan-count survive a daemon
-- restart -- a task, unlike a session, is expected to span more than one
-- daemon process lifetime (see docs/adr/0002).
--
-- Privacy: none of these additions store raw prompt text or raw tool
-- output, matching the invariant documented in 0001_init.sql and
-- `libra-governor-domain`. `replan_reason_json`/`replan_events.detail`
-- carry only the structured, developer-authored detail strings this
-- integration itself constructs (e.g. "17 tool calls vs typical 6"),
-- never raw prompt or tool-output content.

ALTER TABLE plans ADD COLUMN replaces_plan_id TEXT;
ALTER TABLE plans ADD COLUMN replan_reason_json TEXT;

ALTER TABLE tool_call_counts ADD COLUMN last_tool_name TEXT;
ALTER TABLE tool_call_counts ADD COLUMN last_tool_at TEXT;
ALTER TABLE tool_call_counts ADD COLUMN same_tool_streak INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS replan_events (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    prior_plan_id TEXT NOT NULL,
    new_plan_id TEXT NOT NULL,
    trigger TEXT NOT NULL,
    detail TEXT,
    remaining_estimate_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_replan_events_task_id_created_at
    ON replan_events (task_id, created_at);

CREATE TABLE IF NOT EXISTS replan_state (
    task_id TEXT PRIMARY KEY REFERENCES tasks (task_id),
    auto_replan_count INTEGER NOT NULL DEFAULT 0,
    last_replan_at TEXT
);
