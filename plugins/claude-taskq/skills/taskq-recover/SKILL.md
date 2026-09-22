---
name: taskq-recover
description: Diagnose and recover one cmux-taskq run that is stuck, that its supervisor gave up on, whose supervisor died, or that stays claimed/starting/running/validating after the session ended, without disturbing the runs executing next to it. Use when the user reports an orphaned run, a run with last_error and no lease, a stale supervisor in status or doctor, or asks to recover or retry a run.
---

# cmux-taskq: diagnose and recover a run

Prerequisite: resolve the launcher as in the `taskq` skill (`TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"`). Never touch the queue database directly; the binary refuses unsafe recoveries itself, so do not work around it.

## 1. Diagnose without changing state

```sh
"$TASKQ" doctor
```

- `supervisors`: one entry per supervisor process that still holds leases: `pid`, `alive` (`kill -0`), `run_ids`, `heartbeat_age_secs`, and `stale` (older than 30 seconds). Empty means no run is owned by anyone.
- `runs`: every run in `claimed`, `starting`, `running`, or `validating` with `task_id`, `workspace_id`, `worktree_exists`, `run_dir_exists`, `receipt_exists`, `last_error`, its own `lease` (`pid`, `alive`, heartbeat age, `stale`; `null` when no supervisor owns it), and each registered `wrapper` / `agent` process with `pid`, `alive`, heartbeat age, and `exit_code` (`alive` is null once an exit is recorded).
- `blockers`: per run, what still prevents recovery. `recoverable: true` when empty. Only the run's own lease and processes count; other runs, healthy or not, never block it.

Explain to the user what is still alive. Common cases: the supervisor gave the run up (`last_error` set, `lease` null, `runtime_error` event with `lease_released: true`) while its session may still be running, and the supervisor keeps serving other runs; the supervisor was killed (lease stale, PID dead) while the sessions are still running; the whole machine restarted (everything dead); the supervisor is alive but its heartbeat stopped (it must be stopped by the user, a lease is never taken over automatically).

## 2. Stop what is still running

Recovery is refused while any process registered for that run is alive, its lease heartbeat is fresh, or its lease PID is alive. Ask the user to end those first: send `/exit` in the task's cmux workspace (`cmux workspace list` shows it by `workspace_id`), and stop a hung supervisor process. Do not kill processes yourself unless the user asks. Runs owned by a live supervisor are not orphans; leave them to it.

## 3. Recover

```sh
"$TASKQ" recover RUN_ID
```

`RUN_ID` comes from `doctor` or `show ID`. On success it prints `{"outcome": "recovered", "run": ...}`: the run is `interrupted`, a `run_recovered` event records what was checked, and that run's lease (if any) is deleted. Other runs, their leases and processes are untouched, so a supervisor running other tasks keeps going. The worktree, branch, cmux workspace, and run directory are kept for inspection, and the task stays `in_progress`. Nothing is rerun automatically.

## 4. Retry or give up

A retry is a separate decision. To run the task again, `"$TASKQ" ready ID` (or `draft ID` to edit it first, then `ready`); a running supervisor (or the next one) creates a new run with its own worktree. The same applies to a task whose latest run `failed`. To drop the task, `cancel ID`. Removing the old worktree, branch, and workspace is the user's manual cleanup; list them from the run's `worktree_path`, `branch`, and `workspace_id` in `show ID`.
