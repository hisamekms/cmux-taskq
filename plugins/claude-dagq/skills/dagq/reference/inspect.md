# Inspect commands and their fields

Read this when you need a field of `list`, `show`, `goal show`, `graph`, `status`, `stats` or `doctor`, or the meaning of a task or run status.

| Command | Use |
| --- | --- |
| `"$DAGQ" goal list` | All goals with `id`, `title`, `status` (`draft` or `open`), `closed`, `verdict`, and `tasks` counts (`total`, `draft`, `ready`, `in_progress`, `completed`, `canceled`) |
| `"$DAGQ" goal show ID` | `goal` (all fields), `tasks` (`id`, `title`, `status`), `closed`, `events` (`goal_created`, `goal_updated`, `goal_status_changed`, `goal_closed`, `observation`; only `kind` and `created_at` of the latest 10, `events_total` counts them all), `observations` (the goal's latest 5 notes: `id`, `created_at`, `text`, `kind`, `by`) |
| `"$DAGQ" list` | One page of unfinished tasks, newest first: `{"tasks", "next", "total"}` (see below) |
| `"$DAGQ" show ID` | `task` (with `goal_id` and `context`), `dependencies`, `runs` (only the latest: `id`, `status`, `branch`, `result_commit`, `last_error`, `worktree_path`, `workspace_id`; `runs_total` counts them all), `events` (the latest 10, `--events N` for more, with `id`, `kind`, `created_at`, `run_id` when the event belongs to a run, and only `status` / `reason` / `last_error` / `from` / `to` of the payload; `events_total` counts them all), `processes` (the latest run's), `observations` (the latest 5 notes on the task or its runs: `id`, `created_at`, `run_id`, `text`, `kind`, `by`) |
| `"$DAGQ" notes` | Notes as `{"notes", "cursor"}`, oldest first: the latest `--limit` (default 20), or with `--since CURSOR` the first `--limit` after it. `--goal ID` keeps the goal's notes and its tasks' and runs'; `--task ID` the task's and its runs'. Each note is a run event of kind `observation` with payload `text`, `kind` (a slug, default `note`) and `by` (`DAGQ_ROLE` of the writer, or `human`) |
| `"$DAGQ" candidates` | Which tasks the next `supervise` can pick, in ID order: ready, every predecessor completed, no unfinished run, and not in a draft goal |
| `"$DAGQ" graph [--goal ID]` | Unfinished (`draft`, `ready`, `in_progress`) tasks as `{"tasks", "candidates", "critical"}`: per task `id`, `status`, `title`, `goal_id`, `goal_status` (only for a task in a goal; a `draft` goal's tasks are never candidates), `depends_on` (every direct predecessor), `ready_after` (the unfinished ones it still waits for), `blocks` (unfinished tasks depending on it directly) and `unblocks` (how many unfinished tasks depend on it directly or transitively); `candidates` in the order the supervisor claims them (most `unblocks` first, ties by ID); `critical` the chain from the task with the most `unblocks` down its most-releasing dependents (empty when nothing blocks anything). `--goal` narrows the tasks, candidates and the chain's start to one goal; the counts still span every goal |
| `"$DAGQ" locate` | The queue this directory resolves to (`db`, `runs_dir`, `git_common_dir`, `db_exists`) without opening it |
| `"$DAGQ" status` | `supervisors`: every registered `supervise` process (`pid`, `alive`, `parallel`, `heartbeat_age_secs`, `stale`, `run_ids`; listed even while it holds no run) plus any `integrate` process holding a lease (`registered: false`); `runs`: unfinished runs with their leases |
| `"$DAGQ" stats` | Where time goes, from run events: `runs` (latest 50 finished; `--full` all) with seconds of `work` (claim→receipt), `validate` (receipt→validation), `wait_to_land` (validation→integrated), `startup`, and counts `resumes`, `needs_session`, `failed`, `review_verdict`; `goals` and `overall` with `{count, total, median}` per interval; `alerts` (`{kind, task_id, run_id, value, threshold}`: `awaiting_integration` > 15 min, 3rd `needs_session`, `ask_unanswered` > 60 min, `task_failed` twice, `work_over_median` > 2× the goal median, `idle_slots` with ready tasks all blocked, `backend_failures` 2 or more failed cmux calls in the window); `backend_failures` (`{count, by_op, max_load_avg, max_slots}`: `backend_call_failed` events in the window, per op, the highest 1-minute load and slots held); `next_cursor` — pass it to `--since` for only runs finished later. `--goal ID` narrows to one goal |
| `"$DAGQ" doctor` | The same `supervisors`, one line each (`pid`, `alive`, `registered`, `mode`, `workspace_id`, `binary_version`, `heartbeat_age_secs`, `stale`, `run_ids`); per run: `run_id`, `task_id`, `status`, `lease_stale`, `recoverable`, `blocker_count`, `workspace_id`, `worktree_path`. `doctor --full` adds lease liveness, processes, worktree/receipt existence and the `blockers` themselves |

`list` answers "which tasks are moving or can move" and "how far is this goal". It prints `{"tasks": [...], "next": ID | null, "total": N}`:

- `tasks`: at most `--limit` (default 20) tasks in ID descending order (newest first). By default only unfinished ones (`draft`, `ready`, `in_progress`); `--status ready,in_progress` picks statuses (any of them; an unknown status exits 1 with an error), `--all` adds `completed` and `canceled`, `--goal ID` keeps one goal's tasks. `--status` / `--all` and `--goal` combine with AND.
- Each task has only `id`, `status`, `title`, `goal_id`, `dependencies` (predecessor IDs) and `latest_run` (`{"id", "status"}` of the newest run, or null). `--full` adds `description`, `acceptance`, `context`, `verification_commands`, `created_at`, `updated_at`; prefer `show ID` for one task.
- `next`: null means this page is the last one. Otherwise pass it as `--before NEXT` (with the same filters) for the following page; it is the ID of the first task of that page. Decide whether more pages exist from `next` alone — never by counting `tasks`.
- `total`: how many tasks match the filters across all pages.

`show`, `goal show` and `doctor` are compact by default so their size stays bounded by the number of runs or tasks: a long `description`, `acceptance`, `context` or `constraints` (and a run's `last_error`) is cut to 300 characters ending in `…`, and the object that holds it carries `truncated: true`. Add `--full` for the untruncated text, every run with all its fields, every event with its whole payload, and every process; the key names are the same in both forms.

Task `status`: `draft` → `ready` → `in_progress` → `completed`, or `canceled`. A task stays `in_progress` while any run is unfinished or awaiting integration.

Run `status` in `runs` (latest last): `claimed`, `starting`, `running`, `validating` are unfinished; `awaiting_integration` means the receipt passed validation (the verification commands have not run yet; `integrate` runs them after its rebase) and the run waits for `integrate` to land it on `main`; `integrating` means an `integrate` process is landing it right now; `needs_session` means the landing hit a rebase conflict or a failed verification and the supervisor resumes the run's session to fix it, up to three times (`last_error` says what); `integrated` means the run was squashed onto `main` (`result_commit` is the landed commit) and the task is `completed`; `failed` and `interrupted` keep their worktree and workspace for inspection, with the reason in `last_error`. Dependency-free tasks run in parallel (up to the supervisor's `--parallel`), each in its own workspace and worktree; a dependent task waits until every predecessor is `completed`.

Useful run fields: `branch` (`dagq/<run-id>`), `worktree_path`, `workspace_id` (cmux), `result_commit`, `last_error`, and with `show ID --full` also `run_dir` (prompt, logs, `receipt.json`, `integrate-<attempt>-verify-N.log`, one set per integrate attempt), `receipt_path`, `workspace_closed_at`.

## Decide what to run first with `graph`

When planning a goal or deciding what to make `ready` next, read `"$DAGQ" graph --goal ID` (or `graph` for the whole queue) instead of reading each task's `dependencies`:

1. `critical` is the chain that holds back the most work. Its first task should be `ready` and running; if it is `draft`, make it `ready` before tasks with small `unblocks`.
2. A task with a large `unblocks` that is not in `candidates` waits on its `ready_after`; those predecessors are what to finish (or to review and land) first.
3. When one task blocks many (goal 8 had five tasks waiting on one), consider splitting it or removing a dependency that is not real (`dependency remove`) so more tasks run in parallel.
4. `candidates` is the order the supervisor will claim in, so there is no need to register or `ready` tasks in a particular order to get the releasing ones first.

## Change a goal or a task's goal


```sh
"$DAGQ" goal list                         # every goal with its task counts by status
"$DAGQ" goal show ID                      # the goal, its tasks (id, title, status), its latest events (--full: everything)
"$DAGQ" set-goal TASK GOAL                # move a draft or ready task into an open goal
"$DAGQ" set-goal TASK --none              # take a draft or ready task out of its goal
"$DAGQ" goal edit ID --constraints "..."  # replace one or more fields (--title, --description, --acceptance, --constraints, --doc; --doc "" clears it)
"$DAGQ" goal close ID --verdict achieved  # or abandoned; see goal-close.md
```

`set-goal` follows the dependency rules: only a `draft` or `ready` task can be moved, and a closed goal accepts no task. Take an `in_progress` task back with `draft ID` only if its run is finished; a `completed` task keeps the goal it landed with. `goal edit` records the old and new fields in a `goal_updated` event; a run already claimed keeps the prompt it started with, and runs claimed afterwards see the new text.
