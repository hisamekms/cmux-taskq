-- dagq-schema: breaking
-- Plan review (ADR-0041 decisions 11-15, 17). The supervisor reviews one
-- submitted proposal at a time, queue-wide, with a headless job.
--
-- plan_reviews: one row per job. The partial unique index keeps at most one
-- unfinished row, so two supervisors never review at once; a row whose
-- supervisor is gone is finished as `interrupted` by the next one. `dir` is
-- the job's directory under the queue's plan-reviews/ (its prompt and
-- output), `verdict` the JSON verdict it printed and `error` why it failed.
CREATE TABLE plan_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_id INTEGER NOT NULL REFERENCES proposals(id),
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    supervisor_token TEXT NOT NULL,
    dir TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT CHECK (outcome IS NULL
        OR outcome IN ('pass', 'revise', 'concern', 'failed', 'interrupted')),
    verdict TEXT CHECK (verdict IS NULL OR json_valid(verdict)),
    error TEXT,
    CHECK ((finished_at IS NULL) = (outcome IS NULL))
);
CREATE UNIQUE INDEX plan_reviews_running ON plan_reviews(finished_at IS NULL)
    WHERE finished_at IS NULL;
CREATE INDEX plan_reviews_by_proposal ON plan_reviews(proposal_id, id);

-- Where a proposal stands with plan review beyond its status:
-- review_hold: a submitted proposal plan review does not take again by
--   itself, `failed` (the job failed; a person decides) or `concern` (its
--   `approve_plan` ask waits for a person).
-- revise_reasons: the JSON array of reasons a proposal sent back carries to
--   its planner, waiting since revised_at; revise_sent_at is when the
--   runtime claimed their delivery (NULL: not yet, the supervisor delivers
--   them) and revise_planner_id the planner they went to. unresponsive_at
--   is when the supervisor told the inbox that no planner submitted it again
--   within the planner timeout.
-- Submitting the proposal again clears them all.
ALTER TABLE proposals ADD COLUMN review_hold TEXT
    CHECK (review_hold IS NULL OR review_hold IN ('failed', 'concern'));
ALTER TABLE proposals ADD COLUMN revise_reasons TEXT;
ALTER TABLE proposals ADD COLUMN revised_at INTEGER;
ALTER TABLE proposals ADD COLUMN revise_sent_at INTEGER;
ALTER TABLE proposals ADD COLUMN revise_planner_id INTEGER;
ALTER TABLE proposals ADD COLUMN unresponsive_at INTEGER;

-- The `approve_plan` ask plan review opens (ADR-0041 decision 11), about
-- the first task of the proposal. SQLite cannot change a CHECK, so asks is
-- rebuilt like 0025 with the kind added; ids are preserved.
CREATE TABLE asks_v27 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit','follow_up','stalled','approve_plan')),
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
INSERT INTO asks_v27 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v27 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;
