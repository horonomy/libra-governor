-- Session -> task mapping and in-flight preflight bookkeeping, added for
-- the Claude Code hook integration (HORO-1125).
--
-- A Claude Code `session_id` resets every time the host agent restarts,
-- but one task (per docs/adr/0002) may span many prompts within the same
-- session. `session_tasks` lets a second `UserPromptSubmit` hook
-- invocation in the same session resolve to the *same* TaskId and
-- produce a new CompletionContract revision, rather than minting a fresh
-- task per prompt.
--
-- `session_preflights` tracks which preflight (ExecutionPlan) is the
-- current in-flight one for a session, so a later prompt (or a user
-- cancelling mid-session) can supersede the prior in-flight record
-- instead of leaving it dangling as ambiguous "active" state.
--
-- Privacy: neither table stores raw prompt text, matching the invariant
-- documented in 0001_init.sql and `libra-governor-domain`.

CREATE TABLE IF NOT EXISTS session_tasks (
    session_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_preflights (
    session_id TEXT NOT NULL,
    plan_id TEXT NOT NULL REFERENCES plans (id),
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    -- 'in_flight' | 'superseded'
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (session_id, plan_id)
);

CREATE INDEX IF NOT EXISTS idx_session_preflights_session_status
    ON session_preflights (session_id, status);
