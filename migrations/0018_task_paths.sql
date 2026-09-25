-- dagq-schema: breaking
-- Declared paths (ADR-0029): globs of the paths a task's run may change, as
-- a JSON array. Validation parks a run whose diff from its base touches a
-- path none of them matches as `needs_session` (`scope_violation`), and
-- `integrate` refuses to land such a diff after its rebase. Existing tasks
-- declare none, which limits nothing.
ALTER TABLE tasks ADD COLUMN paths TEXT NOT NULL DEFAULT '[]';
