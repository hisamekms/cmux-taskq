# Closing a goal

A goal is closed once, by the planner (the `dagq-planner` skill), after reviewing it; the runtime never closes it. When `goal show ID` lists every task as `completed` (or `canceled`):

1. Look for `draft` tasks in `goal show ID`. `integrate` registered each landed receipt's `follow_ups` as a draft task on the goal (its context reads "task <id>（<title>）の run <run-id> の receipt が提案した follow_up"; the run's `follow_up_registered` events list them). Every draft blocks `achieved`: ask the user whether each becomes `ready` (the goal stays open until it is completed) or is canceled. A run integrated by a binary older than this registration has no `follow_up_registered` events, and a `follow_up_registered` with `task_id: null` is an entry that was not registered: read those runs' receipt `follow_ups` (step 2) and treat them as gaps.
2. For each completed task, read the `summary` of the receipt of its integrated run in `show TASK --full` (the `receipt` in the run's last `integration_receipt` event; `validation_finished` holds the receipt seen before landing) and compare what landed against the goal's `acceptance`. Anything the acceptance asks for that no task delivered, and no draft from step 1 covers, is a gap in the decomposition.
3. Register each gap as a task on the same goal with `add --goal ID`, then `ready` it, and report to the user that the goal stays open; close it after those tasks and the drafts made ready are completed. Do not close first: a closed goal refuses new tasks (a follow-up of a task of a closed goal is registered without a goal).
4. When nothing is missing, record the verdict:

```sh
"$DAGQ" goal close ID --verdict achieved
```

`achieved` is refused while any task is `draft`, `ready` or `in_progress` (the error names the count and status); cancel or finish them first. `abandoned` records that the goal is given up: it is refused while a task is `in_progress`, and it does not cancel the goal's `draft` or `ready` tasks, so cancel them yourself first or the supervisor still runs them. Both verdicts are final; further work on the same problem is a new goal. `goal show ID` afterwards has `closed: true` at the top level, `verdict` and `closed_at` inside `goal`, and a `goal_closed` event with the task counts at close time.

Report a goal to the user as: its title and verdict (or open), its task counts from `goal list`, which tasks are `in_progress` or blocked, and, once every task is completed, whether the acceptance is met, which drafts from follow-ups await the user's decision, and which tasks you registered.
