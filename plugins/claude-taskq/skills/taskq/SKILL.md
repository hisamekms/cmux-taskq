---
name: taskq
description: Register and inspect cmux-taskq goals and tasks through the locally built cmux-taskq binary. Use when the user brings a development problem to queue for cmux-taskq (register it as a goal, decompose it into tasks with title, description, acceptance criteria, verification commands, dependencies and context), make tasks ready, list goals or tasks, check a goal's progress or a task's status or run result, close a goal after reviewing its tasks' receipts and follow_ups, or find the cmux-taskq binary and queue database.
---

# cmux-taskq: register and inspect tasks

cmux-taskq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `cmux-taskq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

A goal is the problem several tasks solve together; a task is one unit of work a session executes in its own worktree. Register the goal first, then its tasks. Starting a run, landing it on `main` with `integrate`, and resuming a `needs_session` run are in the `taskq-run` skill; recovering an interrupted run is in `taskq-recover`.

## 1. Locate the binary and the queue

Run the plugin launcher, which resolves the binary and forwards every command to it from the current directory:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/taskq" --resolve
```

It prints `{"binary", "version", "repo", "db", "db_exists", "queue_dir", "runs_dir", "source", "git_common_dir"}`.

- Binary: `CMUX_TASKQ_BIN` if set, otherwise `cmux-taskq` on PATH. If the launcher prints an error instead, tell the user to build it in the cmux-taskq repository with `cargo build --locked` and either put `target/debug/cmux-taskq` on PATH or `export CMUX_TASKQ_BIN=/absolute/path/to/cmux-taskq`, then retry.
- Queue: one per repository. The binary resolves it from the current directory's Git common directory to `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db` (default `~/.local/share/cmux-taskq/<hash>/queue.db`); `source` is `repository`. Every worktree of the repository, including task worktrees, resolves to the same queue, and runs live in `runs_dir` next to it. Run the launcher from inside the repository the tasks belong to; outside a repository it fails. `CMUX_TASKQ_DB=/path/to/queue.db` uses another queue file instead (`source` becomes `db_flag`; the launcher passes it as `--db`).
- If `db_exists` is false, create the queue once: `"${CLAUDE_PLUGIN_ROOT}/bin/taskq" init` (it creates the directory, binds the queue to this repository, and also migrates an existing queue while keeping its tasks). A queue bound to a different repository is refused by every command; that only happens with `CMUX_TASKQ_DB`, so unset it or point it at the right file.

Report the version and the database path to the user the first time in a session. Use `TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"` below.

## 2. Register a goal and decompose it into tasks

The standard procedure is: hear the problem → register it as a goal with `goal add` → decompose it into tasks and register each with `add --goal` → make them `ready`. Every task of a goal is shown the goal's description, acceptance and constraints in its prompt, together with the receipt summary and landed commit of the tasks it depends on and the titles of the goal's other tasks in progress, so sibling tasks make the same naming and boundary decisions and stay out of each other's scope.

### When a task may have no goal

Skip the goal only for a one-shot task: a single task that finishes the problem by itself and has no sibling to align decisions with, such as a typo fix, resolving a clippy warning, bumping a version, or correcting one document. The test is: if a second task will exist, or a later task would need to know what this one decided (a name, a module boundary, a format), register a goal. A task without a goal gets `Goal: none, this task stands alone` in its prompt and behaves as before. When unsure, register the goal; a goal with one task costs one command, a missing goal costs a mismatched integration.

### Register the goal

Collect from the user, asking only for what is missing:

- title (required, one line): the problem, not the first task
- description: what is wrong or missing today and what the repository looks like when it is solved
- acceptance: how the maintainer decides the whole goal is achieved after every task landed; this is what the tasks' receipts are reviewed against at close time
- constraints: naming, module or file boundaries, and what not to do, shared by every task (for example "the entity is called `Goal`, not `Objective`; do not change the schema; do not touch the plugin"); leave it out when there is nothing to align
- doc: the path of a reference document inside the repository (an ADR, a design document, a plan), relative to the repository root; the worker is told the path and reads it in its worktree, so it must be committed

