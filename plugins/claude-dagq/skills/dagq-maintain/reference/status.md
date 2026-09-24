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
  - `reviewing (runtime)`: `awaiting_integration` under the supervisor's lease: its headless review and what follows from the verdict (ADR-0027); nothing to do.
  - `review by hand`: `awaiting_integration` after `review_failed` (the headless review failed); review it and `integrate` (the `dagq-land` skill).
  - `review and integrate`: `awaiting_integration` without a lease and without a failed review: a run accepted before the supervisor reviewed runs, or one whose supervisor gave it up mid-review on an error (`kind` `runtime_error`; its session may still be open, so read its screen and `/exit` it first).
  - A run whose review raised a concern shows only as its `approve_landing` ask (`answer ask <id>`, for the inbox); once answered `land`, `send_back` or `cancel`, the ask shows `applying the answer of ask <id> (runtime)`.
  - `resuming (runtime)`: `needs_session`, being resumed by the supervisor or with resumes left; nothing to do.
  - `resume session`: `needs_session` after the supervisor's third resume did not resolve it.
  - `inspect and close workspace`: `failed`.
  - `answer the prompt in workspace <id>`: a `running` run that stopped at a dialog before its receipt (`kind` `prompt_waiting`); it disappears after `prompt_cleared` or `receipt_observed`. Handle it with the `dagq-session` skill.
  - `push main`: an `integrated` run (its task `completed`) whose push of `main` to `origin` failed (`kind` `push_failed`, `last_error` the Git error) with no successful push since.
  - `recover run`: a `claimed` / `starting` / `running` / `validating` / `integrating` run without a lease, given up by its supervisor (`kind` `runtime_error` with `lease_released: true`). Nothing adopts it; handle it with the `dagq-recover` skill. It disappears once recovered. If `watch` reported it but `status` shows the run at rest, `status` wins.
  - `restart supervisor`: `kind` `supervisor_stale` (with its `pid`) or `supervisor_stopped` (nothing registered).
  - `answer ask <id>`: an open ask (`kind` `ask_opened`, `status` `open`, with `ask_id`); the inbox's to answer. A `running` run whose exit request timed out is no attention of its own: the supervisor raises it as a `stuck_exit` ask (options `exit` / `wait`) for the inbox; you act on its answer (`read the answer of ask <id> and close it`, `dagq-session`, section 3), and the supervisor closes it once the session exits (that `ask_answered` carries `runtime_closed: true` and is no `watch` event).
  - `read the answer of ask <id> and close it`: an answered ask nobody closed (`kind` `ask_answered`, `status` `answered`); the maintainer's. It disappears after `ask close <id>`.
  - `delivering the answer of ask <id> (runtime)`: an answered `worker_question` of a `running` run; the supervisor types the answer into the worker's terminal once the worker is idle after asking, closes the ask and records `ask_delivered`. Nothing to do; its `ask_answered` is not a `watch` event.
  - `send the answer of ask <id> to the worker and close it`: an answered `worker_question` the supervisor could not type (`kind` `ask_delivery_failed`, tried once), or whose run is no longer `running`. Handle it with the `dagq-session` skill.
- `status --role <maintainer|inbox|planner>` keeps only the attention for that role: `ask_opened` is the inbox's, everything else the maintainer's, none the planner's.
- `asks`: the open asks, each with `id`, `kind`, `question` (first 200 characters, `…` when cut), `task_id`, `run_id`, `asked_by` and `age_secs`.
- `cursor`: the newest event id.

## watch and events

`"$DAGQ" watch --after <cursor> [--timeout 600] [--interval 2]` blocks until an attention event arrives after the cursor or the supervisors' registrations or `alive` / `stale` change, then returns `{events, supervisors_changed, supervisors, cursor}`. On timeout `events` is empty and the cursor is unchanged. `--role <maintainer|inbox|planner>` wakes only for that role's attention events; only `maintainer` also wakes for the supervisors. Asks ride the same cursor: registering one writes `ask_opened`, answering it `ask_answered`. `"$DAGQ" events --after <cursor>` returns the same attention events without waiting (`--all` for every kind, `--limit N`, default 100). Each event is compact: `status`, `exit_code`, `ask_id`, `reason` cut to 300 characters, and `next`.

## asks

`"$DAGQ" asks` lists the asks nobody closed, oldest first, with every field (`question`, `options`, `answer`, `answered_at`, `closed_at`). `--open` keeps the unanswered ones, `--role maintainer` the answered ones waiting for you, `--role inbox` the open ones, `--all` adds closed asks.

Registering one (`"$DAGQ" ask --kind <kind> --question <text> [--option <text>]... (--run RUN_ID | --task ID)`):

