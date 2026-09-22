---
name: taskq-maintain
description: Maintain a cmux-taskq queue: start its runtime with up (a launchd-resident supervisor that runs dependency-ready tasks in parallel, plus the maintainer's own session), read status to tell a registered, alive supervisor from a stale one, watch each run with show, answer the trust and permission prompts a run's session stops at, review a validated run and land it on main with integrate, resume a run left in needs_session, close the workspace of a failed or interrupted run, report a receipt's follow_ups, and stop the runtime with down. Use when the user asks to start, run, or supervise queued tasks, to check whether a run finished, to integrate or land a finished task, to deal with a run in needs_session, to clean up a failed run, or to shut the supervisor down.
---

# cmux-taskq: run tasks and land them on main

Prerequisite: resolve the launcher as in the `taskq` skill (`TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"`, `"$TASKQ" --resolve`), including how it reports a missing binary and a `plugin_version` / `binary_version` mismatch. If it is not in context, read `${CLAUDE_PLUGIN_ROOT}/skills/taskq/SKILL.md` first. Never touch the queue database directly.

## 0. Roles

Three roles share one queue:

- **supervisor**: the resident `cmux-taskq supervise` process. It claims dependency-ready tasks up to `--parallel`, creates a Git worktree and a cmux workspace per run, validates each receipt, and closes the workspace of an accepted run. It is a process, not a session, and `up` keeps it resident.
- **maintainer**: the Claude Code session following this skill. It registers work (the `taskq` skill), starts and stops the runtime, watches the runs, answers their prompts, reviews what they produced, and lands it with `integrate`. It is the only role that pushes `main`.
- **worker**: the Claude session of one run, inside that run's worktree. It commits to `taskq/<run-id>` and writes the receipt; it never merges, pushes, or closes its workspace.

## 1. Start the runtime

```sh
"$TASKQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"
"$TASKQ" up --parallel 4 --plugin-dir "$CLAUDE_PLUGIN_ROOT"    # --parallel defaults to 4
"$TASKQ" up --in-cmux --plugin-dir "$CLAUDE_PLUGIN_ROOT"       # no launchd; see the preflight failure below
```

Always start the runtime through `up`; do not create a workspace for `supervise` yourself. `up` preflights cmux, Claude Code, the repository and an initialized queue (run `init` from the `taskq` skill first if the queue does not exist), deletes supervisor registrations whose process is dead (`pruned_supervisors`), keeps one supervisor resident as a launchd LaunchAgent, opens the maintainer's cmux workspace, and ends with a `doctor` summary. `--in-cmux` runs the supervisor in a cmux workspace named `taskq <repo> supervisor` instead of under launchd; use it only when the preflight below sends you there, because nothing restarts a supervisor started that way. Pass `--plugin-dir "$CLAUDE_PLUGIN_ROOT"` so the maintainer session it opens loads this plugin; add `--repo PATH` only for a checkout other than the working directory, and `--cmux EXE` / `--claude EXE` when those are not on PATH.

Read the result:

- `supervisor`: `{"outcome": "started" | "reused" | "restarted", "mode", "version", "pid", "token", "workspace_id", "plist", "log_dir"}`. `reused` means a live, heartbeating supervisor of this binary's own version already served this queue and nothing was touched. `mode` is `launchd`, or `in_cmux` with the `workspace_id` it runs in; it is null for a supervisor someone started by hand. `restarted` means the live supervisor ran a different build and `up` drained it and started this one in its place (see below).
- `maintainer`: `{"outcome": "created" | "reused" | "skipped", "workspace_id", "name"}`. **`skipped` is the normal answer when you call `up` from inside the maintainer session**: that session is marked with `CMUX_TASKQ_ROLE=maintainer` and `CMUX_TASKQ_QUEUE`, so `up` does not open a second one. It is not an error, and the supervisor was still started or reused.
- `pruned_supervisors`: dead registrations `up` removed.
- `doctor`: `unfinished_runs` (with `lease_stale`), `awaiting_integration`, `needs_session`. Report these to the user; they are the open work.

**A supervisor of another version is replaced, not reused.** After the user has swapped the `cmux-taskq` binary, a plain `up` is the whole update: it unloads the LaunchAgent, signals the old supervisor, waits — with no timeout — for it to stop claiming and finish the runs it holds, closes its workspace if it ran in one, and starts a supervisor of the new version. The result is `"outcome": "restarted"` with `previous_version`, `version` and `replaced`. Because the wait is the length of the runs in flight, `up` can take a long time; tell the user what it is waiting for rather than killing it. `up --no-wait` refuses instead whenever a run is in flight (it names the count and the run IDs and changes nothing), which is how you check whether a replacement would block; with nothing in flight it replaces the supervisor but gives up after 30 seconds if that supervisor does not stop. Two things `up` will not do: replace a supervisor that is alive but no longer heartbeating (it starts a new one beside it and `status` shows the old row as `stale`, which the user stops with `down --force`), and notice a rebuild that did not bump the version, since `binary_version` is `CARGO_PKG_VERSION`.

`up` is idempotent, so run it again whenever you are unsure. If it fails because cmux refuses a connection from outside its own terminals, no LaunchAgent was installed (the preflight runs before anything is written; only the dead registrations it had already pruned are gone): the supervisor launchd would start cannot reach cmux. Report the message to the user with the remedies it names — a socket password saved in cmux's Settings, or `CMUX_SOCKET_PASSWORD` exported in the shell that runs `up`; both are theirs to do, not this session's. The third remedy is `up --in-cmux`, which you can run yourself once they have chosen it: it needs no password, but launchd no longer restarts the supervisor, so tell them that and run `up --in-cmux` again whenever `status` shows nothing serving the queue.

## 2. Check that something can run

```sh
"$TASKQ" candidates
"$TASKQ" status
```

`candidates` must list the task; otherwise make it `ready` or complete its dependencies first (the `taskq` skill). `status` shows `supervisors`, one entry per process that owns runs, and the unfinished runs with their leases. Read each supervisor entry as:

- `registered: true`, `alive: true`, `stale: false`: the resident supervisor is healthy. It picks the task up on its next poll, within a few seconds. Do nothing; it is listed even when `run_ids` is empty.
- `stale: true` (`alive: false`, or `heartbeat_age_secs` over 30): it died or hangs. Run `up` again: it prunes the registrations whose PID is dead and starts a fresh supervisor. Its orphaned runs are handled with the `taskq-recover` skill, one run at a time. A registration whose PID is still alive but silent is neither pruned nor reused, so that `up` starts a **second** supervisor beside the hung process: report the stale row to the user and let them stop that process first (`down --force` if it is the only one), because the runtime never removes it.
- `registered: false`: not a supervisor but a process holding a lease, normally an `integrate` landing a run.

Empty `supervisors` means nothing serves this queue: run `up`.

## 3. Answer the prompts a run stops at

Claude stops at its folder-trust prompt in a run's workspace only when the repository itself has never been trusted (the prompt is decided by the repository root, not the worktree), so the user should run `claude` once in the repository root before the first run. When a run stays `running` right after `agent_started` and nothing happens in its worktree, it is waiting at a prompt. Take `workspace_id` from `"$TASKQ" show ID` (the workspace is named `taskq <repo> <task-id> <run-id>`, where `<repo>` is the repository directory's name), read the screen and answer there:

