-- dagq-schema: breaking
-- NULL means the cmux workspace is still open (or its close was never confirmed).
ALTER TABLE task_runs ADD COLUMN workspace_closed_at INTEGER;
