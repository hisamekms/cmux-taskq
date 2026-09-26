---
name: dagq-inbox
description: Be a dagq queue's inbox: start from status --role inbox, wait for its attention with watch --role inbox in the background, show each open ask (question and options) to the person and write their answer back with answer, report every other attention (an answered ask, a stopped supervisor, a failed review, triage or plan review, an unresponsive planner, a failed push) to the person, and carry out only what the person says, through dagq-recover. Never decides by itself. Use when the session starts or wakes up as a dagq inbox (DAGQ_ROLE=inbox), or when the person asks what the queue is waiting on them for. Registering work is dagq-planner.
---

# dagq: relay the queue's asks and attention to the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill (`"$DAGQ" --resolve`). Never open or edit the queue database; go through the CLI only.

Roles (ADR-0044): the **supervisor** (`dagq supervise`) claims, runs, validates, reviews, resumes, triages and lands runs, and runs the headless **plan review** of each submitted proposal; a **worker** is one run's Claude session; a **planner** is an on-demand session (a person opens any number with `dagq plan`, the runtime opens some itself) that writes goals and tasks and submits them (`dagq-planner`); the **observer** is the supervisor's periodic job. This session, the **inbox**, is the one resident session and where everything that waits for the person reaches them: an **ask** (a question a worker, the supervisor, a job or the observer registered and then moved on from) and every other **attention**. `dagq ask` also sends one `cmux notify` to this workspace.

This session holds no state of its own. After a restart, compaction or `/clear`, start again from step 1 (the plugin's SessionStart hook prints `status --role inbox` after compaction and `/clear`).

## 1. Read what waits

```sh
"$DAGQ" status --role inbox
```

`asks` lists the open asks (`id`, `kind`, `question` cut at 200 characters, `task_id`, `run_id`, `asked_by`, `age_secs`); `attention` has everything that waits, each with a fixed `next`; `cursor` is where the next `watch` starts. Handle open asks first (step 3), then the rest (step 4). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-inbox/reference/status.md` lists every field and `next`.

## 2. Watch in the background

Run `"$DAGQ" watch --role inbox --after <cursor>` with the Bash tool's `run_in_background`. It returns when an attention event arrives or the supervisors' health changes (default `--timeout 600`; on a timeout `events` is empty and the cursor unchanged). Read the returned `events`, `supervisors_changed` and `cursor`, handle them (steps 3 and 4), then watch again from the returned `cursor`. Keep exactly one watch running; never poll `status` in a loop.

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

3. `{"error": "ask <id> is not open"}` means the runtime closed it first (the dialog went away, the session exited): tell the person and move on.

A run waiting on an ask holds no `--parallel` slot (`status` `waiting`); once answered it returns when a slot frees and the supervisor goes on with it (`reference/status.md`, "Runs waiting for a person").

When the person wants more context before answering, read it for them with `"$DAGQ" show <task_id>` (and `--full` for a receipt), without changing anything. Leave an ask they do not want to answer yet open.

What the kinds mean: `approve_landing` (the supervisor's review doubted a landing: `land`, `send_back` or `cancel`, applied by the supervisor), `approve_plan` (plan review raised a concern about a proposal, or would send it back a third time: `ready`, `send_back` (add the person's reason: `send_back: <reason>`, returned to its planner) or `cancel` its tasks, applied by the supervisor), `decide` (a triage's choice, `retry` / `resume` / `cancel`, or `retry` / `cancel` for a run that used up its resumes; applied by the supervisor), `worker_question` (a worker's own question; the supervisor types the answer into its terminal), `planner_question` (a planner the runtime opened for a draft cannot decide it: `adopt`, `cancel` or `keep_draft`; the supervisor types the answer into that planner or hands it to a new one), `answer_prompt` (a worker's session stopped at a dialog; the question ends with its screen), `stuck_exit` (a session held the supervisor's `/exit` back, including one sent because its wrapper stopped heartbeating: `exit` or `wait`), `blocked` (the observer saw a threshold crossed; when the answer is new work, tell the person to plan it in a planner, `dagq plan`), `update_failed` (`retry` / `skip`) and `approve_update` (`install` / `skip`: then, on their word, run the question's command) from `up --auto-update`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/update.md`.

## 4. Report the other attention, act only on the person's word

Report each to the person in one short list (task, status, `next`, gist of `last_error`), and do what they tell you with the `dagq-recover` skill. `(runtime)` entries need nothing.

- `read the answer of ask <id> and close it` (`ask_answered`): an answer the runtime does not apply. `stuck_exit` `exit`, `answer_prompt`, or text the person wrote: carry it out as `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md` says, then `"$DAGQ" ask close <id>`. `wait`, or an answer that needs nothing from this session: `ask close <id>`.
- `send the answer of ask <id> to the worker and close it`: the supervisor could not type a worker's answer; `session.md` too.
- `decide the draft in a planner` (`draft_planner_exhausted`): the runtime's planners left a draft undecided; tell the person, who decides it in a planner of theirs (`dagq plan`).
- `plan review by hand` (`plan_review_failed`): the headless plan review of a proposal failed and it is held; it is not reviewed again by itself. Show the task and `last_error`; the person picks per `dagq-recover` section 8.
- `check the planner` (`planner_unresponsive`): the planner a revise went to did not submit again in time, or no runtime planner took it. Tell the person which proposal; they look at that planner's workspace. Nothing closes it.
- `install tool` (`run_env_program_missing`): a program `dagq.toml`'s `[run.env]` names (`RUSTC_WRAPPER` and the like) is not on the supervisor's PATH, so it claims and lands nothing. Show `last_error`; the person installs it or has a planner take it out of `dagq.toml`. It clears by itself.
- `report the update` (`update_installed`): `up --auto-update` put a new binary in place; tell the person its `version` and `commit`. Nothing to do or close.
- `restart supervisor` (`supervisor_stopped`, `supervisor_stale`): tell the person; `up` once they say so (`dagq-recover`, section 5).
- `review by hand`, `review and integrate`, `push main`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`, with the person.
- `recover run`, `triage by hand`: the `dagq-recover` skill.

## Where your authority ends

Yourself: `status`, `watch`, `asks`, `show`, `answer` with the person's own words, and `ask close` after an answer was carried out. Only when the person says so: what `dagq-recover` describes (`up` / `down` / `install`, `integrate` after a review by hand, `recover`, a retry `ready`, `ready --bypass-review`, `cancel`, keys and `/exit` in a run's workspace). Never answer on the person's behalf, never pick a default, and never `add`, `goal add` or `goal close`: registering work is the planner's.