```sh
cmux read-screen --workspace <workspace_id> --lines 40
cmux send-key --workspace <workspace_id> down      # move to the accepting choice if it is not highlighted
cmux send-key --workspace <workspace_id> enter
cmux send --workspace <workspace_id> "<text>"      # for a question, then send-key enter
```

A dialog is answered with keys, not with text: read the screen first and send `down` only until the choice you want is highlighted. If `supervise` was started on a repository that had never been trusted, every session it started before the first approval (up to `--parallel`) waits at that dialog, so answer each of their workspaces; sessions started after the approval do not show it.

Answer permission prompts about the run's own worktree yourself (edits inside it, `cargo`, `git`); anything else, and anything needing the user's judgement, goes to the user first. A worker that wrote a question on its terminal is answered the same way. A run stuck at a prompt has no receipt and no commit, but it is not a runtime failure: answer the prompt, do not recover it.

## 4. Watch the runs

Poll with `"$TASKQ" show ID` (and `"$TASKQ" status`), not by reading the workspace screen. Read the latest entry of `runs`:

- `claimed` / `starting` / `running`: in progress. `events` shows `lease_acquired`, `workspace_created`, `agent_started`, `receipt_observed`, `session_idle_observed`, `exit_requested`.
- `validating`: the session exited; the supervisor is checking the receipt, commit, clean worktree, and rerunning the task's verification commands.
- `awaiting_integration`: accepted. `result_commit` is the commit to review on `branch`; the workspace was closed (`workspace_closed_at` set) and the worktree and branch are kept until `integrate` lands the run. The supervisor released its lease (`lease_released`).
- `integrating`: an `integrate` process holds the queue's single integration slot for this run (its lease shows in `status`). Wait for it; if its process died (`doctor` shows the lease stale and its PID dead), the `taskq-recover` skill returns the run to `awaiting_integration`.
- `needs_session`: `integrate` could not land it (rebase conflict, or a verification command failed after the rebase); `last_error` says why. See step 6.
- `integrated`: landed on `main`; `result_commit` is the landed commit and the task is `completed`.
- `failed`: rejected or exited nonzero; `last_error` says why (missing or inconsistent receipt, dirty worktree, verification command failure). Workspace and worktree are kept for inspection; close the workspace yourself afterwards (below). Retry with `"$TASKQ" ready ID` after fixing the cause; the resident supervisor makes a new run.
- unfinished with `last_error` set and no lease in `status` (a `runtime_error` event with `lease_released: true`): the supervisor gave this run up (wrapper heartbeat lost, exit request timed out, provisioning or validation error) and kept serving the others; use the `taskq-recover` skill. After a provisioning failure the supervisor stops claiming, finishes its active runs and exits nonzero; launchd restarts it, so fix the cause (cmux, Git) and check `status`.
- `interrupted`: recovered; retry with `ready ID`. Its workspace is kept too; close it yourself (below).

