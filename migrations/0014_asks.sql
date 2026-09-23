-- Questions that wait for a person's answer (ADR-0022): the maintainer's
-- or a worker's consultation as a queue row, delivered by `status` and
-- `watch --role`. An ask belongs to a task and, when it is about one run,
-- to that run. It is open until `answer` writes `answer` / `answered_at`;
-- the maintainer marks an answer read with `ask close`, which writes
-- `closed_at` (only an answered ask is closed). One open ask per
-- (task, run, kind): registering it again returns the existing row.
-- Times are unix seconds, like session_workspaces.
CREATE TABLE asks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question')),
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  run_id TEXT,
  question TEXT NOT NULL CHECK (length(trim(question)) > 0),
  options TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options) AND json_type(options) = 'array'),
  answer TEXT,
  asked_by TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  answered_at INTEGER,
  closed_at INTEGER,
  CHECK ((answer IS NULL) = (answered_at IS NULL)),
  FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
CREATE UNIQUE INDEX asks_open ON asks(task_id, ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;
