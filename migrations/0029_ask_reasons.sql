-- dagq-schema: breaking
-- Why an ask needs a person (ADR-0047 decisions 41, 42). Every ask carries
-- a `reason_category`: `authentication`, `cost`, `scope`, `discard` or
-- `recovery_failed`. Authentication and cost asks are `queue_hold` asks:
-- about no task or run, one open per reason and `subject`, with the runs
-- they hold in `affected` (a JSON array of run IDs). Existing asks get the
-- reason of their kind: a review's, a plan review's and a planner's
-- concern, a worker's question, the observer's blocked ask and the retired
-- follow_up ask are `scope`, the rest `recovery_failed`. SQLite cannot
-- change a CHECK, so asks is rebuilt like 0028 with the columns added; ids
-- are preserved. Breaking: an older binary writes no reason.
CREATE TABLE asks_v29 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit','follow_up','stalled','approve_plan','planner_question','queue_hold')),
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
  CHECK ((answer IS NULL) = (answered_at IS NULL)),
  CHECK (task_id IS NOT NULL OR (kind IN ('blocked','queue_hold') AND run_id IS NULL)),
  CHECK ((kind = 'queue_hold') = (reason_category IN ('authentication','cost'))),
  FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO asks_v29 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      reason_category, created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       CASE WHEN kind IN ('approve_landing','approve_plan','planner_question',
                          'worker_question','blocked','follow_up')
            THEN 'scope' ELSE 'recovery_failed' END,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v29 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind,
                                      reason_category, ifnull(subject, ''))
  WHERE answered_at IS NULL AND closed_at IS NULL;
