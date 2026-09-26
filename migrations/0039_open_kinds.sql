-- dagq-schema: breaking
-- Kinds are not enumerated in the queue (ADR-0073 decisions 19-23): asks
-- and run_events lose every CHECK that names a kind, so adding an ask or
-- event kind needs no migration from now on, and a binary reads a kind it
-- does not know instead of failing. The rules those CHECKs held move to
-- the write port: only a known kind is written; an ask about no task is a
-- blocked or queue_hold ask about no run (0029); a queue_hold ask, and
-- only it, is for authentication or cost (0029); an event on no task,
-- goal or run is a queue event (0012, last listed in 0036). The CHECKs
-- that name no kind stay. SQLite cannot drop a CHECK, so both tables are
-- rebuilt like 0029 and 0036; ids are preserved, and asks_open, the
-- triggers on run_events and the one on tasks that reads it are created
-- again. Breaking, once: a binary that does not read an unknown kind must
-- not open a queue that may hold one.
CREATE TABLE asks_v39 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (length(kind) > 0),
  task_id INTEGER REFERENCES tasks(id),
  run_id TEXT,
  question TEXT NOT NULL CHECK (length(trim(question)) > 0),
  options TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options) AND json_type(options) = 'array'),
  answer TEXT,
  asked_by TEXT NOT NULL,
  reason_category TEXT NOT NULL CHECK (reason_category IN ('authentication','cost','scope','discard','recovery_failed')),
  subject TEXT,
  affected TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(affected) AND json_type(affected) = 'array'),
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  answered_at INTEGER,
  closed_at INTEGER,
  finding_id INTEGER REFERENCES findings(id),
  answered_by TEXT,
  option_index INTEGER,
  CHECK ((answer IS NULL) = (answered_at IS NULL)),
  CHECK (run_id IS NULL OR task_id IS NOT NULL),
  FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO asks_v39 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      reason_category, subject, affected, created_at, answered_at, closed_at,
                      finding_id, answered_by, option_index)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       reason_category, subject, affected, created_at, answered_at, closed_at,
       finding_id, answered_by, option_index FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v39 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind,
                                      reason_category, ifnull(subject, ''),
                                      ifnull(finding_id, 0))
  WHERE answered_at IS NULL AND closed_at IS NULL;

DROP TRIGGER search_task_moved;
CREATE TABLE run_events_v39 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v39 (id, task_id, goal_id, run_id, kind, payload, created_at)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v39 RENAME TO run_events;
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
