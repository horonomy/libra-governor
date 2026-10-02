-- HORO-1669: the comparison substrate for shadow runtime decisions.
--
-- Without persistence, "compare recommendation with eventual
-- outcome/economics" (the ticket's own shadow-mode requirement) has
-- nothing for a later evidence-gate ticket (HORO-1673) to read. This
-- table is write-only from this migration's perspective: nothing here is
-- written back by the shadow-recording path itself.
--
-- Privacy: no column can hold prompt text, tool output, or any other
-- content payload -- `decision_json` carries only the structured,
-- content-free `RuntimeDecision` (see `libra_governor_domain::progressive`
-- module docs for the structural guarantee that type makes).
CREATE TABLE IF NOT EXISTS shadow_runtime_decisions (
    decision_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    plan_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    proposal_kind TEXT NOT NULL,
    decision_json TEXT NOT NULL,
    decision_schema_version TEXT NOT NULL,
    elapsed_secs INTEGER NOT NULL,
    tool_calls_total INTEGER NOT NULL,
    decided_at TEXT NOT NULL,
    UNIQUE (task_id, plan_id, decided_at)
);

CREATE INDEX IF NOT EXISTS idx_shadow_decisions_task
    ON shadow_runtime_decisions (task_id, decided_at);
