---
name: dagq
description: Register and inspect dagq goals and tasks through the locally built dagq binary. Use when the user brings a development problem to queue for dagq (register it as a goal, decompose it into tasks with title, description, acceptance criteria, verification commands, dependencies and context), make tasks ready, list goals or tasks, check a goal's progress or a task's status or run result, close a goal after reviewing its tasks' receipts and follow_ups, adopt or reject a draft goal, record or read notes (observations), or find the dagq binary and queue database.
---

# dagq: register and inspect tasks

dagq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `dagq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

A goal is the problem several tasks solve together; a task is one unit of work a session executes in its own worktree. Registering and closing belong to the planner session (`dagq-planner`); asks go to the inbox (`dagq-inbox`); the maintainer runs the queue (`dagq-maintain`), lands runs (`dagq-land`), acts on sessions (`dagq-session`) and recovers stuck runs (`dagq-recover`).

Reference files, read only when needed: `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/locate.md` (install, version warnings, missing or moved queue), `reference/inspect.md` (every inspect command, its fields, task and run statuses, `graph`, goal editing) and `reference/goal-close.md` (closing a goal), all in the same directory.

## 1. Locate the binary and the queue

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/dagq" --resolve
```

It prints the `binary`, `binary_version`, `plugin_version` and the queue (`db`, `db_exists`, `runs_dir`, `source`). Report `binary_version` and `db` to the user the first time in a session. Then:

- `{"error": ...}` (no binary): pass the message on; it names the install steps. Retry after the user installs it.
- `{"warning": ...}` on stderr (plugin and binary differ in major.minor): report it and continue.
- `db_exists: false`: run `"${CLAUDE_PLUGIN_ROOT}/bin/dagq" init` once, unless the repository was moved or renamed; then do not `init` and read `reference/locate.md`.

The queue is one per repository, resolved from the current directory, so run the launcher inside the repository the tasks belong to. Use `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` below.

## 2. Register a goal and decompose it into tasks

Hear the problem → register a goal (`goal add`) → decompose it into tasks, each registered with `add --goal` → make them `ready`. Every task of a goal is shown the goal's description, acceptance and constraints, its dependencies' receipt summaries and landed commits, and its siblings in progress, so sibling tasks make the same naming and boundary decisions.

Skip the goal only for a one-shot task that finishes the problem by itself (a typo fix, a clippy warning, a version bump). If a second task will exist, or a later task needs to know what this one decided (a name, a boundary, a format), register a goal. When unsure, register it.

### Register the goal

Collect from the user, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed reference document, relative to the repository root).

```sh
"$DAGQ" goal add "TITLE" --description "..." --acceptance "..." --constraints "..." --doc docs/adr/NNNN-name.md
```

A goal has no verification commands; a goal-level check belongs in a final task that depends on all the others. Its one state is draft or open: `goal add --draft` registers a proposal whose tasks are never claimed, even when `ready`. Adopt it with `goal ready ID`, or reject it with `goal close ID --verdict abandoned`. Review the observer job's drafts, notes and `blocked` asks with the user per `reference/observer.md`.

### Register the tasks

Split the goal into tasks one session can finish in one worktree. Collect per task: title (one line), description (what to change and where), acceptance (how a reviewer decides it is done), verification commands (run by `integrate` after its rebase; repeat `--verify`), dependencies (tasks that must be `completed` first; repeat `--depends-on`, may cross goals), `--context` (why it exists and what to read first, when the goal does not say it), and `--evidence` (receipt checks the run must report as `passed` with evidence: `tests`, `e2e` or `subagent_review`; repeatable). A receipt lacking a required check parks the run as `needs_session` (`evidence_missing`) for a resumed session. `--paths GLOB` (repeatable) limits what the task may change: a run changing more parks (`scope_violation`) and never lands. Pick `--verify`, `--paths` and `--evidence` by kind of change (docs: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`) per `reference/scope.md`.

```sh
"$DAGQ" add "TITLE" --goal 1 \
  --description "..." --acceptance "..." --context "..." \
  --verify "cargo test --locked" --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --evidence e2e \
  --depends-on 3
"$DAGQ" ready ID
"$DAGQ" candidates
```

A one-shot task omits `--goal`. `add` registers a `draft`, `ready` makes it runnable, `candidates` lists ready tasks whose dependencies are all `completed`. A ready task missing from `candidates` is blocked: show the blocking IDs from `show ID`'s `dependencies`. `draft ID` takes a task back for editing, `cancel ID` drops it, `dependency add|remove TASK PREDECESSOR` changes prerequisites of a draft or ready task (cycles are rejected). `set-goal` and `goal edit` are in `reference/inspect.md`, `set-paths` in `reference/scope.md`.

## 3. Inspect

- `"$DAGQ" goal list`: every goal with its task counts by status.
- `"$DAGQ" goal show ID`: the goal, its tasks (`id`, `title`, `status`), its latest events.
- `"$DAGQ" list`: one page of unfinished tasks, newest first, as `{"tasks", "next", "total"}`. More pages exist only when `next` is not null; then pass `--before NEXT` with the same filters. Filters: `--status`, `--all`, `--goal ID`.
- `"$DAGQ" show ID`: the task, its dependencies, the latest run and the latest 10 events.
- `"$DAGQ" graph [--goal ID]`: unfinished tasks with what they wait for and how many they release; `critical` is the chain that holds back the most work, and `candidates` the order the supervisor claims in. Use it to decide what to make `ready` next.
- `"$DAGQ" notes [--goal ID] [--task ID] [--since CURSOR]`: notes (`observation` events) oldest first; `note --task ID | --run RUN_ID | --goal ID --text "..." [--kind SLUG]` records one. `show` and `goal show` list the latest 5.
- `"$DAGQ" stats [--since CURSOR] [--goal ID]`: where time goes per run and goal (work, validation, waiting to land) and `alerts` over thresholds; pass `next_cursor` to `--since` for only newer runs.

`show`, `goal show` and `doctor` are compact: long texts are cut to 300 characters ending in `…` with `truncated: true`. Add `--full` only for the whole text, every run, every event payload (a receipt) or `run_dir`. Field lists and statuses are in `reference/inspect.md`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. A Stop hook, an idle session or a receipt file is not success: the supervisor checks the receipt against Git, and `integrate` runs the verification. Summarize: task status, latest run status, branch and commit, and the next step.

A goal is closed once, by the planner, after every task is `completed` or `canceled`, the draft tasks `integrate` registered from receipts' `follow_ups` were made ready or canceled by the user, and the receipts' `summary` was compared with the goal's acceptance; gaps become new tasks on the same goal first. Read `reference/goal-close.md` before running `goal close`.
