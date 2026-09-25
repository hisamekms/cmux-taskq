-- dagq-schema: breaking
-- The cmux workspace of each resident session role that is not a run or a
-- supervisor registration: the maintainer, the in-cmux supervisor's
-- workspace (which outlives the registration of a crashed supervisor), and
-- later the planner and the inbox. The workspace's title is for people and
-- may be renamed, so `up` finds these workspaces by this UUID alone and
-- checks it against `cmux workspace list`; a UUID cmux no longer lists is
-- deleted and the workspace opened again (ADR-0026).
CREATE TABLE session_workspaces (
  role TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
