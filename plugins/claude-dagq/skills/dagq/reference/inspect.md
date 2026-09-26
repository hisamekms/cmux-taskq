# Inspect commands and their fields

Read this when you need a field of `list`, `show`, `goal show`, `graph`, `status`, `stats` or `doctor`, the meaning of a task or run status, how priority orders claiming, or how to edit a draft or submitted task.

| Command | Use |
| --- | --- |
| `"$DAGQ" goal list` | All goals with `id`, `title`, `status` (`draft` or `open`), `closed`, `verdict`, and `tasks` counts (`total`, `draft`, `submitted`, `ready`, `in_progress`, `completed`, `canceled`) |
| `"$DAGQ" goal show ID` | `goal` (all fields), `tasks` (`id`, `title`, `status`), `closed`, `events` (`goal_created`, `goal_updated`, `goal_status_changed`, `goal_closed`, `observation`; only `kind` and `created_at` of the latest 10, `events_total` counts them all), `observations` (the goal's latest 5 notes: `id`, `created_at`, `text`, `kind`, `by`) |
| `"$DAGQ" list` | One page of unfinished tasks, newest first: `{"tasks", "next", "total"}` (see below) |
| `"$DAGQ" show ID` | `task` (with `goal_id` and `context`), `dependencies`, `runs` (only the latest: `id`, `status`, `branch`, `result_commit`, `last_error`, `last_error_code` when known, `worktree_path`, `workspace_id`; `runs_total` counts them all), `events` (the latest 10, `--events N` for more, with `id`, `kind`, `created_at`, `run_id` when the event belongs to a run, and only `status` / `reason` / `last_error` / `from` / `to` / `code` (the reason code) of the payload; `events_total` counts them all), `processes` (the latest run's), `observations` (the latest 5 notes on the task or its runs: `id`, `created_at`, `run_id`, `text`, `kind`, `by`) |
| `"$DAGQ" note --task ID \| --run RUN_ID \| --goal ID --text "..." [--kind SLUG]` | Records one note (an `observation` event) on a task, a run or a goal |
| `"$DAGQ" notes [--goal ID] [--task ID] [--since CURSOR]` | Notes as `{"notes", "cursor"}`, oldest first: the latest `--limit` (default 20), or with `--since CURSOR` the first `--limit` after it. `--goal ID` keeps the goal's notes and its tasks' and runs'; `--task ID` the task's and its runs'. Each note is a run event of kind `observation` with payload `text`, `kind` (a slug, default `note`) and `by` (`DAGQ_ROLE` of the writer, or `human`) |
| `"$DAGQ" candidates` | Which tasks the next `supervise` can pick, in claim order (see "Priority and claim order"): ready, every predecessor completed, every goal it depends on closed as achieved, no unfinished run, and not in a draft goal. Each is the task (with its `priority`) plus the `effective_priority` it is claimed by |
| `"$DAGQ" graph [--goal ID]` | Unfinished (`draft`, `submitted`, `ready`, `in_progress`) tasks as `{"tasks", "candidates", "critical"}`: per task `id`, `status`, `title`, `goal_id`, `goal_status` (only for a task in a goal; a `draft` goal's tasks are never candidates), `depends_on` (every direct predecessor), `goal_dependencies` (every goal it depends on), `ready_after` (what it still waits for: the unfinished predecessors' IDs, then `{"goal": ID}` for each goal it depends on that is not closed as achieved; an abandoned goal stays, since it never releases the task), `blocks` (unfinished tasks depending on it directly, or on its goal while that goal is not closed), `unblocks` (how many unfinished tasks depend on it directly or transitively), `priority` (its own) and `effective_priority` (what it is claimed by); `candidates` in the order the supervisor claims them (highest `effective_priority`, then most `unblocks`, then lowest ID); `critical` the chain from the task with the most `unblocks` down its most-releasing dependents (empty when nothing blocks anything). `--goal` narrows the tasks, candidates and the chain's start to one goal; the counts still span every goal |
| `"$DAGQ" locate` | The queue this directory resolves to (`db`, `runs_dir`, `git_common_dir`, `db_exists`) without opening it |
| `"$DAGQ" status` | `supervisors`: every registered `supervise` process (`pid`, `alive`, `parallel`, `heartbeat_age_secs`, `stale`, `run_ids`; listed even while it holds no run) plus any `integrate` process holding a lease (`registered: false`); `runs`: unfinished runs with their leases |
| `"$DAGQ" stats` | Where time goes, from run events: `runs` (latest 50 finished; `--full` all) with seconds of `work` (claim→receipt), `validate` (receipt→validation), `wait_to_land` (validation→integrated), `startup`, and counts `resumes`, `needs_session`, `failed`, `review_verdict`; `goals` and `overall` with `{count, total, median}` per interval; `alerts` (`{kind, task_id, run_id, value, threshold}`: `awaiting_integration` > 15 min, 3rd `needs_session`, `ask_unanswered` > 60 min, `task_failed` twice, `work_over_median` > 2× the goal median, `idle_slots` with ready tasks all blocked, `backend_failures` 2 or more failed cmux calls in the window); `backend_failures` (`{count, by_op, max_load_avg, max_slots}`: `backend_call_failed` events in the window, per op, the highest 1-minute load and slots held); `next_cursor` — pass it to `--since` for only runs finished later. `kinds` groups the runs by their task's `kind` like `goals` (`kind: null` last). Each run also has `title`, `kind`, `claimed_at` / `validated_at` / `landed_at`, `integrate_attempts`, `deferrals` (per code), `conflict_files`, `broken_by` (the landings on main its landing was deferred after), `broke_runs`, and `resume_attempts` (`{reason, secs, resolved}`); `goals` and `overall` add `resume_outcomes` (per reason, with `resolved_percent`). `--since` / `--until` take an event id, `@<unix seconds>` or an RFC 3339 time. `--goal ID` narrows to one goal |
| `"$DAGQ" doctor` | The same `supervisors`, one line each (`pid`, `alive`, `registered`, `mode`, `workspace_id`, `binary_version`, `heartbeat_age_secs`, `stale`, `run_ids`); per run: `run_id`, `task_id`, `status`, `lease_stale`, `recoverable`, `blocker_count`, `workspace_id`, `worktree_path`. `doctor --full` adds lease liveness, processes, worktree/receipt existence and the `blockers` themselves |

`list` answers "which tasks are moving or can move" and "how far is this goal". It prints `{"tasks": [...], "next": ID | null, "total": N}`:

- `tasks`: at most `--limit` (default 20) tasks in ID descending order (newest first). By default only unfinished ones (`draft`, `submitted`, `ready`, `in_progress`); `--status ready,in_progress` picks statuses (any of them; an unknown status exits 1 with an error), `--all` adds `completed` and `canceled`, `--goal ID` keeps one goal's tasks. `--status` / `--all` and `--goal` combine with AND.
- Each task has only `id`, `status`, `title`, `priority`, `goal_id`, `dependencies` (predecessor IDs), `goal_dependencies` and `latest_run` (`{"id", "status"}` of the newest run, or null). `--full` adds `description`, `acceptance`, `context`, `verification_commands`, `required_evidence` (the `--evidence` checks), `paths` (the `--paths` globs; empty means unrestricted), `created_at`, `updated_at`; `effective_priority` is only in `graph` and `candidates`. Prefer `show ID` for one task.
- `next`: null means this page is the last one. Otherwise pass it as `--before NEXT` (with the same filters) for the following page; it is the ID of the first task of that page. Decide whether more pages exist from `next` alone — never by counting `tasks`.
- `total`: how many tasks match the filters across all pages.

`show`, `goal show` and `doctor` are compact by default so their size stays bounded by the number of runs or tasks: a long `description`, `acceptance`, `context` or `constraints` (and a run's `last_error`) is cut to 300 characters ending in `…`, and the object that holds it carries `truncated: true`. Add `--full` only when you need the untruncated text, every run with all its fields, every event with its whole payload (a receipt), every process, or a run's `run_dir`; the key names are the same in both forms.

Task `status`: `draft` → `submitted` (waiting for plan review; `submit` puts drafts in a proposal, never claimed) → `ready` → `in_progress` → `completed`, or `canceled`. Only plan review, `ready --bypass-review` and a retry of an `in_progress` task whose runs failed make a task `ready`. A task stays `in_progress` while any run is unfinished or awaiting integration.

Run `status` in `runs` (latest last): `claimed`, `starting`, `running`, `validating` are unfinished; `awaiting_integration` means the receipt passed validation (the verification commands have not run yet; `integrate` runs them after its rebase) and the run waits for `integrate` to land it on `main`; `integrating` means an `integrate` process is landing it right now; `needs_session` means the landing hit a rebase conflict or a failed verification and the supervisor resumes the run's session to fix it, up to three times (`last_error` says what); `integrated` means the run was squashed onto `main` (`result_commit` is the landed commit) and the task is `completed`; `failed` and `interrupted` keep their worktree and workspace for inspection, with the reason in `last_error`. Dependency-free tasks run in parallel (up to the supervisor's `--parallel`), each in its own workspace and worktree; a dependent task waits until every predecessor is `completed`.

Useful run fields: `branch` (`dagq/<run-id>`), `worktree_path`, `workspace_id` (cmux), `result_commit`, `last_error`, and with `show ID --full` also `run_dir` (prompt, logs, `receipt.json`, `integrate-<attempt>-verify-N.log`, one set per integrate attempt), `receipt_path`, `workspace_closed_at`.

## Decide what to run first with `graph`

When planning a goal or deciding what to submit next, read `"$DAGQ" graph --goal ID` (or `graph` for the whole queue) instead of reading each task's `dependencies`:

1. `critical` is the chain that holds back the most work. Its first task should be `ready` and running; if it is `draft`, complete it and `submit` it for plan review before tasks with small `unblocks` (only plan review, or a person's explicit `ready --bypass-review`, makes it `ready`).
2. A task with a large `unblocks` that is not in `candidates` waits on its `ready_after`; those predecessors are what to finish (or to review and land) first.
3. When one task blocks many (goal 8 had five tasks waiting on one), consider splitting it or removing a dependency that is not real (`dependency remove`) so more tasks run in parallel.
4. `candidates` is the order the supervisor will claim in, so there is no need to register or submit tasks in a particular order to get the releasing ones first. To put a task ahead of the others, give it a priority (below).

## Priority and claim order

Every task has one of five priorities. Pick it by what waiting costs, not by how much the task matters in general:

| Level | Use for | Example |
| --- | --- | --- |
| `interrupt` | An exceptional cut-in that must run before every other ready task. Not for routine use: when many tasks are `interrupt`, it orders nothing | A goal the person decided to put ahead of the whole queue (goal 28, which added priorities) |
| `urgent` | A defect that is stopping operation now | The supervisor or landing is broken |
| `high` | Work to do early, because other work builds on it | A cleanup or refactor several planned tasks assume |
| `normal` | Everything else (the default) | |
| `low` | Work that can wait | A conditional proposal, an observer's idea not yet needed |

```sh
"$DAGQ" add "TITLE" --goal 1 --priority high ...   # default normal; names only (no numbers)
"$DAGQ" set-priority TASK urgent                    # a draft or ready task
```

- **Claim order.** The supervisor, `candidates` and `graph`'s `candidates` all use one order: highest effective priority → most `unblocks` → lowest ID. (A goal rank will sit between priority and `unblocks` once goals get one.)
- **Effective priority.** A task's own priority, raised to the highest priority of the `ready` tasks that wait on it directly or transitively. So a `high` ready task pulls its unfinished predecessors ahead with it; there is no need to raise them one by one. Only `ready` tasks outside a draft goal pass their priority on: an `in_progress`, `draft`, `canceled` or `completed` task, or a task in a draft goal, raises nothing (a draft set aside does not push its predecessors forward). A task in between does not stop it: a ready task's priority still reaches the predecessors beyond a draft one. `graph` shows both `priority` and `effective_priority`.
- **When it can change.** Only while the task is `draft` or `ready`; `set-priority` on a claimed (`in_progress`) or finished task is refused. It takes effect at the next claim and never stops a run already running. A change is recorded as a `task_priority_changed` event (`from`, `to`).
- **Express urgency with priority only.** Do not move other tasks back to `draft`, and do not add or remove dependencies that are not real, to get one task claimed first; give it a higher priority instead.
- **No aging.** A `low` task waits as long as higher ones keep arriving; nothing raises it over time. Check `graph` for ready tasks that never reach the front and raise them by hand.

## Edit a draft or submitted task

Fix a draft or submitted task in place instead of canceling it and registering it again (that changes its ID and its dependents' edges):

```sh
"$DAGQ" edit TASK --description "..." --acceptance "..."   # also --title, --context
"$DAGQ" edit TASK --verify 'cargo fmt --all --check' --verify 'cargo test --locked --test plugin'
"$DAGQ" edit TASK --evidence e2e --paths 'src/**'          # --no-verify / --no-evidence / --no-paths empty a list
"$DAGQ" edit TASK --kind docs                               # docs, plugin, runtime or ci (reference/scope.md)
```

- Each given field replaces the old value; a repeatable flag (`--verify`, `--evidence`, `--paths`) replaces the whole list, so pass every value it should keep. At least one field is required. The values follow the `add` rules (no blank title or command, valid globs).
- **When it can change.** Only a `draft` or `submitted` task. Editing a `submitted` task does not yet restart its plan review (ADR-0044 decision 9 is not implemented): a review already running may pass the old text, so fix a proposal while it is sent back (`draft`) rather than while it is `submitted`. A `ready` task is refused: it changes only through plan review, which moves it back to `submitted` for a planner (ADR-0044 decision 14), or by taking it back with `draft ID`, editing it and submitting it again. An `in_progress` or finished task is refused; a run already claimed keeps the prompt it started with.
- The change is recorded as a `task_edited` event whose `from` and `to` hold only the fields that changed (under their task JSON names); `show ID` lists it with long texts cut to 300 characters, `show ID --full` has the whole values. Nothing changed, nothing recorded.
- Priority, goal and dependencies have their own commands: `set-priority`, `set-goal`, `dependency add|remove`.

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
