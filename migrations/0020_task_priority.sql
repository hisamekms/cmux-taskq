-- Task priority (ADR-0040 decision 4): how urgently a person wants a task
-- claimed, low=0, normal=1, high=2, urgent=3, interrupt=4. The CLI and the
-- JSON use the names; the claim order compares the effective priority
-- first. Existing tasks are normal.
ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 1
    CHECK (priority BETWEEN 0 AND 4);
