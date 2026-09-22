-- Goals (ADR-0009): a goal groups the tasks that solve one higher-level
-- problem. It has no state machine; progress derives from its tasks' status
-- and closing is recorded once in closed_at / verdict. Tasks gain an optional
-- goal and a free-form context. Goal-level events have no task, so run_events
-- is rebuilt with a nullable task_id and a goal_id, its id order preserved;
-- the migration runner disables foreign keys around this script and runs
-- foreign_key_check before committing.
CREATE TABLE goals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    description TEXT NOT NULL DEFAULT '',
    acceptance TEXT NOT NULL DEFAULT '',
    constraints TEXT NOT NULL DEFAULT '',
    doc TEXT,
    closed_at TEXT,
    verdict TEXT CHECK (verdict IN ('achieved', 'abandoned')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((closed_at IS NULL) = (verdict IS NULL))
);

ALTER TABLE tasks ADD COLUMN goal_id INTEGER REFERENCES goals(id);
ALTER TABLE tasks ADD COLUMN context TEXT NOT NULL DEFAULT '';
CREATE INDEX tasks_by_goal ON tasks(goal_id);

CREATE TABLE run_events_v8 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (task_id IS NOT NULL OR goal_id IS NOT NULL),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v8 (id, task_id, run_id, kind, payload, created_at)
SELECT id, task_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v8 RENAME TO run_events;
CREATE INDEX events_by_task ON run_events(task_id, id);
CREATE INDEX events_by_goal ON run_events(goal_id, id);
