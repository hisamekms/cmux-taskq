-- dagq-schema: breaking
-- Findings (ADR-0044 decision 18): what the observer found wrong, one row
-- per problem. The same kind, target and subject is one problem, so an
-- unsettled (open or proposed) finding is unique on them; recording it
-- again adds an occurrence and its evidence (run event IDs, a JSON array)
-- to the row. A run target keeps its task too; the other IDs are set only
-- for their own target. Times are unix seconds.
CREATE TABLE findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 64),
    target TEXT NOT NULL CHECK (target IN ('queue', 'goal', 'task', 'run')),
    task_id INTEGER REFERENCES tasks(id),
    run_id TEXT,
    goal_id INTEGER REFERENCES goals(id),
    subject TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL CHECK (length(trim(summary)) > 0),
    detail TEXT NOT NULL DEFAULT '',
    impact TEXT NOT NULL DEFAULT 'normal' CHECK (impact IN ('high', 'normal', 'low')),
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    occurrences INTEGER NOT NULL DEFAULT 1 CHECK (occurrences >= 1),
    evidence TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(evidence) AND json_type(evidence) = 'array'),
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'proposed', 'resolved', 'dismissed')),
    status_reason TEXT,
    proposal_id INTEGER REFERENCES proposals(id),
    propose_reason TEXT,
    propose_requested_at INTEGER,
    recorded_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (CASE target
        WHEN 'queue' THEN task_id IS NULL AND run_id IS NULL AND goal_id IS NULL
        WHEN 'goal' THEN task_id IS NULL AND run_id IS NULL AND goal_id IS NOT NULL
        WHEN 'task' THEN task_id IS NOT NULL AND run_id IS NULL AND goal_id IS NULL
        ELSE task_id IS NOT NULL AND run_id IS NOT NULL AND goal_id IS NULL
    END),
    CHECK ((propose_reason IS NULL) = (propose_requested_at IS NULL)),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
CREATE UNIQUE INDEX findings_unsettled ON findings(
    kind, target, ifnull(task_id, 0), ifnull(run_id, ''), ifnull(goal_id, 0), subject)
  WHERE status IN ('open', 'proposed');
CREATE INDEX findings_by_status ON findings(status, id);

-- A blocked ask raises its finding (ADR-0044 decision 23), and the
-- one-open-ask rule of 0029 holds per finding too: a task-less blocked ask
-- is no longer one per queue.
ALTER TABLE asks ADD COLUMN finding_id INTEGER REFERENCES findings(id);
DROP INDEX asks_open;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind,
                                      reason_category, ifnull(subject, ''),
                                      ifnull(finding_id, 0))
  WHERE answered_at IS NULL AND closed_at IS NULL;

-- A finding on the queue records its events on no task, goal or run, so
-- run_events is rebuilt like 0025 with the finding kinds added; ids are
-- preserved, and the search triggers of 0026 on it, and the one on tasks
-- that reads it, are created again.
DROP TRIGGER search_task_moved;
CREATE TABLE run_events_v29 (
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
        'finding_recorded', 'finding_updated', 'finding_status_changed')),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v29 (id, task_id, goal_id, run_id, kind, payload, created_at)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v29 RENAME TO run_events;
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
