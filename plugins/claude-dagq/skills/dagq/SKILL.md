---
name: dagq
description: Register and inspect dagq goals and tasks through the locally built dagq binary. Use when the user brings a development problem to queue for dagq (register it as a goal, decompose it into tasks with title, description, acceptance criteria, verification commands, dependencies and context), lint and submit them for plan review, edit a draft or submitted task, list goals or tasks, check a goal's progress or a task's status or run result, close a goal after reviewing its tasks' receipts and follow_ups, adopt or reject a draft goal, record or read notes (observations), or find the dagq binary and queue database.
---

# dagq: register and inspect tasks

dagq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `dagq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

A goal is the problem several tasks solve together; a task is one unit of work a session executes in its own worktree. Registering, submitting and closing belong to a planner session (`dagq-planner`); the supervisor runs a headless plan review of each submitted proposal, then runs and lands the queue; its asks and attention go to the inbox (`dagq-inbox`); what a person does by hand (up / down, recovery, a review by hand) is `dagq-recover`.

Reference files, read only when needed: `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/locate.md` (install, version warnings, missing or moved queue), `reference/inspect.md` (inspect commands, fields, statuses, `graph`, priority, editing) and `reference/goal-close.md` (closing a goal), all in the same directory.

## 1. Locate the binary and the queue

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/dagq" --resolve
```

It prints `binary`, `binary_version`, `plugin_version` and the queue (`db`, `db_exists`, `runs_dir`, `source`). Report `binary_version` and `db` to the user once per session. Then:

- `{"error": ...}` (no binary): pass the message on (it names the install steps) and retry once installed.
- `{"warning": ...}` on stderr (plugin and binary differ in major.minor): report it and continue.
- `db_exists: false`: run `"${CLAUDE_PLUGIN_ROOT}/bin/dagq" init` once, unless the repository was moved or renamed; then do not `init` and read `reference/locate.md`.

The queue is per repository, resolved from the current directory: run the launcher inside the tasks' repository. Use `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` below.

## 2. Register a goal and decompose it into tasks

Hear the problem → `goal add` → decompose it into tasks, each registered with `add --goal` → `lint` and `submit` them for plan review, which makes them `ready`. Each task's prompt shows the goal, its dependencies' receipt summaries and landed commits, and siblings in progress, so siblings agree on names and boundaries.

Skip the goal only for a one-shot task that finishes the problem by itself (a typo fix, a clippy warning, a version bump). If a second task will exist, or a later task needs to know what this one decided (a name, a boundary, a format), register a goal. When unsure, register it.

### Register the goal

Collect from the user, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed document, relative to the repository root).

```sh
"$DAGQ" goal add "TITLE" --description "..." --acceptance "..." --constraints "..." --doc docs/adr/NNNN-name.md
```

A goal has no verification commands; a goal-level check is a final task depending on all the others. Its state is draft or open: `goal add --draft` registers a proposed goal whose tasks are never claimed, even when `ready`. Adopt it by submitting it (`submit --goal ID`; a `pass` lifts the draft), or reject it with `goal close ID --verdict abandoned`. Review the observer's drafts, notes and `blocked` asks with the user per `reference/observer.md`.

### Register the tasks

Split the goal into tasks one session finishes in one worktree. Collect per task: title (one line), description (what to change and where), acceptance (how a reviewer decides it is done), verification commands (run by `integrate` after its rebase; repeat `--verify`), dependencies (tasks that must be `completed` first; repeat `--depends-on`, may cross goals), `--context` (why it exists and what to read first, when the goal does not say it), and `--evidence` (receipt checks the run must report as `passed` with evidence: `tests`, `e2e` or `subagent_review`; repeatable). A missing check parks the run (`needs_session`, `evidence_missing`) for a resume. `--paths GLOB` (repeatable) limits what the task may change: a run changing more parks (`scope_violation`) and never lands. Pick `--verify`, `--paths`, `--evidence` and `--kind` (docs, plugin, runtime, ci) by kind of change per `reference/scope.md`.

```sh
"$DAGQ" add "TITLE" --goal 1 \
  --description "..." --acceptance "..." --context "..." \
  --verify "cargo fmt --all --check" --verify "cargo test --locked" \
  --evidence e2e --kind runtime --depends-on 3
"$DAGQ" lint ID...               # the fixed rules; each violation {code, task_id, reason}
"$DAGQ" submit ID...             # or --goal GOAL; prints the proposal
"$DAGQ" candidates
```

A one-shot task omits `--goal`. `add` registers a `draft`, never claimed. `submit` (which refuses what `lint` rejects) makes the tasks `submitted`, one proposal owned by this session; nothing claims them. The supervisor's plan review job checks each proposal in turn: `pass` makes its tasks `ready`, `revise` returns them to `draft` with reasons for their planner to fix and `submit --proposal ID` again, `concern` asks the person through the inbox. `proposal list` / `proposal show ID` read proposals. Only plan review readies a task; `ready --bypass-review` skips it on a person's explicit word. `candidates` lists ready tasks whose dependencies are all `completed`; a ready task missing from it is blocked (see `show ID`). `edit` changes a draft or submitted task, `draft ID` takes a ready or submitted task back, `cancel ID` drops it, `dependency add|remove TASK PREDECESSOR` changes prerequisites. `set-goal`, `goal edit`, `edit`: `reference/inspect.md`; `set-paths`: `reference/scope.md`.

`--priority LEVEL` (default `normal`; `set-priority TASK LEVEL` while `draft` or `ready`) orders claiming: `interrupt` (a rare cut-in, never routine), `urgent` (a defect stopping operation), `high` (a prerequisite of other work), `normal`, `low` (deferred). Claim order: effective priority (own, or higher from ready tasks waiting on it), `unblocks`, ID. Never mark urgency by drafting tasks or bending dependencies (`reference/inspect.md`).

## 3. Inspect

`goal list`, `goal show ID`, `list` (unfinished tasks, paged: `--before NEXT` while `next` is not null), `show ID`, `graph [--goal ID]` (what waits on what, `critical`, claim order), `notes` / `note` (observations) and `stats` (time per run and goal, `alerts`). `show`, `goal show` and `doctor` cut long texts; `--full` gives them whole. Flags, fields and statuses: `reference/inspect.md`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. A Stop hook, an idle session or a receipt file is not success. Summarize: task status, latest run status, branch and commit, and the next step.

A goal is closed once, by the planner, after every task is `completed` or `canceled`, the drafts from receipts' `follow_ups` are decided with the user, and the receipts' `summary` meets the goal's acceptance (gaps become new tasks on it first). Read `reference/goal-close.md` before running `goal close`.
