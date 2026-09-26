-- dagq-schema: compatible
-- The automatic update of the fixed binary (ADR-0045 decision 17).
-- `supervisors.auto_update` is 1 on the registration of a supervisor that
-- builds and installs the runtime of every landing that changes it: written
-- by `up --auto-update` (and cleared by a plain `up`) like `mode`, and by a
-- `supervise --auto-update` when it registers. An exec'd supervisor keeps
-- the row, and so the setting.
-- `binary_updates` is the log of those updates, one row per step, oldest
-- first: `kind` is update_started, update_built, update_installed,
-- update_failed, update_awaiting_approval, update_answered or
-- update_retry; `commit_sha` the main commit the step is about; `payload`
-- its details (the pid of the process that works on it, the log, the build
-- identifiers, the error). The run_events kinds are fixed by a CHECK an
-- older binary relies on, so the update keeps its own table. Additions
-- only: an older binary ignores both.
ALTER TABLE supervisors ADD COLUMN auto_update INTEGER;
CREATE TABLE binary_updates (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  commit_sha TEXT,
  payload TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload)),
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
