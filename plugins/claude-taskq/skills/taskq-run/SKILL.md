---
name: taskq-run
description: Start executing a ready cmux-taskq task with the supervisor in a dedicated cmux workspace, watch its run through the binary's JSON, judge completion from the run state and receipt, and confirm integration after the run branch is merged into main. Use when the user asks to run, start, supervise, or launch a queued task, to check whether a run finished, or to mark a merged task as completed.
---

# cmux-taskq: run a task and confirm integration

Prerequisite: resolve the launcher as in the `taskq` skill (`TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"`, `"$TASKQ" --resolve`). If it is not in context, read `${CLAUDE_PLUGIN_ROOT}/skills/taskq/SKILL.md` first. Never touch the queue database directly.

## 1. Check that something can run

```sh
"$TASKQ" candidates
"$TASKQ" status
```

`candidates` must list the task; otherwise make it `ready` or complete its dependencies first. `status` must show `"supervisor": null`; a live lease means a supervisor is already running (one task at a time), and a lease with `heartbeat_stale: true` must be handled with the `taskq-recover` skill before another supervisor can start.

## 2. Start the supervisor in its own cmux workspace

`supervise` blocks until the run is finished, so it must not run inside this Claude Code session's shell. Launch it in a dedicated cmux workspace with absolute paths taken from `--resolve` (the workspace shell does not inherit this session's environment):

```sh
eval "$("$TASKQ" --resolve | sed -n 's/.*"binary": "\([^"]*\)".*"db": "\([^"]*\)".*"repo": "\([^"]*\)".*/BIN=\1; DB=\2; REPO=\3/p')"
cmux workspace create --name "taskq supervise" --cwd "$REPO" \
  --command "'$BIN' --db '$DB' supervise --repo '$REPO'"
```

`--repo` is the repository checkout whose `refs/heads/main` becomes the base commit; use the main checkout, not a task worktree. Pass `--cmux EXE` / `--claude EXE` after `supervise` if those executables are not on the workspace's PATH. If cmux is missing, tell the user the supervisor needs cmux and Claude Code installed and cannot be started from here; the same command can be run by hand in any dedicated terminal.

The supervisor claims one candidate, creates `<db>.runs/<run-id>/`, a worktree on branch `taskq/<run-id>`, and a second cmux workspace where Claude Code works on the task interactively. Trust and permission prompts are answered in that workspace by a person. It processes exactly one task and exits.

## 3. Watch the run

Poll with `"$TASKQ" show ID` (and `"$TASKQ" status`), not by reading the workspace screen. Read the latest entry of `runs`:

- `claimed` / `starting` / `running`: in progress. `events` shows `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session exited; the supervisor is checking the receipt, commit, clean worktree, and rerunning the task's verification commands.
- `awaiting_integration`: accepted. `result_commit` is the commit to review on `branch`; the workspace was closed (`workspace_closed_at` set) and the worktree and branch are kept.
- `failed`: rejected or exited nonzero; `last_error` says why (missing or inconsistent receipt, dirty worktree, verification command failure). Workspace and worktree are kept. Retry with `"$TASKQ" ready ID` after fixing the cause; the next `supervise` makes a new run.
- `interrupted`, or `status` shows a stale lease: use the `taskq-recover` skill.

Completion is decided only by these states. A Stop hook, an idle session, or a `receipt.json` appearing in `run_dir` is not success: the receipt's claims are cross-checked by the supervisor, and `validation_finished` in `events` records the verdict and reason. If the run stays `running` after the supervisor logged `exit_request_timed_out`, tell the user to send `/exit` in the task workspace themselves.

## 4. Review and integrate

When the run is `awaiting_integration`, tell the user the branch and `result_commit`, and how to review: `git log main..taskq/<run-id>`, `git diff main...taskq/<run-id>`, the `receipt.json` and `verify-N.log` files in `run_dir`. The merge into `main` is manual and outside this plugin's control; only merge when asked, and use a merge commit or fast-forward, not squash or cherry-pick, because the same commit must reach `main`.

After the merge, confirm it:

```sh
"$TASKQ" integrate ID
```

`{"outcome": "integrated", ...}` marks the run `integrated` and the task `completed`, which unblocks dependents (`candidates`). `{"outcome": "not_integrated", "main": ..., "reason": ...}` means `result_commit` is not an ancestor of `main`; nothing changed, so check the merge (a squash produces a different SHA and is not recognized). Pass `--repo PATH` only if the repository moved since the run. The worktree and branch are left for the user to remove (`git worktree remove`, `git branch -d`).