- `--run` for a question about one run, `--task` for one about a task as a whole; one of them is required. `approve_landing` is the `dagq-land` doubt, `answer_prompt` a dialog you cannot answer, `decide` anything else; `worker_question` is the workers' own kind (a worker registers it with `--run` and stops; see the `dagq-session` skill).
- Write the question so the user can answer it without your session's context: the task, what you found, the options and what each leads to.
- The same run (or task) and kind is registered once: asking again returns the open ask with `created: false`.
- A new ask sends one `cmux notify` to the inbox workspace (`notified: true`); asking again notifies nobody. A failed notification does not fail the ask: it stays registered with `notified: false` and `notify_error`, and the inbox still sees it through `watch --role inbox`.
- `ask close <id>` marks an answered ask read. An unanswered ask cannot be closed: to withdraw one, answer it yourself first (`"$DAGQ" answer <id> --text "withdrawn: <why>"`), then close it.

## stats

`"$DAGQ" stats [--since CURSOR] [--goal ID] [--full]` reads run events only; the field list is in the `dagq` skill's `reference/inspect.md`. For load problems look at:

- `backend_failures`: `{count, by_op, max_load_avg, max_slots}` — cmux calls that failed or timed out (30 s each) in the window: after `--since` up to `next_cursor`, otherwise since the oldest returned run started. `by_op` counts per call (`create`, `create_named`, `capture`, `close`, `send_exit`, `exists`, `ensure_group`); `max_load_avg` is the highest 1-minute load average recorded with one (null when unavailable) and `max_slots` the most runs the supervisor held then. Each failure is a `backend_call_failed` event (`op`, `workspace_id`, `timeout_secs`, `error`, `load_avg`, `slots`, `parallel`), on its run when it was for one and on no task for `up` / `down` / the workspace group. `cleanup_failed`, `screen_capture_failed` and `exit_request_timed_out` are still recorded as before; the first two also produce a `backend_call_failed`, while `exit_request_timed_out` (a session that did not exit) does not — only a failed `send_exit` does.
- The alert `backend_failures` (`value` the count, `threshold` 2, no task or run): two or more failures in the window. A high `max_load_avg` with it is the evidence for proposing a lower `--parallel`; the observer reads it from `stats` and does not fix it.

## show

`"$DAGQ" show ID` prints the task, only the latest run, the latest 10 events (`--events N` for more) with the gist of their payload, and long texts cut to 300 characters ending in `…` with `truncated: true`. `show ID --full` adds `run_dir`, earlier runs and whole event payloads such as a receipt. `"$DAGQ" list --status in_progress` finds the tasks worth a `show`: each entry's `latest_run` gives the newest run's `id` and `status`.

## Run states

- `claimed` / `starting` / `running`: in progress. Events: `lease_acquired`, `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session went idle after its receipt (it stays open) or exited; the supervisor checks the receipt, the commit, a clean worktree and required evidence. It does not run the verification commands; `integrate` runs them once after its rebase.
- `awaiting_integration`: accepted. While the supervisor holds its lease it reviews the run with the session still open (`review_started` / `review_finished`, `revise_requested` / `revise_finished`); a pass lands it, a concern closes the session and opens an `approve_landing` ask, a failed review closes it and leaves it to you (`review_failed`). `result_commit` on `branch` is what was reviewed; the worktree and branch are kept until the landing.
- `integrating`: an `integrate` process holds the queue's single integration slot. If its process died (`doctor` shows `lease_stale: true`), the `dagq-recover` skill returns the run to `awaiting_integration`.
- `needs_session`: `integrate` could not land it (rebase conflict, or a verification command failed after the rebase); `last_error` says why. The supervisor resumes its session (up to three times, `resume_started` / `resume_finished` events), then lands it if `integrate` was called for it (`integration_approved`), or else validates and reviews it with the resumed session open, like the worker's. A `send_back` answer to a review's `approve_landing` ask also makes a run `needs_session` (`landing_decided`).
- `integrated`: landed on `main`; the task is `completed`.
- `failed`: rejected or exited nonzero; `last_error` says why. Workspace and worktree are kept. Retry with `ready ID` after fixing the cause.
- unfinished with `last_error` and no lease (a `runtime_error` event with `lease_released: true`): the supervisor gave the run up; use the `dagq-recover` skill. After a provisioning failure the supervisor stops claiming, drains and exits nonzero; launchd restarts it, so fix the cause (cmux, Git) and check `status`.
- `interrupted`: recovered; retry with `ready ID`. Its workspace is kept.

Completion is decided only by these states. A Stop hook, an idle session or a `receipt.json` in `run_dir` is not success: `validation_finished` records the supervisor's verdict. One run's failure never changes another run.
