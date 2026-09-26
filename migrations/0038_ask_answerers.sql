-- dagq-schema: compatible
-- Who answered an ask and which of its options the answer chose (task
-- 325), so that `stats` can count the decisions that reached a person.
-- `answered_by` is `person` (a plain terminal), the `DAGQ_ROLE` of the
-- session that ran `answer` (`inbox`, `planner`), or `runtime` when the
-- runtime answered it itself (a withdrawn, superseded or runtime-closed
-- ask). `option_index` is the 0-based index of the option the trimmed
-- answer equals, NULL for a free answer. Both are NULL on an ask answered
-- before this migration, or by a binary that predates it: unknown.
-- Additions only: an older binary never names the columns, and its
-- updates leave them NULL.
ALTER TABLE asks ADD COLUMN answered_by TEXT;
ALTER TABLE asks ADD COLUMN option_index INTEGER;
