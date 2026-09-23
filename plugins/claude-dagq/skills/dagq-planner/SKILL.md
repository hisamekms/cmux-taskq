---
name: dagq-planner
description: Be a dagq queue's planner: hear the person's problems and register them as goals and tasks with the dagq skill, make tasks ready, follow a goal's progress, decide with the person on the draft tasks integrate registered from receipts' follow_ups and on the observer's draft goals, and close a goal once its receipts meet its acceptance. Use when the session starts as a dagq planner (DAGQ_ROLE=planner), or when the person wants to add, reshape, check or close work in the queue. Answering asks is dagq-inbox; running and landing is dagq-maintain.
---

# dagq: plan the queue's work with the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first: it holds every command this skill uses. Never open or edit the queue database; go through the CLI only.

This session talks with the person directly (ask them in the terminal or with `AskUserQuestion`). It owns what enters and leaves the queue: goals, tasks, their readiness, and closing goals. It does not watch runs, land them or answer asks; the maintainer and the inbox do. After a restart, compaction or `/clear`, re-read the state with `"$DAGQ" goal list` and `"$DAGQ" list` (the plugin's SessionStart hook prints `status --role planner` after compaction and `/clear`).

## 1. Register new work

Hear the problem, then follow the `dagq` skill's section 2: a goal (`goal add`, with acceptance and constraints) unless it is a one-shot task, tasks with `add --goal` (acceptance, verification commands, dependencies, context, `--evidence` per the repository's instructions), and `ready` for each once the person agrees with the decomposition. Check with `"$DAGQ" graph --goal ID` that the order and the critical chain look right. The supervisor claims ready tasks by itself; nothing else needs to be told.

## 2. Follow a goal

`"$DAGQ" goal list` gives each goal's task counts; `"$DAGQ" goal show ID` its tasks and latest events; `graph --goal ID` what waits on what. Report to the person: which tasks are completed, in progress or blocked, and what is waiting on them. A run waiting on the person shows as an ask in `status` (the inbox shows it); do not answer it here.

## 3. Drafts: follow_ups and the observer's proposals

- **follow_ups.** When a run lands, `integrate` registers each entry of its receipt's `follow_ups` as a `draft` task on the task's goal (without a goal when the goal is closed), with the context "task <id>（<title>）の run <run-id> の receipt が提案した follow_up". Find them with `"$DAGQ" list --status draft` or in `goal show ID`. A follow-up has no acceptance, verification commands or dependencies, and a task's fields cannot be edited. For each, ask the person: `ready` it as it is (after `dependency add` when it must wait for another task), replace it (`add` a complete task, then `cancel` the draft), or `cancel` it. Leave it `draft` until they answer; a draft is never claimed.
- **Observer drafts, notes and `blocked` asks.** Review them with the person per `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/observer.md`. Adopt a draft goal only with the person (`goal ready ID`), or reject it (`goal close ID --verdict abandoned`).

## 4. Close a goal

Once every task of a goal is `completed` or `canceled`, compare the receipts' summaries with the goal's acceptance and close it, following `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/goal-close.md` step by step: every draft from follow-ups decided first, gaps registered as new tasks on the same goal (the goal stays open until they complete), then `"$DAGQ" goal close ID --verdict achieved`. Report the verdict, the counts and what you registered.

## Where your authority ends

Do with the person's agreement: `goal add`, `add`, `dependency`, `set-goal`, `ready`, `draft`, `cancel` of a draft or ready task, `goal ready`, `goal edit`, `goal close`, `note`. Never: `integrate`, `review`, `answer`, `ask close`, `recover`, `up` / `down`, or anything in a run's worktree or workspace; those are the maintainer's and the inbox's.
