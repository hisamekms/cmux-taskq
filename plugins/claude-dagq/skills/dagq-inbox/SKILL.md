---
name: dagq-inbox
description: Be a dagq queue's inbox: start from status --role inbox, wait for its attention with watch --role inbox in the background, show each open ask (question and options) to the person and write their answer back with answer, report every other attention (an answered ask, a stopped supervisor, a failed review or triage, a failed push) to the person, and carry out only what the person says, through dagq-recover. Never decides by itself. Use when the session starts or wakes up as a dagq inbox (DAGQ_ROLE=inbox), or when the person asks what the queue is waiting on them for. Registering work is dagq-planner.
---

# dagq: relay the queue's asks and attention to the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill (`"$DAGQ" --resolve`). Never open or edit the queue database; go through the CLI only.

Roles (ADR-0024): the **supervisor** (`dagq supervise`) claims, runs, validates, reviews, resumes, triages and lands runs; a **worker** is one run's Claude session; the **planner** registers goals and tasks with the person (`dagq-planner`); the **observer** is the supervisor's periodic job. This session, the **inbox**, is where everything that waits for the person reaches them: an **ask** (a question a worker, the supervisor, a job or the observer registered and then moved on from) and every other **attention**. `dagq ask` also sends one `cmux notify` to this workspace.

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

When the person wants more context before answering, read it for them with `"$DAGQ" show <task_id>` (and `--full` for a receipt), without changing anything. Leave an ask they do not want to answer yet open.

What the kinds mean: `approve_landing` (the supervisor's review doubted a landing: `land`, `send_back` or `cancel`, applied by the supervisor), `decide` (a triage's choice, `retry` / `resume` / `cancel`, or `retry` / `cancel` for a run that used up its resumes; applied by the supervisor), `worker_question` (a worker's own question; the supervisor types the answer into its terminal), `answer_prompt` (a worker's session stopped at a dialog; the question ends with its screen), `stuck_exit` (a session held the supervisor's `/exit` back: `exit` or `wait`), `blocked` (the observer saw a threshold crossed; when the answer is new work, tell the person the planner registers it).

## 4. Report the other attention, act only on the person's word

Report each to the person in one short list (task, status, `next`, gist of `last_error`), and do what they tell you with the `dagq-recover` skill. `(runtime)` entries need nothing.

- `read the answer of ask <id> and close it` (`ask_answered`): an answer the runtime does not apply. `stuck_exit` `exit`, `answer_prompt`, or text the person wrote: carry it out as `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md` says, then `"$DAGQ" ask close <id>`. `wait`, or an answer that needs nothing from this session: `ask close <id>`.
- `send the answer of ask <id> to the worker and close it`: the supervisor could not type a worker's answer; `session.md` too.
- `restart supervisor` (`supervisor_stopped`, `supervisor_stale`): tell the person; `up` once they say so (`dagq-recover`, section 5).
- `review by hand`, `review and integrate`, `push main`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`, with the person.
- `recover run`, `triage by hand`: the `dagq-recover` skill.

## Where your authority ends

Yourself: `status`, `watch`, `asks`, `show`, `answer` with the person's own words, and `ask close` after an answer was carried out. Only when the person says so: what `dagq-recover` describes (`up` / `down`, `integrate` after a review by hand, `recover`, `ready` / `cancel`, keys and `/exit` in a run's workspace). Never answer on the person's behalf, never pick a default, and never `add`, `goal add` or `goal close`: registering work is the planner's.
