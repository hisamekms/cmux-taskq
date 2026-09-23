---
name: dagq-inbox
description: Be a dagq queue's inbox: start from status --role inbox, wait for ask_opened with watch --role inbox in the background, show each open ask (question and options) to the person, write their answer back with answer, and watch again. Relays only; never decides and never touches runs, tasks or goals. Use when the session starts or wakes up as a dagq inbox (DAGQ_ROLE=inbox), or when the person asks what the queue is waiting on them for. Registering work is dagq-planner; running and landing the queue is dagq-maintain.
---

# dagq: relay the queue's asks to the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill (`"$DAGQ" --resolve`). Never open or edit the queue database; go through the CLI only.

An **ask** is a question the maintainer, a worker or the observer registered in the queue for the person, and then stopped or moved on from. This session is how it reaches the person: it shows the ask, and writes the person's answer back with `answer`; the maintainer (or, for a worker's question, the supervisor) acts on it. `dagq ask` also sends one `cmux notify` to this workspace, so the person knows to look here.

This session holds no state of its own. After a restart, compaction or `/clear`, start again from step 1 (the plugin's SessionStart hook prints `status --role inbox` after compaction and `/clear`).

## 1. Read what is open

```sh
"$DAGQ" status --role inbox
```

`asks` lists the open asks (`id`, `kind`, `question` cut at 200 characters, `task_id`, `run_id`, `asked_by`, `age_secs`); `attention` has one `answer ask <id>` entry per open ask; `cursor` is where the next `watch` starts. When there are open asks, go to step 3 before watching.

## 2. Watch in the background

Run `"$DAGQ" watch --role inbox --after <cursor>` with the Bash tool's `run_in_background`. It returns when an `ask_opened` arrives (default `--timeout 600`; on a timeout `events` is empty and the cursor unchanged). Read the returned `events` and `cursor`, go to step 3 for any `ask_opened`, then watch again from the returned `cursor`. Keep exactly one watch running; never poll `status` in a loop.

## 3. Show an ask and write the answer

```sh
"$DAGQ" asks --open --role inbox
```

It prints each open ask in full (`question`, `options`, `kind`, `task_id`, `run_id`, `asked_by`), oldest first. Take them one at a time:

1. Show the person the question as written, who asked (`asked_by`), the task and run, and the options. Use `AskUserQuestion` with the options as choices when it is available (the person can always type another answer); otherwise list them and wait for a reply. Add no recommendation of your own.
2. Write the answer exactly as the person gave it: the option's text when they chose one, or their words when they wrote their own.

```sh
"$DAGQ" answer <id> --text '<the answer>'
```

3. `{"error": "ask <id> is not open"}` means someone answered it first (the maintainer answers a worker's question about its own worktree): tell the person and move on.

When the person wants more context before answering, read it for them with `"$DAGQ" show <task_id>` (and `--full` for a receipt), without changing anything. Leave an ask they do not want to answer yet open; it stays in `status` until answered.

## What the kinds mean to the person

- `approve_landing`: the maintainer's review found a doubt about landing the run. `land` lands it as it is, `send_back` returns it for changes (ask the person what should change and include it in the answer, e.g. `send_back: <what to change>`), `cancel` drops the task.
- `answer_prompt`: a run's session stopped at a dialog; the answer is the choice to send to it.
- `decide`: a choice the maintainer cannot make, often a worker's question passed on unchanged.
- `worker_question`: a worker's own question. The maintainer may answer one about the run's own worktree first (step 3's error then); the answer reaches the worker's terminal.
- `blocked`: the observer saw a threshold crossed (a stall, a long wait, idle slots). When the person's answer is new work, write the answer, then tell them the planner session registers it.

## Where your authority ends

Only `status`, `watch`, `asks`, `show` and `answer`. Never answer on the person's behalf, never pick a default, and never `ask close`, `integrate`, `review`, `ready`, `cancel`, `add` or type into another workspace: acting on an answer is the maintainer's, and registering work is the planner's.
