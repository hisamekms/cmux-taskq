-- dagq-schema: breaking
-- Drafts the runtime opens a planner for (ADR-0041 decision 16, widened by
-- the person's decision of 2026-09-25 to every draft the runtime or a job
-- makes). This replaces the follow-up triage job of ADR-0037.
--
-- draft_origins: where a draft the runtime or a job registered came from.
-- `origin` is `follow_up` (`integrate` registered it from a landed
-- receipt's follow_ups) or `goal_gap` (a goal's judgement found a gap);
-- `material` is what the planner is shown about it (for a follow_up the
-- source task and run, for a goal_gap the goal and the findings). A draft
-- a person registers with `add` has no row, and no planner is opened for
-- it. Follow_up drafts registered before this migration get their row
-- from their `follow_up_registered` event.
CREATE TABLE draft_origins (
    task_id INTEGER PRIMARY KEY REFERENCES tasks(id),
    origin TEXT NOT NULL CHECK (origin IN ('follow_up', 'goal_gap')),
    material TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(material) AND json_type(material) = 'object'),
    created_at INTEGER NOT NULL
);
INSERT OR IGNORE INTO draft_origins (task_id, origin, material, created_at)
SELECT json_extract(e.payload, '$.task_id'), 'follow_up',
       json_object('source_task_id', e.task_id, 'source_run_id', e.run_id,
                   'index', json_extract(e.payload, '$.index')),
       unixepoch()
FROM run_events e
WHERE e.kind = 'follow_up_registered'
  AND json_extract(e.payload, '$.task_id') IS NOT NULL
  AND EXISTS (SELECT 1 FROM tasks t WHERE t.id = json_extract(e.payload, '$.task_id'))
ORDER BY e.id;

-- The draft a planner of the runtime's was opened for (NULL for a
-- person's planner and for one opened for a proposal's revise).
ALTER TABLE planners ADD COLUMN draft_task_id INTEGER REFERENCES tasks(id);
CREATE INDEX planners_by_draft ON planners(draft_task_id);

-- The follow-up triage job's task leases are gone with the job.
DROP TABLE task_leases;

-- The `planner_question` ask a planner of the runtime's opens when it
-- needs a person (ADR-0041 decision 13); its answer is typed into that
-- planner's workspace. `follow_up` stays for the asks the retired triage
-- may have left. SQLite cannot change a CHECK, so asks is rebuilt like
-- 0027 with the kind added; ids are preserved.
CREATE TABLE asks_v28 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL CHECK (kind IN ('approve_landing','answer_prompt','decide','worker_question','blocked','stuck_exit','follow_up','stalled','approve_plan','planner_question')),
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
INSERT INTO asks_v28 (id, kind, task_id, run_id, question, options, answer, asked_by,
                      created_at, answered_at, closed_at)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by,
       created_at, answered_at, closed_at FROM asks;
DROP TABLE asks;
ALTER TABLE asks_v28 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind)
  WHERE answered_at IS NULL AND closed_at IS NULL;
