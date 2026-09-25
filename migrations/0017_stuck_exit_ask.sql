-- dagq-schema: breaking
-- The supervisor raises a session that did not answer `/exit` within the
-- exit timeout (`exit_request_timed_out`) as a `stuck_exit` ask to the inbox,
-- the only way the runtime tells a person (ADR-0022 decision 5), and closes
-- it itself once the session exits. SQLite cannot change a CHECK, so asks is
-- rebuilt like 0016 with the kind added; ids are preserved.
CREATE TABLE asks_v17 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit')),
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
INSERT INTO asks_v17 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v17 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;
