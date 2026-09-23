---
name: dagq-recover
description: Diagnose and recover one dagq run that is stuck, that its supervisor gave up on, whose supervisor or integrate process died, or that stays claimed/starting/running/validating/integrating after its process ended, without disturbing the runs executing next to it. Use when the user reports an orphaned run, a run with last_error and no lease, a stale supervisor in status or doctor, a run stuck in integrating, or asks to recover or retry a run.
---

# dagq: diagnose and recover a run

Prerequisite: resolve the launcher as in the `dagq` skill (`DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`). Never touch the queue database directly; the binary refuses unsafe recoveries itself, so do not work around it.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

Plain `doctor` is the compact form, one line's worth per supervisor and per run (`run_id`, `task_id`, `status`, `lease_stale`, `recoverable`, `blocker_count`, `workspace_id`, `worktree_path`); diagnosing needs `--full`, which prints the lease, processes, paths and the `blockers` described below.

- `supervisors`: one entry per registered `supervise` process (`registered: true`, with `pid`, `alive` (`kill -0`), `parallel`, `started_at`, `heartbeat_at`, `heartbeat_age_secs`, `run_ids`, and `stale` when the PID is dead or the heartbeat is older than 30 seconds), plus one per process that holds leases without a registration, such as a running `integrate` (`registered: false`). A resident supervisor is listed even with an empty `run_ids`; empty `supervisors` means no supervisor is registered and no run is owned by anyone. A stale registration is left by a killed or hung supervisor; the runtime never deletes it, so report it to the user rather than trying to remove it, and `recover` works on runs regardless of it.
- `runs`: every run in `claimed`, `starting`, `running`, `validating`, or `integrating` with `task_id`, `workspace_id`, `worktree_exists`, `run_dir_exists`, `receipt_exists`, `last_error`, its own `lease` (`pid`, `alive`, heartbeat age, `stale`; `null` when no supervisor owns it), and each registered `wrapper` / `agent` process with `pid`, `alive`, heartbeat age, and `exit_code` (`alive` is null once an exit is recorded).
- `blockers`: per run, what still prevents recovery. `recoverable: true` when empty. Only the run's own lease and processes count; other runs, healthy or not, never block it.

Explain to the user what is still alive. Common cases: the supervisor gave the run up (`last_error` set, `lease` null, `runtime_error` event with `lease_released: true`) while its session may still be running, and the supervisor keeps serving other runs; the supervisor was killed (lease stale, PID dead) while the sessions are still running; the whole machine restarted (everything dead); the supervisor is alive but its heartbeat stopped (it must be stopped by the user); an `integrate` process died while landing a run (the run is `integrating` with a stale lease and no wrapper/agent processes).

A `running` or `validating` run whose lease is stale while its wrapper is alive (heartbeat within 30 seconds) or has recorded its exit is not a case for `recover`: the next supervisor with a free slot adopts it (a `run_adopted` event, the lease and `supervisor_token` move to that supervisor) and finishes it, so do not send `/exit` or recover such a run; start or wait for a supervisor (`up` reuses a live one) and watch `show ID`. Only a run whose wrapper is dead or silent, a `claimed` / `starting` run, an `integrating` run, or a run without a lease needs `recover`.

## 2. Stop what is still running

Recovery is refused while any process registered for that run is alive, its lease heartbeat is fresh, or its lease PID is alive. Ask the user to end those first: send `/exit` in the task's cmux workspace (`cmux workspace list` shows it by `workspace_id`), and stop a hung supervisor process. Do not kill processes yourself unless the user asks. Runs owned by a live supervisor are not orphans; leave them to it.

## 3. Recover

```sh
"$DAGQ" recover RUN_ID
```

`RUN_ID` comes from `doctor` or `show ID`. On success it prints `{"outcome": "recovered", "run": ...}`: the run is `interrupted` (an `integrating` run goes back to `awaiting_integration` instead, because its validated result is intact; land it again with the `dagq-maintain` skill), a `run_recovered` event records what was checked, and that run's lease (if any) is deleted. Other runs, their leases and processes are untouched, so a supervisor running other tasks keeps going. The worktree, branch, cmux workspace, and run directory are kept for inspection, and the task stays `in_progress`. Nothing is rerun automatically.

## 4. Retry or give up

A retry is a separate decision. To run the task again, `"$DAGQ" ready ID` (or `draft ID` to edit it first, then `ready`); a running supervisor (or the next one) creates a new run with its own worktree. The same applies to a task whose latest run `failed`. To drop the task, `cancel ID`. Removing the old worktree, branch, and workspace is the user's manual cleanup; list them from the run's `worktree_path`, `branch`, and `workspace_id` in `show ID`.
