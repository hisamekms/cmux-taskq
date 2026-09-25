---
name: dagq-recover
description: What a person does by hand in a dagq queue, from the inbox or planner session and only on the person's word. Recover a run when no supervisor serves the queue (or a dead supervisor's lease holds it); decide on a failed or interrupted run whose headless triage failed; review and integrate a run whose headless review failed, or push main after a failed push; carry out the answer of a stuck_exit or answer_prompt ask in a run's cmux workspace; and start, stop or update the runtime with up / down. Use when status or watch shows "recover run", "triage by hand", "review by hand", "review and integrate", "push main", "restart supervisor", "send the answer of ask <id> to the worker", an answered stuck_exit or answer_prompt ask, or when the person asks to start, stop, update or recover. Entries ending in "(runtime)" need nothing.
---

# dagq: what a person does by hand

Prerequisite: resolve the launcher as in the `dagq` skill (`DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`). Never touch the queue database directly; the binary refuses unsafe recoveries itself, so do not work around it. Everything here is done from the inbox or planner session, when the person says so (ADR-0041 decision 6).

The supervisor does most of the work itself (ADR-0041 decision 3). A run without a lease whose session processes are all gone is recovered by the next supervisor pass (`run_recovered` with `by: supervisor`); every `failed` or `interrupted` run goes to its headless triage (`next: triaging (runtime)`), which readies the task, resumes the run or opens a `decide` ask, then closes the run's workspace; a `needs_session` run is resumed (`resuming (runtime)`) and, after three resumes, handed to the person as a `decide` ask (`retry` / `cancel`). Do not recover, `ready` or close anything for such a run. `status` is authoritative over what a `watch` reported.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

Explain to the person what is still alive: the supervisors, each unfinished run's lease and processes, and its `blockers` (`recoverable: true` when empty). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/doctor.md` lists the fields and the common cases. A `running` or `validating` run whose wrapper is alive under a stale lease is adopted by the next supervisor: start one (`up`, section 5) instead of recovering it.

## 2. Stop what is still running

Recovery is refused while a process of that run is alive or its lease is fresh. The person ends them (`/exit` in the run's workspace, or stopping a hung supervisor); do not kill processes yourself.

## 3. Recover (only without a supervisor)

Attention `recover run` (`kind` `runtime_error`): an unfinished run without a lease whose session may still live. With a supervisor running it recovers the run once the session is gone; without one:

```sh
"$DAGQ" recover RUN_ID
```

The run becomes `interrupted` (an `integrating` one `awaiting_integration`), other runs are untouched, and the worktree, workspace and run directory are kept. The next supervisor triages it; start one (`up`) rather than deciding yourself.

## 4. Triage by hand

Attention `triage by hand` (`kind` `triage_failed`): the supervisor's headless triage of a `failed` / `interrupted` run could not start, timed out, printed no verdict, or its verdict could not be applied (`show ID` has the `triage_failed` event with `error`). The run stays as it is and is not triaged again. Read `last_error` and the run directory's `triage-prompt-N.txt`, `triage-N.out` and `triage-N.err`, and bring the choice to the person. On their answer: `"$DAGQ" ready ID` runs the task again as a new run (a task to change goes to the planner), `cancel ID` drops it. The run's workspaces stay open until the task is readied or canceled; the supervisor's next pass then closes them (`workspace_closed`, `by: supervisor`), so read the screen before answering. Close one by hand (`cmux workspace close <workspace_id>`) only while no supervisor runs. Removing the old worktree and branch is the person's manual cleanup (`worktree_path` and `branch` in `show ID`).

## 5. Start, stop and update the runtime

```sh
"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"            # add --parallel N (default 4)
"$DAGQ" up --in-cmux --plugin-dir "$CLAUDE_PLUGIN_ROOT"  # only when the preflight sends you there
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
```

`up` is idempotent: it keeps one supervisor resident and opens the inbox and planner workspaces (the one you run it from is `skipped`). `restart supervisor` (`supervisor_stopped`, `supervisor_stale`) is answered with `up`; an in-cmux supervisor is never restarted by anything else. Updating the fixed binary is: replace it, then `up`, which drains a supervisor of another version first. `down --force` kills the supervisor and loses its active runs: only on the person's explicit word. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/up-down.md` has the outcomes, the in-cmux case, the binary update and the logs.

## 6. Review by hand, and a failed push

Attention `review by hand` (`review_failed`: the supervisor's headless review failed) or `review and integrate` (a run accepted without a review): review it in a subagent from the file `review ID` writes, and on the person's word `integrate` it; on doubt, register an `approve_landing` ask. `push main` (`push_failed`): fix the cause, then `git push origin main`. Follow `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`.

## 7. A run's session: dialogs, stuck exits, undelivered answers

The answer of a `stuck_exit` ask (`exit`), of an `answer_prompt` ask, and `send the answer of ask <id> to the worker and close it` are carried out in the run's cmux workspace with keys and text, never by recovering the run. Follow `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md` (and `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/stuck-exit.md` for `stuck_exit`). What the runtime's resume of a `needs_session` run sends and when it ends is in `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/resume.md`; never open a resume workspace yourself.
