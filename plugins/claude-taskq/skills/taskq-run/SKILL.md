---
name: taskq-run
description: Start the cmux-taskq supervisor (a resident loop that runs dependency-ready tasks in parallel, each in its own cmux workspace) in a dedicated cmux workspace, watch runs through the binary's JSON, judge completion from the run state and receipt, land a validated run on main with integrate (the runtime rebases, re-validates and squashes it), answer the trust prompt a real session stops at in each run's workspace, resume a run that needs a session after a conflict, and close the workspace of a failed or interrupted run. Use when the user asks to run, start, supervise, or launch queued tasks, to check whether a run finished, to integrate or land a finished task, to deal with a run in needs_session, or to clean up a run that failed.
---

# cmux-taskq: run a task and land it on main

Prerequisite: resolve the launcher as in the `taskq` skill (`TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"`, `"$TASKQ" --resolve`). If it is not in context, read `${CLAUDE_PLUGIN_ROOT}/skills/taskq/SKILL.md` first. Never touch the queue database directly.

## 1. Check that something can run

```sh
"$TASKQ" candidates
"$TASKQ" status
```

`candidates` must list the task; otherwise make it `ready` or complete its dependencies first. `status` shows the live supervisors (`supervisors`, one entry per process with its `pid`, `alive`, `run_ids`, `stale`) and the unfinished runs with their leases. If a supervisor is already running and alive, do not start another one: it picks the task up on its next poll (within a few seconds). A supervisor listed with `stale: true` and `alive: false` died; its runs are handled with the `taskq-recover` skill, and a new supervisor may be started meanwhile because leases are per run.

## 2. Start the supervisor in its own cmux workspace

`supervise` is a resident loop, so it must not run inside this Claude Code session's shell. Launch it in a dedicated cmux workspace whose working directory is the repository, so the binary resolves the same queue there. Take `binary` and `repo` from `"$TASKQ" --resolve` and substitute them as absolute paths (the workspace shell does not inherit this session's environment):

```sh
cmux workspace create --name "taskq supervise" --cwd "<repo>" \
  --command "'<binary>' supervise --parallel 4"
```

`--parallel N` (default 4) caps how many runs execute at once. The base commit of every run is the repository's `refs/heads/main` at claim time, whichever checkout `--cwd` names. Only if `CMUX_TASKQ_DB` is set in this session, add `--db '<db>'` before `supervise` with the `db` value from `--resolve`, because the workspace does not see the variable. Pass `--cmux EXE` / `--claude EXE` after `supervise` if those executables are not on the workspace's PATH. If cmux is missing, tell the user the supervisor needs cmux and Claude Code installed and cannot be started from here; the same command can be run by hand in any dedicated terminal inside the repository.

The supervisor claims candidates up to the limit, creates `<runs_dir>/<run-id>/` (next to the queue database, see `--resolve`) for each, a worktree on branch `taskq/<run-id>` inside it, and a cmux workspace per run where Claude Code works on the task interactively. It keeps polling for new candidates every few seconds, including tasks that `integrate` unblocks, until it is stopped: the first Ctrl-C in its workspace stops claiming and waits for the active runs to finish; a second one kills it. `supervise --once` instead exits as soon as nothing is active and nothing is claimable, which suits a single batch.

### Answer the trust prompt of every run

A real Claude Code session stops at a trust prompt for each new run worktree, because every run's worktree is a directory Claude Code has never seen; the runtime does not answer it. Until it is answered the run stays `running` after `agent_started` and nothing happens in the worktree. Answer it in the run's cmux workspace: take `workspace_id` from `"$TASKQ" show ID` (the workspace is named `taskq <task-id> <run-id>`), read the screen, and send the keys that accept the prompt:

```sh
cmux read-screen --workspace <workspace_id> --lines 40
cmux send-key --workspace <workspace_id> enter      # after `down` if the accepting choice is not highlighted
```

Do this for every run the supervisor starts, including runs of dependent tasks claimed later, and check the screen again afterwards: the session may stop at a permission prompt next (answer it the same way when it concerns the worktree, and ask the user otherwise). A run whose session never left the trust prompt has no receipt and no commit, so it is not a runtime failure; do not recover it, answer the prompt.

## 3. Watch the runs

Poll with `"$TASKQ" show ID` (and `"$TASKQ" status`), not by reading the workspace screen. Read the latest entry of `runs`:

