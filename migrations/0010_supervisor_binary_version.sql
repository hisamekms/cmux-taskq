-- The cmux-taskq version behind a registration (`CARGO_PKG_VERSION`),
-- written by the `supervise` process itself when it registers: only that
-- process knows which binary it is. NULL is a supervisor that registered
-- before this column existed, which is by definition not the version of a
-- binary that has it, so `up` replaces such a supervisor like any other
-- mismatch (ADR-0014). Like `mode`, it describes the registered process, so
-- it lives and dies with its row.
ALTER TABLE supervisors ADD COLUMN binary_version TEXT;
