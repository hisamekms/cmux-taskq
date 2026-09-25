-- dagq-schema: breaking
-- The `stalled` ask (ADR-0043 decisions 1 and 3): the supervisor raises a
-- worker's session that stays idle without a receipt after its one nudge to
-- the inbox. SQLite cannot change a CHECK, so asks is rebuilt like 0022 with
-- the kind added; ids are preserved. Breaking: an older binary cannot read
-- an ask of this kind.
CREATE TABLE asks_v24 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit','follow_up','stalled')),
  task_id INTEGER REFERENCES tasks(id),
  run_id TEXT,
  question TEXT NOT NULL CHECK (length(trim(question)) > 0),
  options TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options) AND json_type(options) = 'array'),
  answer TEXT,
  asked_by TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  answered_at INTEGER,
  closed_at INTEGER,
  CHECK ((answer IS NULL) = (answered_at IS NULL)),
  CHECK (task_id IS NOT NULL OR (kind = 'blocked' AND run_id IS NULL)),
  FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO asks_v24 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v24 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;

-- The supervisor records the thresholds it loaded (ADR-0043 decision 4) as a
-- `stall_config_loaded` event of the queue itself, on no task, goal or run,
-- so run_events is rebuilt like 0016 with the kind added.
CREATE TABLE run_events_v24 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (task_id IS NOT NULL OR goal_id IS NOT NULL OR kind IN (
        'backend_call_failed', 'observe_started', 'observe_finished',
        'ask_opened', 'ask_answered', 'stall_config_loaded')),
    CHECK (run_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v24 (id, task_id, goal_id, run_id, kind, payload, created_at)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events;
DROP TABLE run_events;
ALTER TABLE run_events_v24 RENAME TO run_events;
CREATE INDEX events_by_task ON run_events(task_id, id);
CREATE INDEX events_by_goal ON run_events(goal_id, id);
CREATE INDEX events_by_kind ON run_events(kind, id);
