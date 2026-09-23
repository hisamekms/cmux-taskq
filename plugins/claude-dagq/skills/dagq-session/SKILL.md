---
name: dagq-session
description: Act on a dagq run's own Claude session in its cmux workspace: answer the trust or permission prompt a running worker (or a session the runtime resumed) stops at, answer a worker's worker_question ask (or forward it to the person as a decide ask), send /exit to a run whose exit request timed out, close the workspace of a failed or interrupted run, and hand the person, through a decide ask, a needs_session run the runtime gave up resuming. Use when status or watch reports "resume session", "send /exit", "answer the prompt in workspace <id>", "send the answer of ask <id> to the worker and close it" or "inspect and close workspace", when status lists a worker_question ask, or a running run makes no progress. "resuming (runtime)" needs nothing. Not for landing (dagq-land) or a dead lease (dagq-recover).
---

# dagq: act on a run's session

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. A run's session works only in its own worktree; this session never edits that worktree, merges or pushes for it.

Take `workspace_id`, `worktree_path`, the run `id` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters). Read a workspace's screen before sending anything to it:

```sh
cmux read-screen --workspace <workspace_id> --lines 40
```

A run's workspace is named `[<repo>]worker#<task-id> - <task title>` with the description `dagq role=worker queue=<queue hash> run=<run-id> task=<id>`; names are for people, and the runtime finds workspaces by `workspace_id`. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-session/reference/cmux.md` has the key and text commands.

## 1. A running run waits at a prompt

Attention `answer the prompt in workspace <id>` (`kind` `prompt_waiting`): the supervisor found a dialog. When a run has been `running` for 90 seconds with no receipt and no idle marker, it reads the workspace screen and records a dialog there (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) once as `prompt_waiting`; `"$DAGQ" show ID --full` has its `excerpt` (the last 15 lines of the screen). Work from that attention and the workspace it names; do not search the workspaces with `read-screen`. The supervisor sends no key and no notification: the answer is yours or the person's. The attention goes away when the dialog leaves the screen (`prompt_cleared`) or the receipt arrives. A run with an unclosed ask is not read for a dialog: it waits for its answer (section 2). Read the screen of the named workspace, then:

- Folder-trust dialog, or a permission prompt about the run's own worktree (edits inside it, `cargo`, `git`): answer it yourself with keys (`down` until the accepting choice is highlighted, then `enter`).
- Anything else, or anything needing the person's judgement: register `"$DAGQ" ask --kind answer_prompt --run <run_id> --question "<the dialog, self-contained>" --option "<choice>"...` and move on. When it is answered, send the chosen key or text, then `ask close <id>`.
- A question an older worker wrote on its terminal instead of asking: answer it the same way, as text followed by `enter`.

A stuck prompt is not a runtime failure: answer it, do not recover the run. The trust dialog appears only when the repository root was never trusted; every session started before the root is first trusted (up to `--parallel`) shows it, so answer each.

## 2. A worker asked a question (`worker_question`)

A worker that needs a decision registers a `worker_question` ask (`--run`) and stops; it shows in `status` `asks`. Never type into its terminal yourself: the supervisor types the answer once the worker is idle after asking. Answer it yourself with `"$DAGQ" answer <id> --text "..."` when it is about the run's own worktree; otherwise ask the person with a `decide` ask on the same run and forward their answer unchanged with `answer`. `delivering the answer of ask <id> (runtime)` needs nothing. For `send the answer of ask <id> to the worker and close it`, and the details, read `${CLAUDE_PLUGIN_ROOT}/skills/dagq-session/reference/worker-question.md`.

## 3. Send /exit after the exit request timed out

Attention `send /exit`: the run stays `running` after `exit_request_timed_out`, because something in the session, usually one of Claude Code's own dialogs, held the `/exit` back. Do not `recover` it and do not `ready` the task again. Read the screen, deal with the dialog (an `answer_prompt` ask as in section 1 when it needs the person), then send `/exit` in that workspace. The run then goes on to `validating`. The supervisor never resends `/exit`, and any drain waits for this run until its session exits.

## 4. Close the workspace of a failed or interrupted run

Attention `inspect and close workspace`: the runtime keeps the workspace of a `failed` or `interrupted` run for inspection. Report `last_error`; closing discards the screen, so read it first when `last_error` is not enough, then close it:

```sh
cmux workspace close <workspace_id>
```

The worktree, branch and run directory stay for manual cleanup. Retrying is the person's decision: register a `decide` ask on the run (`--option ready --option cancel`) and, on its answer, run `"$DAGQ" ready ID` (a new run) or `cancel ID`.

## 5. needs_session runs: the runtime resumes them

The runtime resumes a `needs_session` run itself: the supervisor opens a workspace titled like the run's worker (`[<repo>]worker#<task-id> - <task title>`, description `run <run-id> resume`) with the run's own session, types the fixed resolution request (the reason, the `main` to rebase onto, the tasks landed since the run's base, the steps), sends `/exit` once the session has rewritten the receipt for the new head (or has gone idle without resolving it, or ran past the resume timeout), and closes the workspace. A run whose `integrate` was already called is then landed by the runtime; any other run goes back to `awaiting_integration` (attention `review and integrate`, the `dagq-land` skill). While this goes on, `status` shows `next: resuming (runtime)`: do nothing for that run. Never open a resume workspace or type into it; the one exception is a dialog the resumed session stops at, which you answer as in section 1 (the attention `answer the prompt in workspace <id>` does not cover resumed sessions; find the workspace with that description in `cmux workspace list`).

The runtime tries three times. A run comes back as attention `resume session` only when the runtime cannot go on: the third resume did not resolve it (its last `resume_finished` event has `exhausted: true`), or a resume session nobody watches is still running (the supervisor died mid-resume, or the session ignored `/exit` and was let go). For the latter, read that workspace's screen and report it; once its session has exited (`/exit`) and the workspace is closed, the supervisor resumes the run again. Take `last_error` and what the attempts did: `"$DAGQ" show ID --full` lists the `resume_started` / `resume_finished` events (`outcome`, `head`), and `run_dir` keeps each request as `resume-N.txt` and the final screen as `terminal-resume-N.txt`. What happens next is the person's decision: register a `decide` ask on the run with those facts (`--option ready --option cancel`), and on its answer run `ready ID` (reruns the task) or `cancel ID` (drops it); a task to edit goes to the planner.
