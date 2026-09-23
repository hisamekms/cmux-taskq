-- Draft goals (ADR-0024 decision 5): a goal is `draft` or `open`. The tasks
-- of a draft goal are not candidates, so the supervisor does not claim them
-- until `goal ready` opens the goal. Existing goals are open. Closing stays
-- in closed_at / verdict, independent of the status. Notes need no schema:
-- they are run_events of kind `observation`.
ALTER TABLE goals ADD COLUMN status TEXT NOT NULL DEFAULT 'open'
    CHECK (status IN ('draft', 'open'));