- `claimed` / `starting` / `running`: in progress. `events` shows `lease_acquired`, `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session exited; the supervisor is checking the receipt, commit, clean worktree, and rerunning the task's verification commands.
- `awaiting_integration`: accepted. `result_commit` is the commit to review on `branch`; the workspace was closed (`workspace_closed_at` set) and the worktree and branch are kept until `integrate` lands the run. The supervisor released its lease (`lease_released`).
- `integrating`: an `integrate` process holds the queue's single integration slot for this run (its lease shows in `status`). Wait for it; if its process died (`doctor` shows the lease stale and its PID dead), the `taskq-recover` skill returns the run to `awaiting_integration`.
- `needs_session`: `integrate` could not land it (rebase conflict, or a verification command failed after the rebase); `last_error` says why. See step 5.
- `integrated`: landed on `main`; `result_commit` is the landed commit and the task is `completed`.
- `failed`: rejected or exited nonzero; `last_error` says why (missing or inconsistent receipt, dirty worktree, verification command failure). Workspace and worktree are kept for inspection; close the workspace yourself afterwards (below). Retry with `"$TASKQ" ready ID` after fixing the cause; the running supervisor makes a new run.
- unfinished with `last_error` set and no lease in `status` (a `runtime_error` event with `lease_released: true`): the supervisor gave this run up (wrapper heartbeat lost, exit request timed out, provisioning or validation error) and kept serving the others; use the `taskq-recover` skill. After a provisioning failure the supervisor stops claiming, finishes its active runs and exits nonzero; fix the cause (cmux, Git) and start it again.
- `interrupted`: recovered; retry with `ready ID`. Its workspace is kept too; close it yourself (below).

The runtime closes only the workspace of an accepted run (`awaiting_integration`). It never closes the workspace of a `failed` or `interrupted` run, so once the user has inspected its screen, worktree, and `last_error`, close it:

```sh
cmux workspace close <workspace_id>
```

`workspace_id` is the run's `workspace_id` in `"$TASKQ" show ID` (`doctor` lists only unfinished runs, so a `failed` run is not there). The session in it has already exited (both states require that), so only the shell is left; closing the workspace discards its screen, so read it first if `last_error` is not enough. The worktree, branch, and run directory stay on disk for the user's manual cleanup, as the `taskq-recover` skill describes.

One run's failure never changes another run: each is judged, closed and released on its own. Completion is decided only by these states. A Stop hook, an idle session, or a `receipt.json` appearing in `run_dir` is not success: the receipt's claims are cross-checked by the supervisor, and `validation_finished` in `events` records the verdict and reason. If a run stays `running` after the supervisor logged `exit_request_timed_out`, tell the user to send `/exit` in that task's workspace themselves.

## 4. Review and land

When the run is `awaiting_integration`, tell the user the branch and `result_commit`, and how to review: `git log main..taskq/<run-id>`, `git diff main...taskq/<run-id>`, the `receipt.json` and `verify-N.log` files in `run_dir`. Never merge, rebase, or fast-forward the branch yourself: landing is the runtime's job and it keeps `main` linear with one squash commit per task. Only land when the user has approved the run.

```sh
"$TASKQ" integrate ID        # this task's run (also resumes a needs_session run)
"$TASKQ" integrate --next    # the oldest run awaiting integration
```

`integrate` takes the single integration slot, rebases the run's worktree onto the current `main`, re-validates it (the receipt must name the worktree head, the rebased head must sit on `main` with a clean tree, and the task's verification commands are rerun), squashes the rebased tree into one commit on `main` (title, receipt summary, trailers `Taskq-Task: ID` and `Taskq-Run: RUN_ID`), and removes the worktree and branch; the run's history stays under `refs/taskq/runs/<run-id>`. Read the `outcome`:

- `integrated`: `run.result_commit` is the new `main` head and the task is `completed`, which unblocks dependents (`candidates`); a running supervisor claims them on its next poll from the landed `main`. Pushing `main` is the user's call.
- `needs_session`: nothing reached `main`; `reason` names the conflicting files (the rebase was aborted and the worktree is back on `result_commit`) or the failed verification command (the rebased tree is left in the worktree). Continue with step 5.
- `failed`: the run's rewritten receipt reported `failed`; the task stays `in_progress` and can be retried with `ready ID` or dropped with `cancel ID`.
- `no_run_awaiting` (`--next` only): the queue is empty; `needs_session` runs are not picked by `--next`.

An error (exit status 1) before `main` moved puts the run back to its previous state with the message in `last_error`; a `main` checkout with uncommitted changes that overlap the landing is a common cause. Fix it and run `integrate` again. The landing happens in the current directory's repository; pass `--repo PATH` only when using `CMUX_TASKQ_DB` from outside it.

## 5. Resume a run that needs a session

A `needs_session` run is fixed by a Claude session in its own worktree, not by this session and not by hand. Take `worktree_path`, the run `id`, and `last_error` from `"$TASKQ" show ID`, then open a workspace that resumes the run's session (its session ID is the run ID):

```sh
cmux workspace create --name "taskq resume <run-id>" --cwd "<worktree_path>" \
  --command "claude --resume <run-id>"
```

Send it the reason from `last_error` and this instruction: rebase the branch onto the `main` commit named in the reason (`git rebase <main commit>`), resolve the conflicts (or fix what broke the verification command on the rebased tree), rerun the task's verification commands, commit, keep the worktree clean, and rewrite `receipt.json` in `run_dir` by atomic rename with the new head commit as `commit`; if the change is no longer needed, write `"result": "failed"` with the reason in `summary` instead. The session must not merge or push. When it reports done, run `"$TASKQ" integrate ID` again: it repeats the rebase (a no-op unless `main` moved again) and the re-validation and lands the run, keeps it `needs_session` with a new reason if the receipt does not name the current head, or marks it `failed` on a failed receipt.
