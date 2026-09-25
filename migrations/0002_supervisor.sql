-- dagq-schema: breaking
ALTER TABLE task_runs ADD COLUMN repo_path TEXT;
ALTER TABLE task_runs ADD COLUMN run_dir TEXT;
ALTER TABLE task_runs ADD COLUMN supervisor_token TEXT;
ALTER TABLE task_runs ADD COLUMN last_error TEXT;

CREATE TABLE queue_repository (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    git_common_dir TEXT NOT NULL
);

CREATE TABLE supervisor_leases (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    token TEXT NOT NULL UNIQUE,
    pid INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE run_processes (
    run_id TEXT NOT NULL REFERENCES task_runs(id),
    role TEXT NOT NULL CHECK (role IN ('wrapper', 'agent')),
    pid INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch()),
    exited_at INTEGER,
    exit_code INTEGER,
    PRIMARY KEY (run_id, role)
);