```sh
"$TASKQ" goal add "TITLE" \
  --description "DESCRIPTION" \
  --acceptance "ACCEPTANCE" \
  --constraints "CONSTRAINTS" \
  --doc docs/adr/0009-goal-groups-tasks.md
```

The JSON is the goal (`id`, `title`, `description`, `acceptance`, `constraints`, `doc`, `verdict: null`, `closed_at: null`). A goal has no state machine and no verification commands: its progress is derived from its tasks' statuses, and a goal-level check belongs in a final task that depends on all the others.

### Decompose and register the tasks

Split the goal into tasks a single session can finish in one worktree, each with its own acceptance, and give them dependencies where one must land before another starts (dependencies may cross goals). Collect per task:

- title (required, one line)
- description: what to change and where
- acceptance: how a reviewer decides this task is done
- verification commands: shell lines the supervisor reruns in the worktree after the session (for example `cargo test --locked`); repeat `--verify`
- dependencies: task IDs that must be `completed` first; repeat `--depends-on`
- `--goal`: the open goal's `id` from `goal add`; a closed goal is refused
- `--context`: why this task exists and what to read first, when the goal's description does not already say it (a journal, an issue, a failing command); it is shown to the worker whether or not the task has a goal

```sh
"$TASKQ" add "TITLE" \
  --goal 1 \
  --description "DESCRIPTION" \
  --acceptance "ACCEPTANCE" \
  --context "CONTEXT" \
  --verify "cargo test --locked" --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --depends-on 3
```

A one-shot task omits `--goal`. The task is registered as `draft` and the JSON includes its `id` and `goal_id`. Then make it runnable and confirm it is a candidate:

```sh
"$TASKQ" ready ID
"$TASKQ" candidates
```

`candidates` lists ready tasks whose dependencies are all `completed`, in registration order, without reserving them; it does not prefer one goal over another. A ready task missing from `candidates` is blocked by a dependency; show the blocking IDs from `show ID`'s `dependencies`. Use `draft ID` to take a task back for editing, `cancel ID` to drop it (its dependents are not satisfied), and `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` to change prerequisites of a draft or ready task. Self-dependencies and cycles are rejected.

### Change a goal or a task's goal

```sh
"$TASKQ" goal list                         # every goal with its task counts by status
"$TASKQ" goal show ID                      # the goal, its tasks (id, title, status), its events
"$TASKQ" set-goal TASK GOAL                # move a draft or ready task into an open goal
"$TASKQ" set-goal TASK --none              # take a draft or ready task out of its goal
"$TASKQ" goal edit ID --constraints "..."  # replace one or more fields (--title, --description, --acceptance, --constraints, --doc; --doc "" clears it)
"$TASKQ" goal close ID --verdict achieved  # or abandoned; see section 4
```

`set-goal` follows the dependency rules: only a `draft` or `ready` task can be moved, and a closed goal accepts no task. Take an `in_progress` task back with `draft ID` only if its run is finished; a `completed` task keeps the goal it landed with. `goal edit` records the old and new fields in a `goal_updated` event; a run already claimed keeps the prompt it started with, and runs claimed afterwards see the new text.

## 3. Inspect

