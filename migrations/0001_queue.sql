-- dagq-schema: breaking
CREATE TABLE tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    description TEXT NOT NULL,
    acceptance TEXT NOT NULL,
    verification_commands TEXT NOT NULL CHECK (json_valid(verification_commands)),
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'ready', 'in_progress', 'completed', 'canceled')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE task_dependencies (
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    predecessor_id INTEGER NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, predecessor_id),
    CHECK (task_id <> predecessor_id)
);
CREATE INDEX dependencies_by_predecessor ON task_dependencies(predecessor_id);

CREATE TABLE task_runs (
    id TEXT PRIMARY KEY NOT NULL,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    status TEXT NOT NULL CHECK (status IN (
        'claimed', 'starting', 'running', 'validating', 'awaiting_integration',
        'succeeded', 'failed', 'interrupted'
    )),
    requested_provider TEXT NOT NULL CHECK (requested_provider = 'claude'),
    actual_provider TEXT NOT NULL CHECK (actual_provider = 'claude'),
    base_commit TEXT NOT NULL,
    branch TEXT,
    worktree_path TEXT,
    workspace_id TEXT,
    receipt_path TEXT,
    log_path TEXT,
    result_commit TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (id, task_id)
);
CREATE INDEX runs_by_task ON task_runs(task_id);
-- Awaiting integration is no longer executing but still owns its task.
CREATE UNIQUE INDEX one_unfinished_run_per_task ON task_runs(task_id)
    WHERE status IN ('claimed', 'starting', 'running', 'validating', 'awaiting_integration');
-- Initial dogfooding has exactly one execution slot per database.
CREATE UNIQUE INDEX one_executing_run_per_queue ON task_runs((1))
    WHERE status IN ('claimed', 'starting', 'running', 'validating');

CREATE TABLE run_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
CREATE INDEX events_by_task ON run_events(task_id, id);
