-- Add the 'integrated' run status and limit each task to one integrated run.
-- SQLite cannot change a CHECK constraint in place, so task_runs is rebuilt
-- with every column added so far (0002, 0003) and its rowid order preserved.
-- The migration runner disables foreign keys around this script and runs
-- foreign_key_check before committing.
CREATE TABLE task_runs_v4 (
    id TEXT PRIMARY KEY NOT NULL,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    status TEXT NOT NULL CHECK (status IN (
        'claimed', 'starting', 'running', 'validating', 'awaiting_integration',
        'integrated', 'succeeded', 'failed', 'interrupted'
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
    repo_path TEXT,
    run_dir TEXT,
    supervisor_token TEXT,
    last_error TEXT,
    workspace_closed_at INTEGER,
    UNIQUE (id, task_id)
);
INSERT INTO task_runs_v4 (rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at)
SELECT rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at
FROM task_runs;
DROP TABLE task_runs;
ALTER TABLE task_runs_v4 RENAME TO task_runs;
CREATE INDEX runs_by_task ON task_runs(task_id);
-- Awaiting integration is no longer executing but still owns its task.
CREATE UNIQUE INDEX one_unfinished_run_per_task ON task_runs(task_id)
    WHERE status IN ('claimed', 'starting', 'running', 'validating', 'awaiting_integration');
-- Initial dogfooding has exactly one execution slot per database.
CREATE UNIQUE INDEX one_executing_run_per_queue ON task_runs((1))
    WHERE status IN ('claimed', 'starting', 'running', 'validating');
-- A task is completed by exactly one integrated run.
CREATE UNIQUE INDEX one_integrated_run_per_task ON task_runs(task_id)
    WHERE status = 'integrated';
