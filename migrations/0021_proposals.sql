-- Proposals and the submitted task status (ADR-0041 decisions 7, 8). A
-- proposal bundles goals and tasks a planner submits for plan review and
-- names the planner that owns it: its cmux workspace and whether a person
-- or the runtime opened it. A task or goal belongs to one proposal at a
-- time (`proposal_id`); only plan review, a person's bypass and a retry
-- make a task ready.
CREATE TABLE proposals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    status TEXT NOT NULL CHECK (status IN ('submitted', 'revising', 'accepted', 'canceled')),
    owner_origin TEXT NOT NULL CHECK (owner_origin IN ('person', 'runtime')),
    owner_workspace_id TEXT,
    submitted_at TEXT NOT NULL,
    revise_count INTEGER NOT NULL DEFAULT 0 CHECK (revise_count >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
-- Plan review takes the submitted proposals oldest first (decision 15).
CREATE INDEX proposals_by_status ON proposals(status, submitted_at);

ALTER TABLE goals ADD COLUMN proposal_id INTEGER REFERENCES proposals(id);

-- SQLite cannot change a CHECK constraint in place, so tasks is rebuilt
-- with every column added so far (0008, 0015, 0018, 0020) and its rowid
-- order and AUTOINCREMENT high-water mark preserved. The migration runner
-- disables foreign keys around this script and runs foreign_key_check
-- before committing. Existing drafts stay drafts (nothing is submitted).
CREATE TEMP TABLE tasks_sequence AS
    SELECT seq FROM sqlite_sequence WHERE name = 'tasks';
CREATE TABLE tasks_v21 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    description TEXT NOT NULL,
    acceptance TEXT NOT NULL,
    verification_commands TEXT NOT NULL CHECK (json_valid(verification_commands)),
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'submitted', 'ready', 'in_progress', 'completed', 'canceled')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    goal_id INTEGER REFERENCES goals(id),
    context TEXT NOT NULL DEFAULT '',
    required_evidence TEXT NOT NULL DEFAULT '[]',
    paths TEXT NOT NULL DEFAULT '[]',
    priority INTEGER NOT NULL DEFAULT 1 CHECK (priority BETWEEN 0 AND 4),
    proposal_id INTEGER REFERENCES proposals(id)
);
INSERT INTO tasks_v21 (rowid, id, title, description, acceptance, verification_commands, status,
    created_at, updated_at, goal_id, context, required_evidence, paths, priority)
SELECT rowid, id, title, description, acceptance, verification_commands, status,
    created_at, updated_at, goal_id, context, required_evidence, paths, priority
FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_v21 RENAME TO tasks;
DELETE FROM sqlite_sequence WHERE name = 'tasks';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'tasks', max(coalesce((SELECT seq FROM tasks_sequence), 0),
                        coalesce((SELECT max(id) FROM tasks), 0))
    WHERE EXISTS (SELECT 1 FROM tasks_sequence) OR EXISTS (SELECT 1 FROM tasks);
DROP TABLE tasks_sequence;
CREATE INDEX tasks_by_goal ON tasks(goal_id);
CREATE INDEX tasks_by_proposal ON tasks(proposal_id);
