-- dagq-schema: breaking
-- Resident supervisor registration. A `supervise` process registers itself
-- when it starts, refreshes the row together with its run leases on every
-- heartbeat, and deletes it on a graceful exit. A row whose pid is dead or
-- whose heartbeat is old is reported by status and doctor, never deleted
-- automatically. Leases join this table by token; an `integrate` process
-- holds a lease without a row here.
CREATE TABLE supervisors (
    token TEXT PRIMARY KEY NOT NULL,
    pid INTEGER NOT NULL,
    parallel INTEGER NOT NULL CHECK (parallel >= 1),
    started_at INTEGER NOT NULL DEFAULT (unixepoch()),
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch())
);
