-- Cumulative subagent-transcript charges that survived a retention prune
-- (HORO-2250). A subagent's "already charged" total is the sum of its
-- charge rows across pools; pruning an ended pool deleted those rows, so a
-- replay or a grown transcript after the prune re-booked the whole
-- cumulative. `prune` now folds each pruned pool's subagent-transcript
-- charges into this table, in the same transaction as the delete, keyed by
-- the family prefix of the idempotency key (everything up to its last '|').
-- Not tied to a period or a pool, so it is never pruned. Forward-only:
-- additive table, no change to existing rows.
CREATE TABLE IF NOT EXISTS shared_charge_floor (
    principal_id TEXT NOT NULL,
    key_prefix   TEXT NOT NULL,
    total        INTEGER NOT NULL CHECK (total >= 0),
    PRIMARY KEY (principal_id, key_prefix)
);
