-- dagq-schema: breaking
-- Merge queue (ADR-0008): 'integrating' holds the single integration slot
-- while the runtime rebases, re-validates and squash-lands a run; 'needs_session'
-- parks a run whose rebase conflicted or whose re-validation failed until a
-- session resolves it. Both still own their task. SQLite cannot change a CHECK
-- constraint in place, so task_runs is rebuilt as in 0004 with its rowid order
-- preserved; the migration runner disables foreign keys around this script and
-- runs foreign_key_check before committing.
CREATE TABLE task_runs_v6 (
    id TEXT PRIMARY KEY NOT NULL,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    status TEXT NOT NULL CHECK (status IN (
        'claimed', 'starting', 'running', 'validating', 'awaiting_integration',
        'integrating', 'needs_session', 'integrated', 'succeeded', 'failed', 'interrupted'
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
INSERT INTO task_runs_v6 (rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at)
SELECT rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at
FROM task_runs;
DROP TABLE task_runs;
ALTER TABLE task_runs_v6 RENAME TO task_runs;
CREATE INDEX runs_by_task ON task_runs(task_id);
-- Integrating and waiting for a session are not executing but still own the task.
CREATE UNIQUE INDEX one_unfinished_run_per_task ON task_runs(task_id)
    WHERE status IN ('claimed', 'starting', 'running', 'validating', 'awaiting_integration',
                     'integrating', 'needs_session');
-- A task is completed by exactly one integrated run.
CREATE UNIQUE INDEX one_integrated_run_per_task ON task_runs(task_id)
    WHERE status = 'integrated';
-- One integration slot per queue: landings are serialized so main stays linear.
CREATE UNIQUE INDEX one_integrating_run_per_queue ON task_runs((1))
    WHERE status = 'integrating';
