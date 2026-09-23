# dagq-session: act on the answer of a `stuck_exit` ask

The supervisor sends `/exit` once, after the receipt, when the worker goes idle. When the session has not exited within the exit timeout (120 seconds with cmux), the supervisor records `exit_request_timed_out`, keeps the run `running` with its lease, and registers a `stuck_exit` ask on the run (`asked_by` `supervisor`). The question names the run, the task, the timeout and the workspace, and ends with the last 15 non-empty lines of the screen. The options are `exit` (answer the dialog so that the session exits, then send `/exit`) and `wait` (leave the session as it is). The inbox shows it to the person; you act only on its answer. The supervisor never resends `/exit` and never types into the workspace, and a drain (`down --wait`, a version swap) waits for this run until its session exits.

Take `workspace_id`, `worktree_path` and `receipt_path` from `"$DAGQ" show <task_id> --full`, and the answer from `"$DAGQ" asks --role maintainer`.

## When the session already exited

If the run is no longer `running`, or `cmux read-screen` shows a shell prompt instead of Claude Code, the session is gone. The supervisor closed the ask when it saw the exit (an unanswered ask gets the answer `the session exited; closed by the runtime`), and the run went on to `validating`. There is nothing to do. If the ask is still listed as answered and not closed, `"$DAGQ" ask close <id>`.

## Answer `exit`

1. Read the screen: `cmux read-screen --workspace <workspace_id> --lines 40`. The excerpt in the question may be old.
2. **"Background work is running"** (Claude Code's confirmation for `/exit` while a `run_in_background` shell, a wait loop or a watch the worker started is still running; its choices are "Exit and stop tasks", "Move to background and exit" and "Stay"). "Exit and stop tasks" kills that work, so first make sure the run's result does not depend on it:
   - `git -C <worktree_path> status --porcelain` prints nothing (the worktree is clean), and
   - the receipt's `commit` is the worktree's HEAD: `jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`.

   When both hold, select "Exit and stop tasks" (`down` until it is highlighted, then `enter`; keys as in `reference/cmux.md`). The session exits by itself, so no second `/exit` is needed. When either does not hold, the background work may still be changing the result: choose nothing, register a `decide` ask on the run with what you found (`--option "exit anyway" --option wait`), and act on that answer instead.
3. **Any other dialog** (a permission prompt, a `❯` numbered choice): answer it as in section 1 of the skill. Answer it yourself when it is about the run's own worktree; otherwise register an `answer_prompt` ask and send its answer. Then send `/exit` and `enter`, as in `reference/cmux.md`.
4. **No dialog**, and the prompt is idle: send `/exit` and `enter`.
5. Read the screen again until the session is gone. The supervisor then closes the ask and the run goes on to `validating`, so do not `ask close` it yourself. If the session still does not exit, report it with the screen, then `ask close <id>`.

## Answer `wait`, or anything else

Do only what the answer says, within your authority (`dagq-maintain`, step 5). `wait` means: leave the session and its dialog alone; the person handles it in the workspace. Then `"$DAGQ" ask close <id>` to mark the answer read. The run stays `running` until its session exits, and then goes on to `validating` as usual.
