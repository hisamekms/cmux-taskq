# status, watch and run states

Read this when a field of `status`, `watch` or `show` is unclear.

## status

- `supervisors`: one entry per process that owns runs (`pid`, `alive`, `registered`, `mode`, `parallel`, `heartbeat_age_secs`, `stale`, `run_ids`).
  - `registered: true`, `alive: true`, `stale: false`: the resident supervisor is healthy. It is listed even when `run_ids` is empty, and picks a new candidate within a few seconds.
  - `stale: true` (`alive: false`, or `heartbeat_age_secs` over 30): it died or hangs. `up` prunes a dead registration and starts a fresh supervisor; its orphaned runs are handled with the `dagq-recover` skill, one run at a time. A registration whose PID is alive but silent is neither pruned nor reused, so `up` starts a second supervisor beside it: report the stale row and let the user stop that process (`down --force` if it is the only one).
  - `registered: false`: not a supervisor but a process holding a lease, normally an `integrate` landing a run.
  - Empty: nothing serves this queue; run `up`.
- `runs`: unfinished runs with their leases and `worktree_path`.
- `attention`: each with `run_id`, `task_id`, `status`, `kind` (the run event that brought it there), `last_error` and a fixed `next`:
  - `review and integrate`: `awaiting_integration`.
  - `resume session`: `needs_session`.
  - `inspect and close workspace`: `failed`.
  - `send /exit`: a `running` run whose exit request timed out.
  - `answer the prompt in workspace <id>`: a `running` run that stopped at a dialog before its receipt (`kind` `prompt_waiting`); it disappears after `prompt_cleared` or `receipt_observed`. Handle it with the `dagq-session` skill.
  - `push main`: an `integrated` run (its task `completed`) whose push of `main` to `origin` failed (`kind` `push_failed`, `last_error` the Git error) with no successful push since.
  - `recover run`: a `claimed` / `starting` / `running` / `validating` / `integrating` run without a lease, given up by its supervisor (`kind` `runtime_error` with `lease_released: true`). Nothing adopts it; handle it with the `dagq-recover` skill. It disappears once recovered. If `watch` reported it but `status` shows the run at rest, `status` wins.
  - `restart supervisor`: `kind` `supervisor_stale` (with its `pid`) or `supervisor_stopped` (nothing registered).
- `cursor`: the newest event id.

## watch and events

`"$DAGQ" watch --after <cursor> [--timeout 600] [--interval 2]` blocks until an attention event arrives after the cursor or the supervisors' registrations or `alive` / `stale` change, then returns `{events, supervisors_changed, supervisors, cursor}`. On timeout `events` is empty and the cursor is unchanged. `"$DAGQ" events --after <cursor>` returns the same attention events without waiting (`--all` for every kind, `--limit N`, default 100). Each event is compact: `status`, `exit_code`, `reason` cut to 300 characters, and `next`.

## show

`"$DAGQ" show ID` prints the task, only the latest run, the latest 10 events (`--events N` for more) with the gist of their payload, and long texts cut to 300 characters ending in `…` with `truncated: true`. `show ID --full` adds `run_dir`, earlier runs and whole event payloads such as a receipt. `"$DAGQ" list --status in_progress` finds the tasks worth a `show`: each entry's `latest_run` gives the newest run's `id` and `status`.

## Run states

- `claimed` / `starting` / `running`: in progress. Events: `lease_acquired`, `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session exited; the supervisor checks the receipt, the commit and a clean worktree, and reruns the verification commands.
- `awaiting_integration`: accepted. `result_commit` on `branch` is what to review; the workspace was closed and the worktree and branch are kept until `integrate`.
- `integrating`: an `integrate` process holds the queue's single integration slot. If its process died (`doctor` shows `lease_stale: true`), the `dagq-recover` skill returns the run to `awaiting_integration`.
- `needs_session`: `integrate` could not land it (rebase conflict, or a verification command failed after the rebase); `last_error` says why.
- `integrated`: landed on `main`; the task is `completed`.
- `failed`: rejected or exited nonzero; `last_error` says why. Workspace and worktree are kept. Retry with `ready ID` after fixing the cause.
- unfinished with `last_error` and no lease (a `runtime_error` event with `lease_released: true`): the supervisor gave the run up; use the `dagq-recover` skill. After a provisioning failure the supervisor stops claiming, drains and exits nonzero; launchd restarts it, so fix the cause (cmux, Git) and check `status`.
- `interrupted`: recovered; retry with `ready ID`. Its workspace is kept.

Completion is decided only by these states. A Stop hook, an idle session or a `receipt.json` in `run_dir` is not success: `validation_finished` records the supervisor's verdict. One run's failure never changes another run.
