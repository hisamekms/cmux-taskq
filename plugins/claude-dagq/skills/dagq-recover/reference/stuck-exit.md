# Carry out the answer of a `stuck_exit` ask

The supervisor sends `/exit` once, after its review's verdict (ADR-0027: the session stays open through validation and review), or, for a run an older supervisor started, after the receipt when the worker goes idle; a resumed session gets it once it goes idle or its resume timeout passes. A session is idle only when its Stop hook's marker (`idle.json` in the run directory) lists no background work `running` (task 147): the supervisor holds `/exit` while it does, and Claude Code writes a new marker when the work ends. So "Background work is running" now means the work outlived that wait (the resume timeout, counted from the receipt or from the start of the wait for `/exit`) or started after the idle. When the session has not exited within the exit timeout (120 seconds with cmux), the supervisor records `exit_request_timed_out`, keeps the run where it is with its lease (`awaiting_integration`, `needs_session` or `failed` after a verdict; `running` on the older path), and registers a `stuck_exit` ask on the run (`asked_by` `supervisor`). The question names the run, the task, the timeout, the workspace and what follows once the session exits, and ends with the last 15 non-empty lines of the screen. The options are `exit` (answer the dialog so that the session exits, then send `/exit`) and `wait` (leave the session as it is). The inbox shows it to the person; its answer is carried out here, on the person's word. The supervisor never resends `/exit` and never types into the workspace, and a drain (`down --wait`, a version swap) waits for this run until its session exits.

Take `workspace_id`, `worktree_path` and `receipt_path` from `"$DAGQ" show <task_id> --full`, and the answer from `"$DAGQ" asks --role inbox`. The cmux commands are in `reference/session.md`.

## When the session already exited

Judge it by the session, never by the run's status: after a verdict the run is already `awaiting_integration`, `needs_session` or `failed` while its session still runs. The session is gone when `"$DAGQ" show <task_id> --full` lists the `wrapper` process with `exited_at` set (or a `session_exited` event after the `exit_request_timed_out`), or `cmux read-screen` shows a shell prompt instead of Claude Code. The supervisor closed the ask when it saw the exit (an unanswered ask gets the answer `the session exited; closed by the runtime`) and moved the run on (see "What follows the exit"). There is nothing to do. If the ask is still listed as answered and not closed, `"$DAGQ" ask close <id>`. While the wrapper still runs, the session is alive: act on the answer below.

## Answer `exit`

1. Read the screen: `cmux read-screen --workspace <workspace_id> --lines 40`. The excerpt in the question may be old.
2. **"Background work is running"** (Claude Code's confirmation for `/exit` while a `run_in_background` shell, a wait loop or a watch the worker started is still running; its choices are "Exit and stop tasks", "Move to background and exit" and "Stay"). "Exit and stop tasks" kills that work, so first make sure the run's result does not depend on it:
   - `git -C <worktree_path> status --porcelain` prints nothing (the worktree is clean), and
   - the receipt's `commit` is the worktree's HEAD: `jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`.

   When both hold, select "Exit and stop tasks" (`down` until it is highlighted, then `enter`). The session exits by itself, so no second `/exit` is needed. When either does not hold, the background work may still be changing the result: choose nothing, tell the person what you found, and do what they say instead.
3. **Any other dialog** (a permission prompt, a `❯` numbered choice): show it to the person and send the key they choose. Then send `/exit` and `enter`.
4. **No dialog**, and the prompt is idle: send `/exit` and `enter`.
5. Read the screen again until the session is gone. The supervisor then closes the ask and moves the run on (see "What follows the exit"), so do not `ask close` it yourself. If the session still does not exit, show the person the screen, then `ask close <id>`.

## Answer `wait`, or anything else

Do only what the answer says, on the person's word. `wait` means: leave the session and its dialog alone; the person handles it in the workspace. Then `"$DAGQ" ask close <id>` to mark the answer read. The run stays where it is, under the supervisor's lease, until its session exits.

## What follows the exit

Once the session exits, the supervisor moves the run on by its status (the question says which):

- `running` (the older path): `validating`, then the supervisor's review.
- `awaiting_integration` after a `pass`: the workspace is closed and the run lands on `main`; after a `concern` (or a third review that does not pass): an `approve_landing` ask for the inbox; after a failed review: `review_failed`, a review by hand (`reference/review-by-hand.md`).
- `needs_session` (evidence missing): the workspace is closed and the runtime resumes the run in a workspace of its own.
- `failed`: the supervisor triages the run and closes its workspace.
- A resumed session (its question says `stays needs_session`): the supervisor let it go at the timeout, so the run is `needs_session` without a lease and `status` shows `resuming (runtime)`. Once the session exits, the supervisor's next pass closes the ask; with attempts left it also closes the workspace it left and resumes the run again, and after the last attempt it makes the run `failed`, opens a `decide` ask (`retry` / `cancel`) and closes the workspaces.
