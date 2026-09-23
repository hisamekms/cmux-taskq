---
name: dagq-maintain
description: Keep a dagq queue running as its maintainer: start the runtime with up, stop it with down, read status --role maintainer, wait for attention with watch --role maintainer in the background and route it, register an ask instead of waiting when a decision is the person's, and act on answered asks. Use when the session starts or wakes up as a dagq maintainer, or is asked to start, stop, or check the supervisor, or what in the queue needs attention. Landing a run is dagq-land; answering, resuming, or closing a run's session is dagq-session; a stuck lease is dagq-recover.
---

# dagq: keep the queue running and watch it

Prerequisite: resolve the launcher as in the `dagq` skill (`DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`, `"$DAGQ" --resolve`); if it is not in context, read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first. Never open or edit the queue database; go through the CLI only.

Roles: the **supervisor** is the resident `dagq supervise` process that claims ready tasks and runs each in its own worktree and cmux workspace; the **maintainer** is this session, which starts and stops the runtime, watches, lands a run when its subagent review passes (dagq-land), and turns what it cannot decide into asks; a **worker** is the Claude session of one run. The **inbox** session shows asks to the person and writes their answers (`dagq-inbox`); the **planner** registers goals and tasks and closes goals (`dagq-planner`).

This session holds no state of its own. After a restart, compaction or `/clear`, start again from step 2: `status` rebuilds everything (the plugin's SessionStart hook prints it after compaction and `/clear`).

## 1. Start the runtime

```sh
"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"            # add --parallel N (default 4)
"$DAGQ" up --in-cmux --plugin-dir "$CLAUDE_PLUGIN_ROOT"  # only when the preflight sends you there
```

Always start the supervisor through `up`, which is idempotent: run it again whenever unsure. Inside this session `maintainer.outcome` is `skipped`, which is normal. Report `supervisor.outcome` and the `doctor` summary. If `up` fails because cmux refuses a connection from outside its terminals, or takes long because it drains a supervisor of another version, read `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/up-down.md` before acting.

## 2. Read status

```sh
"$DAGQ" status --role maintainer
```

Read, in this order:

1. `supervisors`: healthy is `registered: true`, `alive: true`, `stale: false`. Empty, or `stale: true`, means nothing serves the queue: run `up` again, and report a stale row whose process is still alive to the user (it is stopped with `down --force`, never by you).
2. `attention`: what waits for you now. Each entry has `run_id`, `task_id`, `status`, `kind`, `last_error` and a fixed `next`. Route it:
   - `review and integrate` (`awaiting_integration`): the `dagq-land` skill.
   - `resuming (runtime)` (`needs_session`): the supervisor is resolving it; nothing to do.
   - `resume session` (`needs_session` the runtime gave up), `inspect and close workspace` (`failed`), `send /exit`, `answer the prompt in workspace <id>` (`prompt_waiting`: a worker stopped at a dialog), `send the answer of ask <id> to the worker and close it`: the `dagq-session` skill.
   - `recover run` (`kind` `runtime_error`: an unfinished run left without a lease): the `dagq-recover` skill.
   - `restart supervisor` (`supervisor_stale`, `supervisor_stopped`): `up` as in step 1.
   - `push main` (`push_failed` on an `integrated` run): landed but not pushed; `dagq-land`, step 6.
   - `read the answer of ask <id> and close it` (`kind` `ask_answered`): an ask was answered; act on it as in step 4.
   - `delivering the answer of ask <id> (runtime)`: nothing to do.
3. `runs`: unfinished runs with their leases; one in progress needs nothing.
4. `asks`: the open asks (`id`, `kind`, `question` cut at 200 characters, `task_id`, `run_id`, `asked_by`, `age_secs`). They wait for the inbox. A `worker_question` does not wake your `watch`: answer one you see here when it is about the run's worktree (`dagq-session`, section 2); otherwise the inbox relays it.
5. `cursor`: the newest event id, where the next `watch` starts.

Report the attention in one short list (task, status, `next`, gist of `last_error`). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/status.md` lists every field and run state, for when an entry is unclear. For one task use `"$DAGQ" show ID` (`--full` only when a step asks). `doctor` diagnoses a stuck run; never poll with it.

## 3. Watch in the background

Do not poll `status`, `show` or `doctor` in a loop. Wait for the next attention with `watch`:

1. Take `cursor` from the last `status` (or the last `watch`).
2. Run `"$DAGQ" watch --after <cursor> --role maintainer` with the Bash tool's `run_in_background`. It blocks until an attention event for you (`ask_answered` included, `ask_opened` not) or a supervisor change (default `--timeout 600`).
3. When it finishes, read its `events`, `supervisors_changed`, `supervisors` and `cursor`. Route each attention event as in step 2; for a supervisor change, run `status` and act on step 2.1. On a timeout `events` is empty and the cursor is unchanged.
4. Go back to 2 with the returned `cursor`. Keep exactly one watch running at a time.

`watch` and `events --after <cursor>` only read the queue. **Never call `integrate` because a watch returned**; landing goes through the `dagq-land` review, which lands on a pass and asks only on doubt.

## 4. Ask instead of waiting

When a run needs a decision you cannot make yourself (landing on doubt, a choice between options, anything the person must approve), do not wait at the terminal or use `AskUserQuestion`: register an ask and move on to the next attention.

```sh
"$DAGQ" ask --kind <approve_landing|answer_prompt|decide> --run <run_id> \
  --question "<what to decide and why, self-contained>" --option "<choice>" --option "<choice>"
```

Leave that run alone until its answer arrives as an `ask_answered` from your `watch`; read it with `"$DAGQ" asks --role maintainer`, act on it, then `"$DAGQ" ask close <id>`:

- `approve_landing`: the `dagq-land` skill, step 5.
- `decide` you forwarded from a worker's `worker_question`: pass it on unchanged (`dagq-session`, section 2).
- Other `decide` / `answer_prompt`: do what the answer says within step 5 (a dialog's key as in `dagq-session`); report anything beyond it.
- `blocked` (the observer's): act only on what the answer asks of you, then close it.

Before your first ask, read the asks section of `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/status.md` (kinds, `--task`, duplicates, withdrawing).

## 5. Where your authority ends

- Land without asking when the `dagq-land` review passes (`integrate` also pushes `main`); on doubt register an `approve_landing` ask (step 4) and land only once it is answered `land`.
- Ask first (an ask): `down --force`, `recover`, changing a task's acceptance, anything outside a run's own worktree, and a `push_failed` whose cause needs the person (`dagq-land`, step 6).
- Do yourself: `up`, `status`, `watch`, `show`, `review`, `integrate` after a passing review, answering a trust or permission prompt about a run's own worktree or a worker's question about it (`dagq-session`), and reporting.
- Not yours: `add`, `goal add`, `goal close`, and the `draft` tasks from `follow_ups` are the planner's (`dagq-planner`); `ready` / `cancel` only as an answer says. Record new work with `"$DAGQ" note --task ID --text "..."`.

## 6. Stop the runtime

```sh
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
"$DAGQ" down --force    # kill it now; only with the user's consent
```

Use `--wait` before replacing the binary or shutting down, after sending `/exit` to any run whose exit request timed out (`dagq-session`): the drain waits for it. `down` never closes the maintainer, inbox or planner workspaces or the workers'. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-maintain/reference/up-down.md` has the outcomes, the in-cmux case and where the logs are.
