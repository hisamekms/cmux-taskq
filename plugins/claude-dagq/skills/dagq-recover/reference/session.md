# A run's session: dialogs, stuck exits and undelivered answers

Read this to carry out, on the person's word, an answer that has to reach a run's Claude session in its cmux workspace (the `dagq-recover` skill, section 7). A run's session works only in its own worktree; never edit that worktree, merge or push for it, and never `recover` a run whose session only waits at a dialog.

Take `workspace_id`, `worktree_path`, the run `id` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters). A run's workspace is named `[<repo>]worker#<task-id> - <task title>` with the description `dagq role=worker queue=<queue hash> run=<run-id> task=<id>`; a resumed session's is titled the same with the description `run <run-id> resume`. Names are for people; the runtime finds workspaces by `workspace_id`.

## cmux commands

```sh
cmux read-screen --workspace <workspace_id> --lines 40   # read before sending anything
cmux send-key --workspace <workspace_id> down            # move to the choice you want
cmux send-key --workspace <workspace_id> enter           # confirm a dialog, or submit text
cmux send --workspace <workspace_id> "<text>"            # type an answer or /exit, then send-key enter
```

- A dialog is answered with keys, not with text: send `down` only until the choice you want is highlighted, then `enter`.
- The runtime closes the workspaces of accepted runs and of triaged runs itself; `cmux workspace close <workspace_id>` is only for a run whose triage failed (`triage by hand`).
- The folder-trust prompt is decided by the repository root, not the worktree. Running `claude` once in the repository root before the first run avoids it.
- Never open a resume workspace yourself; only a dialog a resumed session stops at is answered here.

## An `answer_prompt` ask (a dialog)

When a run has been `running` for 90 seconds with no receipt and no idle marker, the supervisor reads its screen; a dialog there (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) is recorded once as `prompt_waiting` and raised as an `answer_prompt` ask (`asked_by` `supervisor`) whose question names the workspace and ends with the last 15 lines of the screen. The supervisor sends no key. Once the person answered it (`read the answer of ask <id> and close it`), read the screen of the named workspace, send the chosen key or text as the answer says, and `"$DAGQ" ask close <id>`. When the dialog leaves the screen, the receipt arrives or the session exits, the supervisor closes the ask itself (`answer` then reports it is not open). A run with an unclosed `worker_question` is not read for a dialog: it waits for its answer.

## An undelivered worker answer

A worker's `worker_question` is answered by the person in the inbox; the supervisor types `answer to ask <id>: <answer>` and Enter into the worker's terminal once the worker went idle after asking, closes the ask and records `ask_delivered` (`delivering the answer of ask <id> (runtime)` meanwhile). Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run no longer `running`): the supervisor could not type it (it tries once), or the session is gone. When the run is still `running`, read the worker's screen, send the text `answer to ask <id>: <answer>` followed by `enter`, then `"$DAGQ" ask close <id>`. When the run is at rest, the answer has nobody to reach: tell the person and close the ask.

## A `stuck_exit` ask

The answer `exit` is carried out as `reference/stuck-exit.md` says; `wait` needs only `ask close <id>`.
