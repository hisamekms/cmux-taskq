# cmux commands for a run's workspace

Read this when you need the exact command to answer a run's session.

```sh
cmux read-screen --workspace <workspace_id> --lines 40   # read before sending anything
cmux send-key --workspace <workspace_id> down            # move to the choice you want
cmux send-key --workspace <workspace_id> enter           # confirm a dialog, or submit text
cmux send --workspace <workspace_id> "<text>"            # type an answer or /exit, then send-key enter
cmux workspace close <workspace_id>                      # only for a failed or interrupted run
```

- A dialog is answered with keys, not with text: send `down` only until the choice you want is highlighted, then `enter`.
- `workspace_id` is the run's `workspace_id` in `"$DAGQ" show ID`. `doctor` lists only unfinished runs, so a `failed` run's workspace is found through `show`.
- The runtime closes only the workspace of an accepted run (`awaiting_integration`); it never closes a `failed` or `interrupted` run's workspace. The session in it has already exited, so only the shell is left.
- The folder-trust prompt is decided by the repository root, not the worktree. The user avoids it by running `claude` once in the repository root before the first run.
- The runtime opens, answers and closes a resumed `needs_session` session's workspace (titled like the run's worker, `[<repo>]worker#<task-id> - <task title>`, with the description `run <run-id> resume`). Never open one yourself or type into it; only a dialog it stops at is yours to answer, with keys as above.