| Command | Use |
| --- | --- |
| `"$TASKQ" goal list` | All goals with `id`, `title`, `closed`, `verdict`, and `tasks` counts (`total`, `draft`, `ready`, `in_progress`, `completed`, `canceled`) |
| `"$TASKQ" goal show ID` | `goal` (all fields), `tasks` (`id`, `title`, `status`), `closed`, `events` (`goal_created`, `goal_updated`, `goal_closed`) |
| `"$TASKQ" list` | All tasks with `id`, `title`, `status`, `goal_id` |
| `"$TASKQ" show ID` | `task` (with `goal_id` and `context`), `dependencies`, `runs`, `events`, `processes` |
| `"$TASKQ" candidates` | What the next `supervise` would pick |
| `"$TASKQ" locate` | The queue this directory resolves to (`db`, `runs_dir`, `git_common_dir`, `db_exists`) without opening it |
| `"$TASKQ" status` | `supervisors`: every registered `supervise` process (`pid`, `alive`, `parallel`, `heartbeat_age_secs`, `stale`, `run_ids`; listed even while it holds no run) plus any `integrate` process holding a lease (`registered: false`); `runs`: unfinished runs with their leases |
| `"$TASKQ" doctor` | The same `supervisors`; per run: lease liveness, processes, worktree/receipt existence, `blockers`, `recoverable` |

Task `status`: `draft` → `ready` → `in_progress` → `completed`, or `canceled`. A task stays `in_progress` while any run is unfinished or awaiting integration.

Run `status` in `runs` (latest last): `claimed`, `starting`, `running`, `validating` are unfinished; `awaiting_integration` means the receipt and verification passed and the run waits for `integrate` to land it on `main`; `integrating` means an `integrate` process is landing it right now; `needs_session` means the landing hit a rebase conflict or a failed verification and a resumed session must fix it (`last_error` says what); `integrated` means the run was squashed onto `main` (`result_commit` is the landed commit) and the task is `completed`; `failed` and `interrupted` keep their worktree and workspace for inspection, with the reason in `last_error`. Dependency-free tasks run in parallel (up to the supervisor's `--parallel`), each in its own workspace and worktree; a dependent task waits until every predecessor is `completed`.

Useful run fields: `branch` (`taskq/<run-id>`), `worktree_path`, `workspace_id` (cmux), `run_dir` (prompt, logs, `receipt.json`, `verify-N.log`), `receipt_path`, `result_commit`, `last_error`, `workspace_closed_at`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. Neither a Stop hook firing, an idle session, nor the receipt file's existence means success; the supervisor validates the receipt against Git and the verification commands before a run becomes `awaiting_integration`. Summarize for the user: task status, latest run status, branch and commit to review, and the next step (`taskq-run` to start or integrate, `taskq-recover` if the run is stuck).

### Close a goal

A goal is closed once, by the maintainer, after reviewing it; the runtime never closes it. When `goal show ID` lists every task as `completed` (or `canceled`):

1. For each completed task, read the receipt of its integrated run in `show TASK`: the `receipt` in the run's last `integration_receipt` event (`validation_finished` holds the receipt seen before landing; `receipt_path` is the file). Collect its `summary` and its `follow_ups`, an optional array of `{title, description}` the worker proposed for work it found outside its task.
2. Compare what landed against the goal's `acceptance` (`goal show ID`). A follow-up, or anything the acceptance asks for that no task delivered, is a gap in the decomposition.
3. Register each gap as a task on the same goal with `add --goal ID`, then `ready` it, and report to the user that the goal stays open; close it after those tasks are completed. Do not close first: a closed goal refuses new tasks.
4. When nothing is missing, record the verdict:

```sh
"$TASKQ" goal close ID --verdict achieved
```

`achieved` is refused while any task is `draft`, `ready` or `in_progress` (the error names the count and status); cancel or finish them first. `abandoned` records that the goal is given up: it is refused while a task is `in_progress`, and it does not cancel the goal's `draft` or `ready` tasks, so cancel them yourself first or the supervisor still runs them. Both verdicts are final; further work on the same problem is a new goal. `goal show ID` afterwards has `closed: true` at the top level, `verdict` and `closed_at` inside `goal`, and a `goal_closed` event with the task counts at close time.

Report a goal to the user as: its title and verdict (or open), its task counts from `goal list`, which tasks are `in_progress` or blocked, and, once every task is completed, whether the acceptance is met or which follow-ups you registered.
