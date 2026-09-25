-- dagq-schema: breaking
-- Run-level supervisor leases and parallel execution (ADR-0007).
-- A supervisor owns each run it executes through its own lease row; the
-- queue-wide singleton lease and the single execution slot are gone. The
-- migration runner disables foreign keys around this script and runs
-- foreign_key_check before committing.
CREATE TABLE run_leases (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES task_runs(id),
    token TEXT NOT NULL,
    pid INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX leases_by_token ON run_leases(token);
-- A lease held by a v4 supervisor moves to the run it was executing, so an
-- orphaned run keeps refusing recovery until that supervisor is confirmed gone.
INSERT INTO run_leases (run_id, token, pid, heartbeat_at)
SELECT r.id, l.token, l.pid, l.heartbeat_at
FROM task_runs r JOIN supervisor_leases l ON l.token = r.supervisor_token
WHERE r.status IN ('claimed', 'starting', 'running', 'validating');
DROP TABLE supervisor_leases;
DROP INDEX one_executing_run_per_queue;
