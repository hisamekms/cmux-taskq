-- dagq-schema: breaking
-- How the supervisor behind a registration was started, recorded by `up`
-- once the process has registered itself (ADR-0011): `launchd` for the
-- LaunchAgent that launchd keeps resident, `in_cmux` for the fallback that
-- runs `supervise` inside the cmux workspace `taskq <repo> supervisor`.
-- NULL is a supervisor nobody claimed: one started by hand. `workspace_id`
-- belongs to `in_cmux` alone and names the workspace `down` closes once
-- that supervisor is gone. Both columns describe the registered process, so
-- they live and die with its row.
ALTER TABLE supervisors ADD COLUMN mode TEXT CHECK (mode IN ('launchd', 'in_cmux'));
ALTER TABLE supervisors ADD COLUMN workspace_id TEXT;
