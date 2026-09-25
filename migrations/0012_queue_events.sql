-- dagq-schema: breaking
-- A failed or timed-out call to the workspace backend (cmux) is recorded as
-- `backend_call_failed`. A call that belongs to no run (the maintainer and
-- in-cmux supervisor workspaces `up` opens, the queue's workspace group,
-- `down`'s close) has neither a task nor a goal, so run_events is rebuilt to
-- admit that one kind without either; every other kind keeps the old rule.
-- The id order is preserved; the migration runner disables foreign keys
-- around this script and runs foreign_key_check before committing.
CREATE TABLE run_events_v12 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (task_id IS NOT NULL OR goal_id IS NOT NULL OR kind = 'backend_call_failed'),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v12 (id, task_id, goal_id, run_id, kind, payload, created_at)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v12 RENAME TO run_events;
CREATE INDEX events_by_task ON run_events(task_id, id);
CREATE INDEX events_by_goal ON run_events(goal_id, id);