The runtime closes only the workspace of an accepted run (`awaiting_integration`). It never closes the workspace of a `failed` or `interrupted` run, so once the user has inspected its screen, worktree, and `last_error`, close it:

```sh
cmux workspace close <workspace_id>
```

`workspace_id` is the run's `workspace_id` in `"$TASKQ" show ID` (`doctor` lists only unfinished runs, so a `failed` run is not there). The session in it has already exited (both states require that), so only the shell is left; closing the workspace discards its screen, so read it first if `last_error` is not enough. The worktree, branch, and run directory stay on disk for the user's manual cleanup, as the `taskq-recover` skill describes.

One run's failure never changes another run: each is judged, closed and released on its own. Completion is decided only by these states. A Stop hook, an idle session, or a `receipt.json` appearing in `run_dir` is not success: the receipt's claims are cross-checked by the supervisor, and `validation_finished` in `events` records the verdict and reason. If a run stays `running` after the supervisor logged `exit_request_timed_out`, tell the user to send `/exit` in that task's workspace themselves.

## 5. Review and land

When the run is `awaiting_integration`, tell the user the branch and `result_commit`, and how to review:

```sh
git log main..taskq/<run-id>
git diff main...taskq/<run-id>
```

together with the `receipt.json` and `verify-N.log` files in `run_dir` (`show ID`); `integrate` writes its own re-validation logs there as `integrate-verify-N.log`. A run that changed the runtime itself is also worth a `cargo test --locked --test e2e -- --ignored` in its worktree before landing, when the repository asks for it. If the receipt has `follow_ups` (work the worker found outside its task, as `{title, description}`; also in the `receipt` of the `validation_finished` event), report them to the user before `integrate` and again with the outcome after it, so they are registered with the `taskq` skill (`add --goal ID` on the task's goal, or a plain `add` when the task has none) rather than lost with the run. Never merge, rebase, or fast-forward the branch yourself: landing is the runtime's job and it keeps `main` linear with one squash commit per task. Only land when the user has approved the run.

```sh
"$TASKQ" integrate ID        # this task's run (also resumes a needs_session run)
"$TASKQ" integrate --next    # the oldest run awaiting integration
```

`integrate` takes the single integration slot, rebases the run's worktree onto the current `main`, re-validates it (the receipt must name the worktree head, the rebased head must sit on `main` with a clean tree, and the task's verification commands are rerun), squashes the rebased tree into one commit on `main` (title, receipt summary, trailers `Taskq-Task: ID` and `Taskq-Run: RUN_ID`), and removes the worktree and branch; the run's history stays under `refs/taskq/runs/<run-id>`. Read the `outcome`:

