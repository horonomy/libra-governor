-- Lifetime scope marker on task budgets, added for HORO-1727 S3
-- (scope-separated resource policy).
--
-- `task_budgets.lifetime_bounded` records whether a task's hard limit is a
-- lifetime bound (1) or is scoped to a turn (0). Every existing row, and
-- every row written by a build that does not know this column, is
-- lifetime-bounded: the column default is 1, so no backfill statement is
-- needed and an older binary inserting into a migrated database fails
-- closed. No code reads this column yet, so applying the migration changes
-- no behavior.

ALTER TABLE task_budgets ADD COLUMN lifetime_bounded INTEGER NOT NULL DEFAULT 1
    CHECK (lifetime_bounded IN (0, 1));
