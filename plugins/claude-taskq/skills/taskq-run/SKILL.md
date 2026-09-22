---
name: taskq-run
description: Start the cmux-taskq supervisor (a resident loop that runs dependency-ready tasks in parallel, each in its own cmux workspace) in a dedicated cmux workspace, watch runs through the binary's JSON, judge completion from the run state and receipt, and confirm integration after a run branch is merged into main. Use when the user asks to run, start, supervise, or launch queued tasks, to check whether a run finished, or to mark a merged task as completed.
---

# cmux-taskq: run a task and confirm integration

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

The supervisor claims candidates up to the limit, creates `<runs_dir>/<run-id>/` (next to the queue database, see `--resolve`) for each, a worktree on branch `taskq/<run-id>` inside it, and a cmux workspace per run where Claude Code works on the task interactively. Trust and permission prompts are answered in those workspaces by a person. It keeps polling for new candidates every few seconds, including tasks that `integrate` unblocks, until it is stopped: the first Ctrl-C in its workspace stops claiming and waits for the active runs to finish; a second one kills it. `supervise --once` instead exits as soon as nothing is active and nothing is claimable, which suits a single batch.

## 3. Watch the runs

Poll with `"$TASKQ" show ID` (and `"$TASKQ" status`), not by reading the workspace screen. Read the latest entry of `runs`:

- `claimed` / `starting` / `running`: in progress. `events` shows `lease_acquired`, `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session exited; the supervisor is checking the receipt, commit, clean worktree, and rerunning the task's verification commands.
- `awaiting_integration`: accepted. `result_commit` is the commit to review on `branch`; the workspace was closed (`workspace_closed_at` set) and the worktree and branch are kept. The supervisor released its lease (`lease_released`).
- `failed`: rejected or exited nonzero; `last_error` says why (missing or inconsistent receipt, dirty worktree, verification command failure). Workspace and worktree are kept. Retry with `"$TASKQ" ready ID` after fixing the cause; the running supervisor makes a new run.
- unfinished with `last_error` set and no lease in `status` (a `runtime_error` event with `lease_released: true`): the supervisor gave this run up (wrapper heartbeat lost, exit request timed out, provisioning or validation error) and kept serving the others; use the `taskq-recover` skill. After a provisioning failure the supervisor stops claiming, finishes its active runs and exits nonzero; fix the cause (cmux, Git) and start it again.
- `interrupted`: recovered; retry with `ready ID`.

One run's failure never changes another run: each is judged, closed and released on its own. Completion is decided only by these states. A Stop hook, an idle session, or a `receipt.json` appearing in `run_dir` is not success: the receipt's claims are cross-checked by the supervisor, and `validation_finished` in `events` records the verdict and reason. If a run stays `running` after the supervisor logged `exit_request_timed_out`, tell the user to send `/exit` in that task's workspace themselves.

## 4. Review and integrate

When the run is `awaiting_integration`, tell the user the branch and `result_commit`, and how to review: `git log main..taskq/<run-id>`, `git diff main...taskq/<run-id>`, the `receipt.json` and `verify-N.log` files in `run_dir`. The merge into `main` is manual and outside this plugin's control; only merge when asked, and use a merge commit or fast-forward, not squash or cherry-pick, because the same commit must reach `main`.

After the merge, confirm it:

```sh
"$TASKQ" integrate ID
```

`{"outcome": "integrated", ...}` marks the run `integrated` and the task `completed`, which unblocks dependents (`candidates`); a running supervisor claims them on its next poll with the new `main` as their base commit. `{"outcome": "not_integrated", "main": ..., "reason": ...}` means `result_commit` is not an ancestor of `main`; nothing changed, so check the merge (a squash produces a different SHA and is not recognized). The check runs against the current directory's repository; pass `--repo PATH` only when using `CMUX_TASKQ_DB` from outside it. The worktree and branch are left for the user to remove (`git worktree remove`, `git branch -d`).
