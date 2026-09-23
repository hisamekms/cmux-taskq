---
name: dagq-maintain
description: Keep a dagq queue running as its maintainer: start the runtime with up, stop it with down, read status (supervisor health, unfinished runs, attention, cursor), and wait for attention with watch in the background, then report it and route it; register an ask instead of waiting when a decision is the user's. Use when the session starts or wakes up as a dagq maintainer, when the user asks to start, stop, or check the supervisor, or asks what in the queue needs attention now. Landing a run is dagq-land; answering, resuming, or closing a run's session is dagq-session; a stuck lease is dagq-recover.
---

# dagq: keep the queue running and watch it

Prerequisite: resolve the launcher as in the `dagq` skill (`DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`, `"$DAGQ" --resolve`); if it is not in context, read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first. Never open or edit the queue database; go through the CLI only.

Roles: the **supervisor** is the resident `dagq supervise` process that claims ready tasks and runs each in its own worktree and cmux workspace; the **maintainer** is this session, which starts and stops the runtime, watches, reports, and lands a run when its subagent review passes, asking the user only on doubt (dagq-land); a **worker** is the Claude session of one run.

This session holds no state of its own. After a restart, compaction or `/clear`, start again from step 2: `status` rebuilds everything needed (a maintainer session gets it from the plugin's SessionStart hook after compaction and `/clear`).

## 1. Start the runtime

```sh
"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"            # add --parallel N (default 4)
"$DAGQ" up --in-cmux --plugin-dir "$CLAUDE_PLUGIN_ROOT"  # only when the preflight sends you there
```

Always start the supervisor through `up`, never in a workspace of your own. `up` is idempotent: run it again whenever unsure. From inside the maintainer session its `maintainer.outcome` is `skipped`, which is normal. Report the `supervisor.outcome` (`started`, `reused`, `restarted`) and the `doctor` summary it ends with. If `up` fails because cmux refuses a connection from outside its terminals, or takes long because it drains a supervisor of another version, read `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/up-down.md` before acting.

## 2. Read status

```sh
"$DAGQ" status
```

Read, in this order:

1. `supervisors`: healthy is `registered: true`, `alive: true`, `stale: false`. Empty, or `stale: true`, means nothing serves the queue: run `up` again, and report a stale row whose process is still alive to the user (it is stopped with `down --force`, never by you).
2. `attention`: what waits for the user or you now. Each entry has `run_id`, `task_id`, `status`, `kind`, `last_error` and a fixed `next`. Route it:
   - `review and integrate` (`awaiting_integration`): the `dagq-land` skill.
   - `resuming (runtime)` (`needs_session`): the supervisor is resolving it; nothing to do.
   - `resume session` (`needs_session` the runtime could not resolve), `inspect and close workspace` (`failed`), `send /exit` (exit request timed out), `answer the prompt in workspace <id>` (`kind` `prompt_waiting`: a worker stopped at a dialog), `send the answer of ask <id> to the worker and close it`: the `dagq-session` skill.
   - `recover run` (`kind` `runtime_error`: an unfinished run its supervisor gave up, left without a lease): the `dagq-recover` skill.
   - `restart supervisor` (`supervisor_stale`, `supervisor_stopped`): `up` as in step 1.
   - `push main` (`push_failed` on an `integrated` run): `integrate` landed it but could not push; the `dagq-land` skill, step 5.
   - `read the answer of ask <id> and close it` (`kind` `ask_answered`): the user answered an ask; act on it as in step 4.
   - `answer ask <id>` (`kind` `ask_opened`): for the inbox, not for you; `status --role maintainer` leaves these out. A `worker_question` is yours: `dagq-session`, section 2.
   - `delivering the answer of ask <id> (runtime)`: nothing to do.
3. `runs`: unfinished runs with their leases. A run in progress needs nothing from you.
4. `asks`: the open asks (`id`, `kind`, the first 200 characters of `question`, `task_id`, `run_id`, `asked_by`, `age_secs`), waiting for the user's answer through the inbox.
5. `cursor`: the newest event id, where the next `watch` starts.

Report the attention entries to the user in one short list (task, status, `next`, the gist of `last_error`). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/status.md` lists every field and run state; read it only when an entry is unclear. For one task use `"$DAGQ" show ID` (compact; `--full` only when a step asks for it). Use `doctor` only to diagnose a stuck run, never to poll.

## 3. Watch in the background

Do not poll `status`, `show` or `doctor` in a loop. Wait for the next attention with `watch`:

1. Take `cursor` from the last `status` (or the last `watch`).
2. Run `"$DAGQ" watch --after <cursor> --role maintainer` with the Bash tool's `run_in_background`. It blocks until an attention event for the maintainer arrives (an `ask_answered` included, an `ask_opened` not) or the supervisors change (default `--timeout 600`).
3. When it finishes, read its `events`, `supervisors_changed`, `supervisors` and `cursor`. Report each attention event to the user as in step 2; for a supervisor change, run `status` and act on step 2.1. On a timeout `events` is empty and the cursor is unchanged.
4. Go back to 2 with the returned `cursor`. Keep exactly one watch running at a time.

`watch` and `events --after <cursor>` (the same events without waiting) only read the queue. **Never call `integrate` because a watch returned**; landing goes through the `dagq-land` review, which lands on a pass and asks the user only on doubt (step 4).

## 4. Ask instead of waiting

When a run needs a decision you cannot make yourself (landing on doubt, a choice between options, anything the user must approve), do not wait at the terminal or use `AskUserQuestion`: register an ask and move on to the next attention.

```sh
"$DAGQ" ask --kind <approve_landing|answer_prompt|decide> --run <run_id> \
  --question "<what to decide and why, self-contained>" --option "<choice>" --option "<choice>"
```

Leave that run alone until its answer arrives as an `ask_answered` from your `watch`; read it with `"$DAGQ" asks --role maintainer`, act on it, then `"$DAGQ" ask close <id>`. Before your first ask, read the asks section of `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/status.md` (kinds, `--task`, duplicates, withdrawing).

## 5. Where your authority ends

- Land without asking when the `dagq-land` review passes (`integrate` also pushes `main`); on doubt (an acceptance mismatch, changes outside the task, review findings) register an `approve_landing` ask (step 4) and land only once its answer says so.
- Report and wait: pushing `main` by hand after a `push_failed`, `down --force`, `recover`, changing a task's acceptance, and anything outside a run's own worktree need the user's go-ahead.
- Do yourself, without asking: `up`, `status`, `watch`, `show`, `review`, `integrate` after a passing review, answering a run's trust or permission prompt about its own worktree (`dagq-session`), and reporting.
- A receipt's `follow_ups` are registered by `integrate` as `draft` tasks; making one `ready` waits for the user (`dagq-land`). Registering other new work (a new goal, a gap in a goal) follows the `dagq` skill once the user agrees.

## 6. Stop the runtime

```sh
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
"$DAGQ" down --force    # kill it now; only with the user's consent
```

Use `--wait` before replacing the binary or shutting down, and send `/exit` to any run whose exit request timed out first (`dagq-session`), since the drain waits for it. `down` never closes the maintainer, inbox or planner workspaces or the workers' sessions. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/up-down.md` has the outcomes, the in-cmux case and where the logs are.