- `integrated`: `run.result_commit` is the new `main` head and the task is `completed`, which unblocks dependents (`candidates`); the resident supervisor claims them on its next poll from the landed `main`. Pushing `main` is the maintainer's step and the user's call.
- `needs_session`: nothing reached `main`; `reason` names the conflicting files (the rebase was aborted and the worktree is back on `result_commit`) or the failed verification command, whose output is in `integrate-verify-N.log` in `run_dir` (the rebased tree is left in the worktree). Continue with step 6.
- `failed`: the run's rewritten receipt reported `failed`; the task stays `in_progress` and can be retried with `ready ID` or dropped with `cancel ID`.
- `no_run_awaiting` (`--next` only): the queue is empty; `needs_session` runs are not picked by `--next`.

An error (exit status 1) before `main` moved puts the run back to its previous state with the message in `last_error`; a `main` checkout with uncommitted changes that overlap the landing is a common cause. Fix it and run `integrate` again. The landing happens in the current directory's repository; pass `--repo PATH` only when using `CMUX_TASKQ_DB` from outside it.

## 6. Resume a run that needs a session

A `needs_session` run is fixed by a Claude session in its own worktree, not by this session and not by hand. Take `worktree_path`, the run `id`, and `last_error` from `"$TASKQ" show ID`, then open a workspace that resumes the run's session (its session ID is the run ID):

```sh
cmux workspace create --name "taskq resume <run-id>" --cwd "<worktree_path>" \
  --command "claude --resume <run-id>"
```

Send it the reason from `last_error` and this instruction: rebase the branch onto the `main` commit named in the reason (`git rebase <main commit>`), resolve the conflicts (or fix what broke the verification command on the rebased tree), rerun the task's verification commands, commit, keep the worktree clean, and rewrite `receipt.json` in `run_dir` by atomic rename with the new head commit as `commit`; if the change is no longer needed, write `"result": "failed"` with the reason in `summary` instead. The session must not merge or push. When it reports done, end it with `/exit` in that workspace — landing removes the worktree it works in — and run `"$TASKQ" integrate ID` again: it repeats the rebase (a no-op unless `main` moved again) and the re-validation and lands the run, keeps it `needs_session` with a new reason if the receipt does not name the current head, or marks it `failed` on a failed receipt.

## 7. Stop the runtime

```sh
"$TASKQ" down            # unload the agent and return; the supervisor drains its runs
"$TASKQ" down --wait     # unload, then block until its registration is gone
"$TASKQ" down --force    # unload, then SIGKILL it at once and drop its registration
```

`--wait` and `--force` exclude each other: `--force` does not drain first.

Use plain `down` to end the day's work: the supervisor stops claiming, finishes its active runs and exits, and launchd does not restart it. Use `--wait` when the next step depends on it being gone (rebuilding or replacing the binary, a machine the user is about to shut down); it blocks while the runs drain, which can take as long as a run. Use `--force` only when the user accepts losing the active runs: their leases go stale after 30 seconds and the runs are then handled with the `taskq-recover` skill. The `outcome` is `draining` (plain `down`, which returns at once), `stopped` (`--wait`, the registration is gone), `killed` (`--force`), or `not_running` when no live supervisor was registered — a lingering agent is unloaded in that case too, and `--force` also drops the dead registrations. `down` never closes the maintainer workspace, and it does not stop the workers' own sessions.

An `in_cmux` supervisor (`mode` in `status`) has no launchd agent to unload, so `down` sends it SIGINT instead and closes the `taskq <repo> supervisor` workspace it ran in once it has seen the stop through — after the drain with `--wait`, after the kill with `--force`. Plain `down` returns while that supervisor is still draining, so it leaves the workspace open and reports it under `supervisor_workspaces` as `left_open`; run `down --wait` (or have the user close it) before the next `up --in-cmux`, which refuses to open a second supervisor workspace over a leftover one.

## 8. Logs

`"$TASKQ" locate` prints `log_dir` (plus `label` and `launch_agent` for the LaunchAgent). Each supervisor start appends to its own `supervisor-<started_at>-<pid>.log` there — token, PID, `--parallel`, queue and repository, the claims, workspaces, receipts, exit requests and rejections, and the final result — and launchd's own stdout/stderr for the agent go to `launchd.log` in the same directory. Read these when a supervisor is missing from `status`, when `up` reports that no supervisor registered, or when a run failed for a reason `show` does not explain. Nothing rotates them; deleting old files is the user's call.
