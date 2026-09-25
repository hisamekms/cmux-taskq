-- dagq-schema: breaking
-- The oldest binary schema this queue accepts (ADR-0045 decision 7). A
-- binary knowing schema S opens a queue at any user_version as long as S is
-- at least `floor`, so a later compatible migration (declared on the first
-- line of its file) leaves older binaries and the run wrappers they copied
-- at claim time working. `migrate` writes the row in the transaction that
-- sets user_version: the version of the last breaking migration applied.
-- This migration is itself breaking: binaries before it reject any newer
-- user_version, so they cannot honor the floor.
CREATE TABLE schema_floor (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    floor INTEGER NOT NULL CHECK (floor >= 1)
);
