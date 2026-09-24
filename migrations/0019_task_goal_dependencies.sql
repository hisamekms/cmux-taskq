-- Goal dependencies (ADR-0038): a task waits for a goal to be closed as
-- achieved, not only for predecessor tasks to complete. The edges live
-- beside task_dependencies, which this migration leaves untouched; a task
-- never depends on its own goal (checked by the domain, not a CHECK, since
-- the goal is a column of the task).
CREATE TABLE task_goal_dependencies (
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    goal_id INTEGER NOT NULL REFERENCES goals(id),
    PRIMARY KEY (task_id, goal_id)
);
CREATE INDEX goal_dependencies_by_goal ON task_goal_dependencies(goal_id);
