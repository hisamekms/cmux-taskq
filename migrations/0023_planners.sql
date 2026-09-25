-- Planner sessions (ADR-0041 decisions 1, 6, 12, 13). A planner is an
-- on-demand cmux workspace, never a resident one: a person opens as many as
-- they like with `dagq plan`, and the runtime opens one for a proposal it
-- sends back or a follow_up draft. `session_workspaces` keys on the role and
-- so holds one workspace per role, which a planner cannot live with; each
-- planner gets its own row here instead. `workspace_id` is the UUID cmux
-- returned (ADR-0026), set once the workspace exists. The session wrapper
-- (`planner-session`) records its pid, its agent's pid, its heartbeat and
-- the agent's exit, as a run's wrapper does, so liveness is judged the way a
-- worker's is. `closed_at` marks a planner that is over (its workspace
-- failed to open, or it is gone); `error` says why when it failed.
CREATE TABLE planners (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    origin TEXT NOT NULL CHECK (origin IN ('person', 'runtime')),
    proposal_id INTEGER REFERENCES proposals(id),
    workspace_id TEXT UNIQUE,
    wrapper_pid INTEGER,
    agent_pid INTEGER,
    heartbeat_at INTEGER,
    exit_code INTEGER,
    exited_at INTEGER,
    closed_at INTEGER,
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX planners_by_proposal ON planners(proposal_id);
