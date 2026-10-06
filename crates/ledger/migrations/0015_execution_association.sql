-- Additive owner state. Legacy host/session rows acquire no inferred identity.
CREATE TABLE execution_lanes (
    lane TEXT PRIMARY KEY,
    current_turn TEXT NOT NULL,
    exact_context TEXT NOT NULL
);
CREATE TABLE execution_turns (
    lane TEXT NOT NULL REFERENCES execution_lanes(lane),
    turn TEXT NOT NULL,
    exact_identity TEXT NOT NULL,
    identity_json TEXT NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    initial_plan_id TEXT NOT NULL REFERENCES plans(id),
    state TEXT NOT NULL CHECK (state IN ('active','superseded','finalized')),
    PRIMARY KEY (lane, turn)
);
CREATE TABLE execution_replays (
    lane TEXT NOT NULL,
    operation TEXT NOT NULL,
    native_ref TEXT NOT NULL,
    turn TEXT NOT NULL,
    target_json TEXT NOT NULL,
    PRIMARY KEY (lane, operation, native_ref),
    FOREIGN KEY (lane,turn) REFERENCES execution_turns(lane,turn)
);
