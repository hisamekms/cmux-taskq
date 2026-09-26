-- dagq-schema: compatible
-- The handoff of a supervisor to another binary without waiting for its
-- sessions (ADR-0045 decision 10). `handoff_accepted` is 1 on the
-- registration of a supervisor that knows the protocol (a supervisor of an
-- older binary leaves it NULL and is drained instead). `handoff_binary` is
-- the binary `up` or `install` asked it to exec: the supervisor picks the
-- request up at its next pause between short steps, execs that binary
-- under its own pid and token, and the new process clears both request
-- columns when it takes the registration back. Like `mode`, they describe
-- the registered process, so they live and die with its row. Nullable
-- additions only: an older binary ignores them.
ALTER TABLE supervisors ADD COLUMN handoff_accepted INTEGER;
ALTER TABLE supervisors ADD COLUMN handoff_binary TEXT;
ALTER TABLE supervisors ADD COLUMN handoff_requested_at INTEGER;
