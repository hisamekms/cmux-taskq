-- dagq-schema: compatible
-- The kind of change a task makes (goal 21): docs, plugin, runtime or ci,
-- written by `add --kind` and `edit --kind`. NULL for a task registered
-- without one: the tasks before this migration are not filled in from
-- their titles, whose prefixes are what the kind replaces. The column has
-- no CHECK, so a later kind is an addition too; a binary reads a value it
-- does not know as none. Additions only: an older binary never names the column,
-- and its inserts leave it NULL.
ALTER TABLE tasks ADD COLUMN kind TEXT;
