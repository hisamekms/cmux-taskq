---
name: dagq
description: Register and inspect dagq goals and tasks through the locally built dagq binary. Use when the user brings a development problem to queue for dagq (register it as a goal, decompose it into tasks with title, description, acceptance criteria, verification commands, dependencies and context), make tasks ready, list goals or tasks, check a goal's progress or a task's status or run result, close a goal after reviewing its tasks' receipts and follow_ups, adopt or reject a draft goal, record or read notes (observations), or find the dagq binary and queue database.
---

# dagq: register and inspect tasks

dagq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `dagq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

A goal is the problem several tasks solve together; a task is one unit of work a session executes in its own worktree. Registering and closing belong to the planner session (`dagq-planner`); the supervisor runs and lands the queue; its asks and attention go to the inbox (`dagq-inbox`); what a person does by hand (up / down, recovery, a review by hand) is `dagq-recover`.

Reference files, read only when needed: `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/locate.md` (install, version warnings, missing or moved queue), `reference/inspect.md` (every inspect command, its fields, task and run statuses, `graph`, priority, goal editing) and `reference/goal-close.md` (closing a goal), all in the same directory.

## 1. Locate the binary and the queue

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/dagq" --resolve
```

It prints the `binary`, `binary_version`, `plugin_version` and the queue (`db`, `db_exists`, `runs_dir`, `source`). Report `binary_version` and `db` to the user the first time in a session. Then:

- `{"error": ...}` (no binary): pass the message on (it names the install steps) and retry once installed.
- `{"warning": ...}` on stderr (plugin and binary differ in major.minor): report it and continue.
- `db_exists: false`: run `"${CLAUDE_PLUGIN_ROOT}/bin/dagq" init` once, unless the repository was moved or renamed; then do not `init` and read `reference/locate.md`.

The queue is per repository, resolved from the current directory: run the launcher inside the tasks' repository. Use `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` below.

## 2. Register a goal and decompose it into tasks

Hear the problem → register a goal (`goal add`) → decompose it into tasks, each registered with `add --goal` → make them `ready`. Each task's prompt shows the goal, its dependencies' receipt summaries and landed commits, and its siblings in progress, so siblings decide names and boundaries alike.

Skip the goal only for a one-shot task that finishes the problem by itself (a typo fix, a clippy warning, a version bump). If a second task will exist, or a later task needs to know what this one decided (a name, a boundary, a format), register a goal. When unsure, register it.

### Register the goal

Collect from the user, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed reference document, relative to the repository root).

```sh
"$DAGQ" goal add "TITLE" --description "..." --acceptance "..." --constraints "..." --doc docs/adr/NNNN-name.md
```

A goal has no verification commands; a goal-level check belongs in a final task that depends on all the others. Its one state is draft or open: `goal add --draft` registers a proposal whose tasks are never claimed, even when `ready`. Adopt it with `goal ready ID`, or reject it with `goal close ID --verdict abandoned`. Review the observer job's drafts, notes and `blocked` asks with the user per `reference/observer.md`.

### Register the tasks

Split the goal into tasks one session can finish in one worktree. Collect per task: title (one line), description (what to change and where), acceptance (how a reviewer decides it is done), verification commands (run by `integrate` after its rebase; repeat `--verify`), dependencies (tasks that must be `completed` first; repeat `--depends-on`, may cross goals), `--context` (why it exists and what to read first, when the goal does not say it), and `--evidence` (receipt checks the run must report as `passed` with evidence: `tests`, `e2e` or `subagent_review`; repeatable). A missing check parks the run (`needs_session`, `evidence_missing`) for a resumed session. `--paths GLOB` (repeatable) limits what the task may change: a run changing more parks (`scope_violation`) and never lands. Pick `--verify`, `--paths` and `--evidence` by kind of change (docs: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`) per `reference/scope.md`.

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

`--priority LEVEL` (default `normal`; `set-priority TASK LEVEL` while `draft` or `ready`) orders claiming: `interrupt` (a rare cut-in, never routine), `urgent` (a defect stopping operation), `high` (a prerequisite of other work), `normal`, `low` (deferred). Claim order: effective priority (own, or higher from ready tasks waiting on it), `unblocks`, ID. Mark urgency this way, never by drafting other tasks or bending dependencies; see `reference/inspect.md`.

## 3. Inspect

- `"$DAGQ" goal list`: every goal with its task counts by status.
- `"$DAGQ" goal show ID`: the goal, its tasks (`id`, `title`, `status`), its latest events.
- `"$DAGQ" list`: one page of unfinished tasks, newest first, as `{"tasks", "next", "total"}`. When `next` is not null, pass `--before NEXT` with the same filters for more. Filters: `--status`, `--all`, `--goal ID`.
- `"$DAGQ" show ID`: the task, its dependencies, the latest run and the latest 10 events.
- `"$DAGQ" graph [--goal ID]`: unfinished tasks with what they wait for and how many they release; `critical` is the chain that holds back the most work, and `candidates` the claim order.
- `"$DAGQ" notes [--goal ID] [--task ID] [--since CURSOR]`: notes (`observation` events) oldest first; `note --task ID | --run RUN_ID | --goal ID --text "..." [--kind SLUG]` records one. `show` and `goal show` list the latest 5.
- `"$DAGQ" stats [--since CURSOR] [--goal ID]`: time per run and goal, and `alerts`; `next_cursor` feeds `--since`.

`show`, `goal show` and `doctor` cut long texts to 300 characters (`truncated: true`); add `--full` only for whole texts, every run, event payloads (a receipt) or `run_dir`. Fields and statuses: `reference/inspect.md`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. A Stop hook, an idle session or a receipt file is not success. Summarize: task status, latest run status, branch and commit, and the next step.

A goal is closed once, by the planner, after every task is `completed` or `canceled`, the drafts from receipts' `follow_ups` are decided with the user, and the receipts' `summary` meets the goal's acceptance (gaps become new tasks on it first). Read `reference/goal-close.md` before running `goal close`.
