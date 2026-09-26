-- dagq-schema: breaking
-- Change marks (ADR-0051 decisions 10-13): the supervisor records its
-- start and handoff (`supervisor_started`) and its stop
-- (`supervisor_stopped`), and a change of the normalized hash of the main
-- checkout's `[run.env]` (`run_env_changed`); a person or a planner records
-- a mark (`mark_recorded`) and its retraction (`mark_retracted`) with
-- `dagq mark`. All five are on the queue itself, on no task, goal or run.
-- SQLite cannot change a CHECK, so run_events is rebuilt like 0035 with the
-- five kinds added; ids are preserved, and the triggers on it, and the one
-- on tasks that reads it, are created again. Breaking: an older binary
-- cannot write a queue whose events it would reject.
DROP TRIGGER search_task_moved;
CREATE TABLE run_events_v36 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (task_id IS NOT NULL OR goal_id IS NOT NULL OR kind IN (
        'backend_call_failed', 'observe_started', 'observe_finished',
        'ask_opened', 'ask_answered', 'stall_config_loaded',
        'finding_recorded', 'finding_updated', 'finding_status_changed',
        'run_env_program_missing', 'run_env_program_found',
        'session_opened', 'session_closed', 'session_turns',
        'supervisor_started', 'supervisor_stopped', 'run_env_changed',
        'mark_recorded', 'mark_retracted')),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v36 (id, task_id, goal_id, run_id, kind, payload, created_at)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v36 RENAME TO run_events;
CREATE INDEX events_by_task ON run_events(task_id, id);
CREATE INDEX events_by_goal ON run_events(goal_id, id);
CREATE INDEX events_by_kind ON run_events(kind, id);

CREATE TRIGGER search_note_inserted AFTER INSERT ON run_events
WHEN new.kind = 'observation' BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 2, 'note', new.id, new.task_id,
            coalesce(new.goal_id, (SELECT goal_id FROM tasks WHERE id = new.task_id)), new.run_id,
            coalesce((SELECT status FROM tasks WHERE id = new.task_id),
                     (SELECT CASE WHEN closed_at IS NULL THEN status ELSE verdict END
                      FROM goals WHERE id = new.goal_id)),
            new.created_at, '', '', '', '', coalesce(CAST(json_extract(new.payload, '$.text') AS TEXT), ''));
END;

CREATE TRIGGER search_note_deleted AFTER DELETE ON run_events
WHEN old.kind = 'observation' BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 2;
END;

CREATE TRIGGER search_run_integrated AFTER INSERT ON run_events
WHEN new.kind = 'run_integrated' BEGIN
    INSERT OR IGNORE INTO landed_commits (run_id, task_id, commit_sha, message, git_common_dir,
                                          landed_at)
    VALUES (new.run_id, new.task_id, json_extract(new.payload, '$.result_commit'),
            CAST(json_extract(new.payload, '$.message') AS TEXT),
            json_extract(new.payload, '$.git_common_dir'),
            new.created_at);
END;

CREATE TRIGGER search_task_moved AFTER UPDATE OF status, goal_id ON tasks
WHEN old.status IS NOT new.status OR old.goal_id IS NOT new.goal_id BEGIN
    UPDATE search_index SET status = new.status, goal_id = new.goal_id
    WHERE task_id = new.id AND kind = 'commit';
    UPDATE search_index
    SET status = new.status,
        goal_id = coalesce((SELECT e.goal_id FROM run_events e WHERE e.id = search_index.ref),
                           new.goal_id)
    WHERE task_id = new.id AND kind = 'note';
END;
