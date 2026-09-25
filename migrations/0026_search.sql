-- dagq-schema: compatible
-- Full-text search (ADR-0046 decisions 1-3). One FTS5 table indexes the
-- four kinds of document `dagq search` reads: tasks (title, description,
-- acceptance, context), goals (title, description, acceptance, constraints
-- in `context`), notes (the `text` of an `observation` run event) and the
-- messages of landed commits (`text`). One table, so bm25 ranks across
-- kinds. The trigram tokenizer matches any substring of three or more
-- characters, which Japanese text without spaces between words needs;
-- shorter terms are matched with LIKE on the same columns.
--
-- The rowid encodes the document: 4*id for a task, 4*id+1 for a goal,
-- 4*event id+2 for a note, 4*landed_commits.id+3 for a landed commit, so a
-- trigger reaches its row without scanning. The unindexed columns carry
-- what search filters on and prints: the status is the task's, the goal's
-- (draft, open, achieved, abandoned) or, for a note or a commit, that of
-- the task or goal it belongs to; goal_id is the goal itself or the task's.
--
-- Triggers keep the index current inside the writing transaction, so a
-- binary that predates this migration still keeps it current. They only
-- write the tables this migration creates, which is why the migration is
-- compatible (ADR-0045 decision 6). A later migration that rebuilds tasks,
-- goals or run_events drops their triggers and must create them again.
CREATE TABLE landed_commits (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL UNIQUE,
    task_id INTEGER NOT NULL,
    commit_sha TEXT NOT NULL,
    message TEXT,
    git_common_dir TEXT,
    landed_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE search_index USING fts5(
    kind UNINDEXED,
    ref UNINDEXED,
    task_id UNINDEXED,
    goal_id UNINDEXED,
    run_id UNINDEXED,
    status UNINDEXED,
    updated_at UNINDEXED,
    title,
    description,
    acceptance,
    context,
    text,
    tokenize = 'trigram'
);

-- The rows already there. `integrate` has put the landed commit's message
-- in the run_integrated payload since it first landed; `migrate` reads a
-- missing one from Git. OR IGNORE, as in the trigger below: a second
-- run_integrated of a run keeps the first.
INSERT OR IGNORE INTO landed_commits (run_id, task_id, commit_sha, message, git_common_dir, landed_at)
SELECT run_id, task_id, json_extract(payload, '$.result_commit'),
       CAST(json_extract(payload, '$.message') AS TEXT),
       json_extract(payload, '$.git_common_dir'), created_at
FROM run_events
WHERE kind = 'run_integrated' AND run_id IS NOT NULL AND task_id IS NOT NULL
  AND json_extract(payload, '$.result_commit') IS NOT NULL
ORDER BY id;

INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                          title, description, acceptance, context, text)
SELECT id * 4, 'task', id, id, goal_id, NULL, status, updated_at,
       title, description, acceptance, context, ''
FROM tasks;

INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                          title, description, acceptance, context, text)
SELECT id * 4 + 1, 'goal', id, NULL, id, NULL,
       CASE WHEN closed_at IS NULL THEN status ELSE verdict END, updated_at,
       title, description, acceptance, constraints, ''
FROM goals;

INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                          title, description, acceptance, context, text)
SELECT e.id * 4 + 2, 'note', e.id, e.task_id, coalesce(e.goal_id, t.goal_id), e.run_id,
       coalesce(t.status, CASE WHEN g.closed_at IS NULL THEN g.status ELSE g.verdict END),
       e.created_at, '', '', '', '', coalesce(CAST(json_extract(e.payload, '$.text') AS TEXT), '')
FROM run_events e
LEFT JOIN tasks t ON t.id = e.task_id
LEFT JOIN goals g ON g.id = e.goal_id
WHERE e.kind = 'observation';

INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                          title, description, acceptance, context, text)
SELECT c.id * 4 + 3, 'commit', c.commit_sha, c.task_id, t.goal_id, c.run_id, t.status,
       c.landed_at, '', '', '', '', coalesce(c.message, '')
FROM landed_commits c
LEFT JOIN tasks t ON t.id = c.task_id;

-- Tasks: every change reindexes the row; a change of status or goal is
-- copied to the task's notes and landed commits (a note recorded with its
-- own goal keeps it).
CREATE TRIGGER search_task_inserted AFTER INSERT ON tasks BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4, 'task', new.id, new.id, new.goal_id, NULL, new.status, new.updated_at,
            new.title, new.description, new.acceptance, new.context, '');
END;

CREATE TRIGGER search_task_updated AFTER UPDATE ON tasks BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4, 'task', new.id, new.id, new.goal_id, NULL, new.status, new.updated_at,
            new.title, new.description, new.acceptance, new.context, '');
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

CREATE TRIGGER search_task_deleted AFTER DELETE ON tasks BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4;
END;

-- Goals: the same, and a goal's own notes follow its status.
CREATE TRIGGER search_goal_inserted AFTER INSERT ON goals BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 1, 'goal', new.id, NULL, new.id, NULL,
            CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END, new.updated_at,
            new.title, new.description, new.acceptance, new.constraints, '');
END;

CREATE TRIGGER search_goal_updated AFTER UPDATE ON goals BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 1;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 1, 'goal', new.id, NULL, new.id, NULL,
            CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END, new.updated_at,
            new.title, new.description, new.acceptance, new.constraints, '');
    UPDATE search_index
    SET status = CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END
    WHERE kind = 'note' AND task_id IS NULL AND goal_id = new.id;
END;

CREATE TRIGGER search_goal_deleted AFTER DELETE ON goals BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 1;
END;

-- Notes are observation run events; a landing is recorded in
-- landed_commits in the transaction that records run_integrated.
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

-- OR IGNORE: a payload missing a column this table needs skips the row
-- rather than failing the landing.
CREATE TRIGGER search_run_integrated AFTER INSERT ON run_events
WHEN new.kind = 'run_integrated' BEGIN
    INSERT OR IGNORE INTO landed_commits (run_id, task_id, commit_sha, message, git_common_dir,
                                          landed_at)
    VALUES (new.run_id, new.task_id, json_extract(new.payload, '$.result_commit'),
            CAST(json_extract(new.payload, '$.message') AS TEXT),
            json_extract(new.payload, '$.git_common_dir'),
            new.created_at);
END;

CREATE TRIGGER search_commit_inserted AFTER INSERT ON landed_commits BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 3, 'commit', new.commit_sha, new.task_id,
            (SELECT goal_id FROM tasks WHERE id = new.task_id), new.run_id,
            (SELECT status FROM tasks WHERE id = new.task_id), new.landed_at,
            '', '', '', '', coalesce(new.message, ''));
END;

CREATE TRIGGER search_commit_updated AFTER UPDATE ON landed_commits BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 3;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 3, 'commit', new.commit_sha, new.task_id,
            (SELECT goal_id FROM tasks WHERE id = new.task_id), new.run_id,
            (SELECT status FROM tasks WHERE id = new.task_id), new.landed_at,
            '', '', '', '', coalesce(new.message, ''));
END;
