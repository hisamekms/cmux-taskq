-- dagq-schema: compatible
-- The limit on the runs a supervisor keeps waiting for a person outside its
-- slots (ADR-0062 decision 7): `supervise --max-waiting N`, written by the
-- supervisor when it registers or takes its registration back after an
-- exec, next to `parallel`. NULL is a supervisor of an older binary, which
-- keeps every run in its slots. A nullable addition only: an older binary
-- ignores it.
ALTER TABLE supervisors ADD COLUMN max_waiting INTEGER;
