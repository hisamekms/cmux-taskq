---
name: dagq-session
description: Act on a dagq run's own Claude session in its cmux workspace: resume a needs_session run so it rebases and fixes its branch, answer the trust or permission prompt or question a running worker stops at, send /exit to a run whose exit request timed out, and close the workspace of a failed or interrupted run. Use when status or watch reports "resume session", "send /exit", "answer the prompt in workspace <id>" or "inspect and close workspace", or a running run makes no progress. Not for landing (dagq-land) or a dead lease (dagq-recover).
---

# dagq: act on a run's session

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. A run's session works only in its own worktree; this session never edits that worktree, merges or pushes for it.

Take `workspace_id`, `worktree_path`, the run `id` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters). Read a workspace's screen before sending anything to it:

```sh
cmux read-screen --workspace <workspace_id> --lines 40
```

A run's workspace is named `[<repo>]dagq#<task-id> <task title>` with the description `run <run-id>`. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-session/reference/cmux.md` has the key and text commands.

## 1. A running run waits at a prompt or a question

Attention `answer the prompt in workspace <id>` (`kind` `prompt_waiting`): the supervisor found a dialog. When a run has been `running` for 90 seconds with no receipt and no idle marker, it reads the workspace screen and records a dialog there (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) once as `prompt_waiting`; `"$DAGQ" show ID --full` has its `excerpt` (the last 15 lines of the screen). Work from that attention and the workspace it names; do not search the workspaces with `read-screen`. The supervisor sends no key and no notification: the answer is yours or the user's. The attention goes away when the dialog leaves the screen (`prompt_cleared`) or the receipt arrives. A question the worker writes on its terminal may not be detected. Read the screen of the named workspace, then:

- Folder-trust dialog, or a permission prompt about the run's own worktree (edits inside it, `cargo`, `git`): answer it yourself with keys (`down` until the accepting choice is highlighted, then `enter`).
- Anything else, or anything needing the user's judgement: report it to the user and send their answer.
- A question the worker wrote on its terminal: answer it the same way, as text followed by `enter`.

A stuck prompt is not a runtime failure: answer it, do not recover the run. The trust dialog appears only when the repository root was never trusted; every session started before the root is first trusted (up to `--parallel`) shows it, so answer each.

## 2. Send /exit after the exit request timed out

Attention `send /exit`: the run stays `running` after `exit_request_timed_out`, because something in the session, usually one of Claude Code's own dialogs, held the `/exit` back. Do not `recover` it and do not `ready` the task again. Read the screen, deal with the dialog (ask the user when it needs their judgement), then send `/exit` in that workspace. The run then goes on to `validating`. The supervisor never resends `/exit`, and any drain waits for this run until its session exits.

## 3. Close the workspace of a failed or interrupted run

Attention `inspect and close workspace`: the runtime keeps the workspace of a `failed` or `interrupted` run for inspection. Report `last_error` to the user; once they have looked at what they need (closing discards the screen, so read it first if `last_error` is not enough), close it:

```sh
cmux workspace close <workspace_id>
```

The worktree, branch and run directory stay for the user's manual cleanup. Retrying is the user's decision: `"$DAGQ" ready ID` makes a new run.

## 4. Resume a needs_session run

Attention `resume session`: `integrate` could not land the run (a rebase conflict, or a verification command failed on the rebased tree). The run's own Claude session fixes it, not this session and not by hand. Open a workspace that resumes it (the session ID is the run ID):

```sh
cmux workspace create --name "[<repo>]dagq resume <run-id>" --cwd "<worktree_path>" \
  --command "claude --resume <run-id>"
```

Send it the reason from `last_error` and this instruction: rebase the branch onto the `main` commit named in the reason (`git rebase <main commit>`), resolve the conflicts or fix what broke the verification command, rerun the task's verification commands, commit, keep the worktree clean, and rewrite `receipt.json` in `run_dir` by atomic rename with the new head as `commit`; if the change is no longer needed, write `"result": "failed"` with the reason in `summary`. It must not merge or push.

When it reports done, send `/exit` in that workspace (landing removes the worktree it works in), then land the run again with the `dagq-land` skill: `review ID`, then `integrate ID` on a passing review (the user is asked only on doubt). `integrate` repeats the rebase and reruns the verification commands on the new head; it keeps the run in `needs_session` with a new reason if the receipt does not name the current head, and marks it `failed` on a failed receipt.
