---
name: taskq-recover
description: Diagnose and recover a cmux-taskq run that is stuck, whose supervisor died or lost its lease, or that stays claimed/starting/running/validating after the session ended. Use when the user reports that supervise refuses to start, a run looks orphaned, status shows a stale heartbeat, or asks to recover or retry a run.
---

# cmux-taskq: diagnose and recover a run

Prerequisite: resolve the launcher as in the `taskq` skill (`TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"`). Never touch the queue database directly; the binary refuses unsafe recoveries itself, so do not work around it.

## 1. Diagnose without changing state

```sh
"$TASKQ" doctor
```

- `supervisor`: lease `pid`, `alive` (`kill -0`), `heartbeat_age_secs`, and `stale` (older than 30 seconds). `null` means no supervisor.
- `runs`: every run in `claimed`, `starting`, `running`, or `validating` with `task_id`, `workspace_id`, `worktree_exists`, `run_dir_exists`, `receipt_exists`, `last_error`, and each registered `wrapper` / `agent` process with `pid`, `alive`, heartbeat age, and `exit_code` (`alive` is null once an exit is recorded).
- `blockers`: per run, what still prevents recovery. `recoverable: true` when empty.

Explain to the user what is still alive. Common cases: the supervisor was killed (lease stale, PID dead) while the session in the task workspace is still running; the whole machine restarted (everything dead); the supervisor is alive but its heartbeat stopped (it must be stopped by the user, the lease is never taken over automatically).

## 2. Stop what is still running

Recovery is refused while any registered process is alive, the lease heartbeat is fresh, or the lease PID is alive. Ask the user to end those first: send `/exit` in the task's cmux workspace (`cmux workspace list` shows it by `workspace_id`), and stop a hung supervisor process. Do not kill processes yourself unless the user asks.

## 3. Recover

```sh
"$TASKQ" recover RUN_ID
```

`RUN_ID` comes from `doctor` or `show ID`. On success it prints `{"outcome": "recovered", "run": ...}`: the run is `interrupted`, a `run_recovered` event records what was checked, and the stale lease is deleted. The worktree, branch, cmux workspace, and run directory are kept for inspection, and the task stays `in_progress`. Nothing is rerun automatically.

## 4. Retry or give up

A retry is a separate decision. To run the task again, `"$TASKQ" ready ID` (or `draft ID` to edit it first, then `ready`); the next supervisor creates a new run with its own worktree. The same applies to a task whose latest run `failed`. To drop the task, `cancel ID`. Removing the old worktree, branch, and workspace is the user's manual cleanup; list them from the run's `worktree_path`, `branch`, and `workspace_id` in `show ID`.
