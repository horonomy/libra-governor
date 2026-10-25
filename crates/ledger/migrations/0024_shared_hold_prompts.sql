-- Every host prompt id a shared-pool turn hold has accepted (HORO-2250).
-- A re-prompt renews the lane's existing hold, whose durable idempotency key
-- keeps only the FIRST prompt's id, so a replayed Stop for a renewed prompt
-- could not be recognised and instead settled the lane's next hold. This maps
-- (principal, session, prompt id) to the hold that accepted it, written in
-- the same transaction as the renewal. It is deliberately not tied to the
-- hold's row or its pool: a retention prune deletes the hold but must not
-- forget which prompts it closed. A re-sent prompt id re-points to the newest
-- hold. Additive and forward-only.
CREATE TABLE IF NOT EXISTS shared_hold_prompts (
    principal_id   TEXT NOT NULL,
    session_id     TEXT NOT NULL,
    prompt_id      TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    PRIMARY KEY (principal_id, session_id, prompt_id)
);
