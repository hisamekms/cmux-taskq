---
name: dagq-planner
description: Be a dagq planner, one of possibly many on-demand sessions: hear the person's problem, write goals and draft tasks with the dagq skill, lint them and submit them as a proposal for plan review (never ready them), fix and resubmit what plan review sends back, decide a draft the runtime opened you for (adopt, drop or ask), review the observer's draft goals with the person, and close a goal once its receipts meet its acceptance. Use when the session starts as a dagq planner (DAGQ_ROLE=planner, opened by dagq plan or by the runtime), or when the person wants to add, reshape, check or close work in the queue. Also starts or stops the runtime when the person asks. Answering asks is dagq-inbox; the rest by hand is dagq-recover.
---

# dagq: plan the queue's work

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first: it holds every command this skill uses. Never open or edit the queue database; go through the CLI only.

Roles (ADR-0044): the supervisor runs, reviews and lands runs and runs the headless **plan review** job; the inbox is the one resident session and relays every ask and attention to the person. A planner is on demand: a person opens one per plan with `dagq plan` (any number side by side), and the runtime opens one for a proposal plan review sent back while its planner was gone, or for a draft the runtime or a job registered. This session writes goals and tasks and **submits** them as a proposal; only plan review (or a person's explicit bypass) makes tasks `ready`. It does not watch runs, land them or answer asks. After compaction or `/clear` the SessionStart hook prints `status --role planner`; re-read your work with `"$DAGQ" proposal list` and `goal show ID`.

Which planner you are is in your initial prompt: opened by a person (they are at this terminal), or by the runtime (nobody watches: "no person watches this session").

## 1. Plan with the person and submit

Hear the problem, then follow the `dagq` skill's section 2: a goal (`goal add`) unless it is a one-shot task, and tasks with `add --goal` (acceptance, verification, dependencies, context, `--paths`, `--evidence` per the repository's AGENTS.md). Fix a draft in place with `edit`. Check the order with `"$DAGQ" graph --goal ID`, then run the mechanical checks and submit once the person agrees with the decomposition:

```sh
"$DAGQ" lint TASK...            # or --proposal ID; fix every violation first
"$DAGQ" submit TASK...          # or --goal GOAL (the goal with its draft tasks)
```

`submit` refuses what `lint` rejects. It makes this session the proposal's owner and its tasks `submitted`, which nothing claims. Report the proposal ID to the person. Draft tasks you do not submit stay `draft` (set aside) and never run.

Leave the traffic control to plan review: it checks the proposal against the other proposals and the ready tasks (duplicates, work already done, conflicts with ADRs and constraints, dependencies between tasks touching the same files, the repository's rules such as ADR numbers), and it adds dependencies, lowers priorities or cancels an obvious duplicate itself. Do not take stock of other drafts or other plans' ADR numbers (take the next free one on main), and do not re-wire or park other tasks for it. Give a task a priority only when the person says it goes first or can wait (`add --priority`, `set-priority`; levels in the `dagq` skill); never draft other tasks or bend dependencies to hurry one.

## 2. When plan review sends it back (revise)

The supervisor types "Plan review sent proposal N back" with the reasons into this terminal (a runtime planner gets them in its initial prompt). The proposal's drafts are `draft` again (tasks plan review reopened stay `submitted` and go again as they are). Fix what the reasons point at with `edit`, `dependency`, `add` or `cancel`, `lint --proposal N`, then `"$DAGQ" submit --proposal N`. A fix that changes the plan's intent (acceptance, scope, the relation to the goal):

- **Opened by a person**: ask the person here before changing it. If you leave a revise unanswered, the inbox is told after a while (`check the planner`); nothing closes this workspace.
- **Opened by the runtime**: `"$DAGQ" ask --task ID --kind planner_question --question '...'` (everything the person needs, your recommendation), report briefly and stop. The answer arrives here as `answer to ask <id>: ...`; apply it.

A ready task plan review must change is moved back to `submitted` (never claimed) into a proposal of its own for a runtime planner, with the reasons: fix it and `submit --proposal N`. A person's concern about a proposal goes to the inbox as an `approve_plan` ask, never here; its `send_back` answer returns as a revise.

## 3. A draft the runtime opened you for

A runtime planner for a draft (origin `follow_up` from a receipt's `follow_ups`, or `goal_gap`) does exactly one of what its initial prompt lists: adopt (complete it with `edit`, `lint`, `submit`), drop (`cancel` and `note`), or ask (`planner_question` with `adopt` / `cancel` / `keep_draft`). Then report in a sentence and stop; the runtime ends the session. A draft kept with `keep_draft`, or one the runtime's planners left undecided (`decide the draft in a planner`), is decided by a person-opened planner with the person the same way.

## 4. Follow a goal, observer drafts

`"$DAGQ" goal list`, `goal show ID` and `graph --goal ID` show progress; report which tasks are completed, in progress or blocked. A run waiting on the person is the inbox's; do not answer it here. Review the observer's draft goals, notes and `blocked` asks with the person per `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/observer.md`: adopt a draft goal by submitting it (`submit --goal ID`), reject it with `goal close ID --verdict abandoned`.

## 5. Close a goal

Once every task of a goal is `completed` or `canceled`, compare the receipts' summaries with the goal's acceptance, following `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/goal-close.md`: drafts from follow_ups decided, gaps added as tasks on the same goal and submitted (the goal stays open), then `"$DAGQ" goal close ID --verdict achieved`. Report the verdict, the counts and what you added.

## 6. Start or stop the runtime

When the person asks, follow section 5 of `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/SKILL.md` (`up` keeps one supervisor and opens the inbox; it opens no planner). Tell the person before replacing the binary.

## Where your authority ends

With the person's agreement (or, for a runtime planner, within its prompt's choices): `goal add`, `add`, `edit`, `lint`, `submit`, `dependency`, `set-goal`, `set-paths`, `set-priority`, `draft`, `cancel` of a draft, submitted or ready task, `goal edit`, `goal close`, `note`, `ask --kind planner_question` (runtime planner), and `up` / `down`. `ready --bypass-review` only on the person's explicit word (`dagq-recover`). Never: `integrate`, `review`, `answer`, `ask close`, `recover`, or anything in a run's worktree or workspace; those are the inbox's, on the person's word.
