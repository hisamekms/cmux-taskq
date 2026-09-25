-- dagq-schema: breaking
-- Follow-up triage (ADR-0037). A headless job decides each follow_up draft
-- the supervisor hands it; the runtime applies its verdict and a person's
-- answer to its `follow_up` ask.
--
-- task_leases: the lease of a task that has no run (a follow_up draft under
-- its triage), with the freshness rule of run_leases (a heartbeat older
-- than 30 seconds is stale). `reason` names what holds it.
CREATE TABLE task_leases (
  task_id INTEGER PRIMARY KEY REFERENCES tasks(id),
  supervisor_token TEXT NOT NULL,
  reason TEXT NOT NULL,
  heartbeat_at INTEGER NOT NULL
);

-- How many follow-ups in a row were adopted without a person's judgement
-- (ADR-0037 decision 6): 0 for a task a person or the planner registered or
-- readied, the source's depth + 1 for a draft `integrate` registers, the
-- draft's for a task the job adopts. Existing tasks are 0, and a follow_up
-- draft still waiting is 1: nothing was adopted automatically before.
ALTER TABLE tasks ADD COLUMN follow_up_depth INTEGER NOT NULL DEFAULT 0;
UPDATE tasks SET follow_up_depth = 1
WHERE status = 'draft' AND id IN (
  SELECT json_extract(payload, '$.task_id') FROM run_events
  WHERE kind = 'follow_up_registered' AND json_extract(payload, '$.task_id') IS NOT NULL
);

-- The `follow_up` ask a follow-up triage opens about a draft (no run).
-- SQLite cannot change a CHECK, so asks is rebuilt like 0017 with the kind
-- added; ids are preserved.
CREATE TABLE asks_v19 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit','follow_up')),
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
INSERT INTO asks_v19 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v19 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;
