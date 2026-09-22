# cmux-taskq

cmux-taskq is a Rust task orchestrator for running dependency-aware development tasks in cmux workspaces and isolated Git worktrees.

The runtime is distributed as a binary. Claude Code and Codex integrations are distributed as plugins that invoke the binary.

## Current status

The Rust/SQLite queue and a parallel supervisor are implemented. Tasks, dependencies, state transitions, candidate selection, run reservation, per-run supervisor leases, process heartbeats, and events are persisted locally. `supervise` is a resident loop: it claims dependency-ready tasks up to `--parallel N` (default 4), creates a Git worktree and a cmux workspace for each, starts an interactive Claude Code session through a wrapper, records each session's exit, validates each completion receipt against Git and the task's verification commands, and closes the workspace of an accepted run. `integrate` is the merge queue: it lands one validated run at a time on `main` by rebasing its worktree onto the current `main`, re-validating it, and squashing it into a single commit, and the supervisor picks up the tasks that unblocks. Claude Code's interactive lifecycle was [verified first](docs/journal/001-claude-lifecycle-spike.md).

A validated run stays in `awaiting_integration`; its workspace is closed, while its worktree and branch are kept until `integrate` lands it. A run whose rebase conflicts, or whose verification fails after the rebase, waits as `needs_session` for a resumed Claude session to fix it. One run's failure never touches another: each run has its own lease, and `doctor` / `recover` judge and release one run at a time. A supervisor that dies while a run's session is still alive does not lose the run: the next supervisor with a free slot adopts the stale lease and finishes the run; every other stale lease waits for `recover`.

## Build and try the queue

Requires Rust 1.93 or newer and a C compiler for bundled SQLite. No separate SQLite installation is required.

Each Git repository has one queue. Run the binary from anywhere inside the repository (any worktree, including a task worktree) and it resolves the queue to `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`, by default `~/.local/share/cmux-taskq/<hash>/queue.db`, where `<hash>` is the first 16 hex digits of the SHA-256 of the repository's canonical Git common directory. `locate` prints that resolution without opening anything; `init` creates the directory and the queue.

The examples assume `target/debug/cmux-taskq` is on PATH after `cargo build --locked`.

```sh
taskq_demo=$(mktemp -d) && git -C "$taskq_demo" init -q -b main && cd "$taskq_demo"
export XDG_DATA_HOME="$taskq_demo/data"   # keep the demo out of ~/.local/share
cmux-taskq locate
cmux-taskq init
cmux-taskq add "Improve setup docs" \
  --description "Explain the local setup procedure" \
  --acceptance "A new contributor can follow the documented commands" \
  --verify "git diff --check"
cmux-taskq add "Check setup examples" --depends-on 1
cmux-taskq ready 1
cmux-taskq ready 2
cmux-taskq candidates
cmux-taskq show 2
```

The fresh queue assigns task IDs 1 and 2. Only task 1 is a candidate; task 2 remains blocked until task 1 is completed. Every shell inside the same repository sees the same queue. Only `init` creates a database. `--db PATH` before the subcommand uses an explicit queue file instead, for disposable repositories and tests; `init` creates its directory as well.

Commands return JSON on stdout. Runtime errors return JSON on stderr with a nonzero exit status; argument errors and help use the standard CLI format. Outside a repository, every command except `--db` usage fails with an error that says so.

| Command | Effect |
| --- | --- |
| `locate` | Show the queue this directory resolves to: `db`, `queue_dir`, `runs_dir`, `log_dir`, `label` and `launch_agent` (the supervisor's launchd label and plist path, whether or not `up` has written it), `source` (`repository` or `db_flag`), `git_common_dir`, `db_exists` |
| `init` | Create or migrate the queue and bind it to this repository; preserves existing tasks |
| `add TITLE [--description TEXT] [--acceptance TEXT] [--verify COMMAND] [--depends-on ID] [--goal ID] [--context TEXT]` | Register a draft; `--verify` and `--depends-on` can be repeated. `--goal` joins an open goal, `--context` records why the task exists and what to read first |
| `list` / `show ID` | Inspect tasks; `show` includes dependencies, runs and events, and the task's `goal_id` and `context` |
| `ready ID` / `draft ID` | Move between draft and ready; also allowed from `in_progress` once every run has failed or been interrupted |
| `cancel ID` | Cancel a draft, ready, or retryable in-progress task; does not satisfy its dependents |
| `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` | Change prerequisites of a draft or ready task |
| `candidates` | List dependency-ready tasks in registration order without reserving them |
| `goal add TITLE [--description TEXT] [--acceptance TEXT] [--constraints TEXT] [--doc PATH]` | Register a goal: the higher-level problem a group of tasks solves. A goal has no state machine and no verification commands |
| `goal list` / `goal show ID` | `list` gives each goal's `closed`, `verdict` and task counts by status; `show` adds the goal's tasks (id, title, status) and its events |
| `goal edit ID [--title TEXT] [--description TEXT] [--acceptance TEXT] [--constraints TEXT] [--doc PATH]` | Replace fields (an empty `--doc` clears it); the old and new goal go into a `goal_updated` event. Runs already started keep their prompt snapshot |
| `goal close ID --verdict achieved\|abandoned` | Record the verdict once. `achieved` is refused while a task is neither completed nor canceled; `abandoned` while a task is in progress. A closed goal accepts no more tasks |
| `set-goal TASK GOAL` / `set-goal TASK --none` | Move a draft or ready task into an open goal, or out of its goal (same rule as dependency changes) |
| `up [--parallel N] [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE]` | Start the queue's runtime, idempotently: prune supervisor registrations whose process is gone, keep the supervisor resident as a launchd LaunchAgent (started only when no live one is registered), and open the maintainer's Claude Code session in the cmux workspace `taskq <repo> maintainer` (skipped inside that session, reused if open). Reports what needs attention: unfinished runs, runs awaiting integration, runs that need a session |
| `down [--wait] [--force]` | Stop the supervisor by unloading its LaunchAgent so it drains its runs and is not restarted; `--wait` blocks until it has deregistered or exited, `--force` kills it and drops its registration. The maintainer workspace stays open |
| `supervise [--parallel N] [--once] [--repo PATH] [--cmux EXE] [--claude EXE] [--log-dir DIR]` | Resident loop: claim dependency-ready tasks up to `N` (default 4), run each in its own cmux workspace, request exit once its receipt is in and Claude is idle, validate the receipt, close the workspace on success, and keep polling for new candidates (including tasks unblocked by `integrate`). `--once` exits when nothing is active or claimable. `--log-dir` adds one `supervisor-<started_at>-<pid>.log` per start. The checkout is the working directory unless `--repo` says otherwise; `up` runs it under launchd with the queue's `logs/` directory |
| `integrate ID` / `integrate --next` `[--repo PATH]` | Land a validated run on `main`: rebase its worktree onto the current `main`, re-validate, squash into one commit with `Taskq-Task` / `Taskq-Run` trailers, mark the task `completed`, remove the worktree. `--next` takes the oldest awaiting run; `ID` also resumes a `needs_session` run. Lands in the working directory's repository unless `--repo` says otherwise |
| `status` | List every registered supervisor (PID, liveness, heartbeat age, `parallel`, the runs it holds), any other lease holder such as an `integrate` process, and the unfinished runs with their leases |
| `doctor` | Report the same supervisors plus every unfinished run with its lease, wrapper/agent processes, heartbeats, paths, and recovery blockers without changing state |
| `recover RUN_ID` | Mark one unfinished run `interrupted` and drop its lease once its processes and supervisor are gone; keeps its worktree and workspace, and leaves other runs alone |

Verification commands are shell lines that the supervisor runs in the worktree (`/bin/sh -c`) after the session exits; the agent is asked to run them too. Task descriptions and acceptance criteria are optional during registration.

## Start the runtime with `up`

Three roles share a queue: the **supervisor** is the resident `supervise` process that runs tasks, the **maintainer** is the resident Claude Code session that registers, watches, reviews and lands them, and a **worker** is the Claude session of one run. `up` starts the first two from a shell whose PATH has `cmux-taskq`, `cmux` and `claude`:

```sh
cmux-taskq up --parallel 4 --plugin-dir /path/to/cmux-taskq/plugins/claude-taskq
```

`up` checks cmux (`ping`), Claude (`--version`) and the queue (`init` first), deletes supervisor registrations whose process is dead (`pruned_supervisors`), and then keeps one supervisor resident as a launchd LaunchAgent. Before it writes anything it proves that cmux will admit that supervisor (see the prerequisite below): it runs `cmux ping` the way the supervisor will run, outside cmux's process tree (orphaned, so launchd is its parent) and with the environment the agent will carry and none of the `CMUX_*` variables of the terminal it runs in, and fails with the remedies if cmux refuses. Then it writes `~/Library/LaunchAgents/com.cmux-taskq.<queue hash>.plist` (`locate` shows the path as `launch_agent`) whose `ProgramArguments` run this binary's `supervise --parallel N --log-dir <queue dir>/logs` from the repository root with your shell's PATH, `KeepAlive` and `RunAtLoad` so it restarts after any exit, and stdout/stderr in `<queue dir>/logs/launchd.log`; loads it with `launchctl bootstrap gui/<uid>`; and waits for the supervisor to register itself (`{"supervisor": {"outcome": "started", "pid": ...}}`). A supervisor that is already registered, alive and heartbeating is reused and neither the plist nor launchd is touched (`"reused"`). Then it opens the maintainer session: a cmux workspace named `taskq <repo> maintainer` (`<repo>` is the repository directory's name) whose command starts `claude` with `CMUX_TASKQ_ROLE=maintainer` and `CMUX_TASKQ_QUEUE=<db>` in its environment, `--plugin-dir` if given, and a generated prompt that names the queue, the roles and the log directory and asks the session to report `status` and `doctor` through the plugin's `taskq-maintain` skill and wait for you. The workspace is `"reused"` when one with that title is open and `"skipped"` when `up` runs inside the maintainer session itself (the plugin skill calls it), so running `up` twice changes nothing the second time. The result ends with a `doctor` summary: unfinished runs (with `lease_stale`), runs `awaiting_integration`, and runs in `needs_session`.

**Prerequisite: cmux must accept a connection from outside its terminals.** cmux decides by process ancestry, not by environment: a client that descends from one of its terminals is admitted whatever its variables, and one under launchd is refused (`only processes started inside cmux can connect`, cmux 0.64.25 in its default `cmuxOnly` mode) unless a socket password admits it (`cmux --help`, Socket Auth: `--password`, then `CMUX_SOCKET_PASSWORD`, then the password saved in cmux's Settings, which its CLI uses on its own). Save a socket password in cmux's Settings once (`automation.socketControlMode: "password"` and `automation.socketPassword` in `~/.config/cmux/cmux.json`, then `cmux reload-config`), or export `CMUX_SOCKET_PASSWORD` in the shell that runs `up`; in that case, and only then, `up` stores it in the plist's `EnvironmentVariables` next to PATH (the plist is written mode 0600), so prefer the Settings-saved password. Without either, `up` stops before the plist exists: `cmux refused a connection from outside its own terminals ... save a socket password in cmux Settings ... or export CMUX_SOCKET_PASSWORD ...`, followed by cmux's own error. (A ping that cannot be run or does not answer within 60 seconds is reported as `cmux could not be asked ...` instead; that is not a password problem.) A supervisor that is already registered and alive (one you started by hand in a cmux terminal, below) is reused without this check, so until an in-cmux mode of `up` exists that is the fallback: start `supervise` yourself and run `up` for the maintainer workspace.

```sh
cmux-taskq down          # unload the agent; the supervisor drains and exits
cmux-taskq down --wait   # ... and wait until it has deregistered
cmux-taskq down --force  # ... then SIGKILL it and drop its registration
```

`down` unloads the LaunchAgent (`launchctl bootout`) and removes the plist, so the supervisor receives SIGTERM, stops claiming, waits for its active runs, deregisters and exits without being restarted or coming back at the next login; a signal alone would only make launchd restart it. A supervisor started by hand (one that is not the agent's process) gets the SIGTERM from `down` itself. Without a live registration `down` reports `not_running` (still unloading a lingering agent, and with `--force` also dropping dead registrations). It never closes the maintainer workspace. Each supervisor start writes its own `supervisor-<started_at>-<pid>.log` under `<queue dir>/logs` with its token, PID, `--parallel`, queue and repository, the progress messages that also go to stderr, and its final result.

## Run tasks with the supervisor

`up` is the normal way to start the supervisor. You can also run it yourself in a dedicated terminal inside the repository, with cmux and an authenticated Claude Code on PATH (or `--cmux` / `--claude`); it stays resident and runs dependency-ready tasks as they appear, up to `--parallel` at once.

```sh
cmux-taskq supervise --parallel 4
```

The supervisor registers itself in the queue when it starts (its PID, `--parallel`, and start time), refreshes that registration with its leases every 2 seconds, and removes it whenever it exits: after Ctrl-C drained its runs, after `--once` ran out of work, after a provisioning failure drained them, or on an error such as an unreadable `main`. Only a heartbeat failure keeps the row, since the database may be unreachable. `status` and `doctor` list it under `supervisors` from its first second, holding runs or not, so you can tell whether a resident loop exists before starting another. A supervisor that was killed or hangs leaves its row behind: it is shown with `alive: false` or a growing `heartbeat_age_secs` and `stale: true`; the next `up` removes the rows whose process is dead and reports them as `pruned_supervisors`, and nothing else removes them.

Its runs are not lost. Before claiming, whenever it has a free slot, a supervisor looks for `running` or `validating` runs whose lease belongs to another supervisor and is stale (its PID is dead, or its heartbeat is older than 30 seconds) while the run's session wrapper is still alive and heartbeating, or has already recorded its exit. It adopts such a run in one transaction: the lease and the run's `supervisor_token` move to the adopter and a `run_adopted` event records the previous token and PID, how old the heartbeat was, and the wrapper's state. The adopter then watches the run like one it claimed, without repeating an `/exit` that was already requested, and restarts validation for a `validating` run. So after `up` replaces a killed supervisor (a binary update, a `down --force`), the runs in flight simply continue under the new one. Nothing else is adopted: a `claimed` or `starting` run, a run whose wrapper is dead or silent, a run without a lease (given up by its supervisor) and an `integrating` run are left for `recover`. Two supervisors that find the same stale lease adopt it exactly once, and a supervisor whose lease was taken from it stops watching that run without writing anything more about it.

Every few seconds the supervisor looks for candidates and claims them until `--parallel` runs (default 4) are active. Each claim reads `refs/heads/main` again and uses it as the run's base commit, whichever worktree you start from, so a task released by `integrate` starts from the `main` that contains its predecessor's landing; pass `--repo PATH` to use a checkout other than the working directory. Each run gets its own lease (`lease_acquired` / `lease_released` events), a cmux workspace named `taskq <repo> <task-id> <run-id>`, and a worktree, and goes through the same state machine independently of the others. Runtime files live next to the database in `runs/<run-id>/` (see `locate`'s `runs_dir`): the prompt, a snapshot of the runtime binary, the worktree on branch `taskq/<run-id>`, Claude's per-run settings and debug log, the idle marker, the receipt, and the final terminal screen. The prompt also names the task's direct predecessors (each with the commit `integrate` landed and its receipt summary) and the other tasks in progress at claim time, so a dependent run knows what it builds on and what runs beside it. The workspace command starts a hidden `session` wrapper that launches Claude with the run ID as its session ID and reports heartbeats and the exit code.

The loop ends in three ways. Ctrl-C (or SIGTERM) once stops claiming and waits for the active runs to finish; a second Ctrl-C terminates the process immediately and its leases go stale after 30 seconds. `--once` exits as soon as no run is active and no task is claimable, which suits a single batch or a test. A provisioning failure (worktree or workspace creation) is treated as an environment problem: the supervisor gives that run up, stops claiming, waits for its other runs, and exits with an error. The final JSON lists the runs that came to rest under `runs` and the runs it gave up under `errors`.

Claude stops at its folder-trust prompt in a run's workspace only when the repository itself has never been trusted: the prompt is decided by the repository root, not by the worktree path, so run `claude` once in the repository root and accept the prompt before the first `supervise` on a repository, and every run worktree starts without it (see [Trust prompt](docs/design/provider-lifecycle.md#trust-prompt)). Permission prompts can still appear in each workspace; answer them there. A receipt does not end the session. Claude is started with a per-run `--settings` file whose `Stop` hook writes `<run-dir>/idle.json` each time a response finishes; once that marker is newer than the receipt, the supervisor records `session_idle_observed` and sends `/exit` to the workspace once (`exit_requested`). You can still send `/exit` yourself at any time, and you must if the hook is disabled or Claude is waiting on a prompt. If the session has not exited 120 seconds after the request, the supervisor records `exit_request_timed_out` and gives that run up (see below), leaving it `running` with its workspace and worktree intact for you to finish by hand while the other runs continue. A nonzero exit code marks the run `failed` with `session exited with code N` in `last_error`. With exit code 0 the supervisor validates the receipt: it must name this run, report `succeeded`, give evidence for passed checks and a reason for `not_applicable` ones, and its commit must be the clean head of the run branch on top of the base commit. The supervisor then reruns the task's verification commands in the worktree, logging each to `<run-dir>/verify-N.log`. A run that passes becomes `awaiting_integration` with its `result_commit` recorded; anything else becomes `failed` with the reason in `last_error`. Only an accepted run has its cmux workspace closed (`workspace_closed_at` is set once cmux confirms); its worktree and branch stay until integration. If the close fails, the run stays `awaiting_integration` with a `cleanup_failed` event and the error in `last_error`, and `workspace_closed_at` stays null so the workspace is not treated as cleaned. A failed run keeps its workspace, worktree, and branch. A run that reached `awaiting_integration` or `failed` releases its lease.

If something goes wrong around a run rather than in it (its wrapper stops heartbeating, the exit request times out, validation itself errors, or the close cannot be recorded), the supervisor gives that one run up: it records the cause in `last_error` and a `runtime_error` event, deletes the run's lease, and leaves its status, processes, workspace, and worktree untouched, then keeps serving the other runs. Such a run shows up in `doctor` without a lease. Nothing is rerun automatically.

A run's `last_error` (in `show`, `doctor`, and the supervisor's final `errors`) holds only the latest of these messages; `integrate` clears it when the run lands, and the full history is in the run's events. A nonzero session exit writes `session exited with code N` (128 for a session killed by a signal) as the run becomes `failed`. A rejected receipt writes the first failed check verbatim as the run becomes `failed`: `receipt was not submitted at <path>`, the parse or `run_id` error for a malformed receipt, `no commit was made on top of base <sha>`, `worktree is on <ref> instead of refs/heads/<branch>`, `receipt commit <sha> is not the head of <branch> (<head>)`, `commit <sha> does not descend from base <sha>`, `worktree is not clean:` followed by the `git status` output, or `verification command "<cmd>" exited with <code>; see <run-dir>/verify-N.log`. A runtime or provisioning error writes the error text as-is next to a `runtime_error` event without changing the status: `run <run-id> provisioning failed: ...` when the worktree or workspace could not be created, `wrapper heartbeat expired; session may still be alive`, `session did not exit within 120s of the exit request; ...`, or the Git or database error from validation itself. A cleanup failure writes `workspace <workspace-id> could not be closed: ...` (the run stays `awaiting_integration`) or, after landing, `landed worktree <path> could not be removed: ...` next to a `cleanup_failed` event. `integrate` writes it too: `needs_session` carries the rebase or re-validation reason, an error before `main` moves puts the run back with `integration stopped before main moved: ...`, and a rewritten `failed` receipt marks the run `failed` with the receipt's reason (see [Land a run on main](#land-a-run-on-main)). Read it together with the status and the last event: `failed` means a nonzero exit, a rejected receipt, or a failed receipt at integration; `claimed`, `starting`, `running`, or `validating` with a `last_error` means the supervisor gave the run up; `awaiting_integration` with a `last_error` means the close failed (`cleanup_failed`, `workspace_closed_at` null) or an `integrate` attempt stopped before `main` moved (`integration_error`); `needs_session` means `integrate` parked it.

## Recover an interrupted run

A run is orphaned when its supervisor gave it up, or was killed or lost its heartbeat while no supervisor with a free slot could adopt it (its session wrapper was dead or silent too, or it had not reached `running`). The task stays `in_progress` and the run keeps its resources; other runs, and a supervisor still running them, are unaffected. Inspect first:

```sh
cmux-taskq doctor
```

`doctor` lists the supervisors under `supervisors`: every registered `supervise` process (`registered: true`, with `pid`, `alive` from `kill -0`, `parallel`, `started_at`, `heartbeat_at`, `heartbeat_age_secs`, the `run_ids` it holds, and `stale` when the PID is dead or the heartbeat is older than 30 seconds) and any other process that holds a lease without a registration, such as a running `integrate` (`registered: false`). A stale registration is reported, never deleted; `recover` and `integrate` do not touch it. Then every run in `claimed`, `starting`, `running`, `validating`, or `integrating` with its workspace ID, whether its worktree and run directory exist, its own lease (or `null`), and each registered wrapper/agent process with its PID, liveness (`kill -0`), and heartbeat age. `blockers` names what would stop a recovery of that run, considering only its own lease and processes; `recoverable` is true when the list is empty. Stop the listed processes yourself, for example by exiting the session in its workspace.

```sh
cmux-taskq recover <run id>
```

`recover` refuses while any process registered for that run is still alive, its lease heartbeat is fresh, or its lease's supervisor PID is alive. Otherwise it marks the run `interrupted`, records a `run_recovered` event with the state it checked, and deletes that run's lease only. The worktree, branch, and workspace are kept for inspection, and the task stays `in_progress`. To retry, make the task ready again with `ready ID` (or `draft ID` to edit it first); a running supervisor (or the next one) creates a new run with its own worktree. The same applies to a task whose last run `failed`.

The runtime never closes the cmux workspace of a failed or interrupted run. Once you are done inspecting it, close it yourself with `cmux workspace close <workspace_id>`; the workspace ID is the run's `workspace_id` in `show ID` (`doctor` lists only unfinished runs, so a failed or interrupted run is not there). If you instead resolved the run in a `claude --resume <run-id>` session (see below), exit that session with `/exit` before running `integrate ID`, because landing removes the worktree the session is working in.

The receipt is JSON at `<run-dir>/receipt.json`, written by atomic rename:

```json
{"run_id": "<run id>", "result": "succeeded", "commit": "<full SHA>",
 "tests": {"status": "passed", "evidence_or_reason": "cargo test: 15 passed"},
 "e2e": {"status": "not_applicable", "evidence_or_reason": "library change"},
 "subagent_review": {"status": "passed", "evidence_or_reason": "no findings"},
 "summary": "..."}
```

`status` is `passed`, `failed`, or `not_applicable`; `result` is `succeeded` or `failed`. The receipt's claims never make a run succeed on their own.

## Land a run on main

Review the run branch `taskq/<run-id>` (`git log main..taskq/<run-id>`, `git diff main...taskq/<run-id>`, the `receipt.json` and `verify-N.log` files in the run directory). Do not merge it yourself; landing is the runtime's job, one run at a time:

```sh
cmux-taskq integrate 1        # the task's run
cmux-taskq integrate --next   # the oldest run awaiting integration
```

`integrate` marks the run `integrating` (holding the queue's single integration slot with a lease of its own), rebases the run's worktree onto the current `refs/heads/main`, and re-validates it: the receipt must name the worktree's head and report `succeeded`, the rebased head must sit on `main` with a clean tree, and the task's verification commands are rerun (`<run-dir>/integrate-verify-N.log`). It then squashes the rebased tree into **one** commit on top of `main` whose message is the task title, the receipt's summary, and the trailers `Taskq-Task: <id>` and `Taskq-Run: <run-id>`. Where `main` is checked out the commit is fast-forwarded through that checkout so its files move too; otherwise the ref is updated directly. No merge commit and no fast-forward of the run branch: `main` stays a straight line with one commit per task, and the landed tree equals the validated worktree's tree. The run becomes `integrated` with the landed commit as its `result_commit`, the task becomes `completed` (`run_integrated` and `task_status_changed` events), the run's own history is kept under `refs/taskq/runs/<run-id>`, and the worktree and branch are removed. Dependent tasks then appear in `candidates`; a running supervisor claims them on its next poll with the landed `main` as their base. `--next` lands runs in the order their validation finished and prints `{"outcome": "no_run_awaiting"}` when none is left.

If the rebase conflicts, `integrate` aborts it, leaves the worktree on its validated head, and parks the run as `needs_session` with the conflicting files in `last_error` (`{"outcome": "needs_session", "main": ..., "reason": ...}`). If the rebase applies but a verification command fails on the result, the run is parked the same way with the rebased tree left in place. Nothing reaches `main`. Reopen a Claude session in that worktree (`claude --resume <run-id>` in a cmux workspace whose `--cwd` is the worktree) and have it rebase onto `main` (`git rebase <main commit>` from the reason), resolve, rerun the verification commands, and rewrite `receipt.json` with the new head commit; if the change is no longer needed, have it write `"result": "failed"` with the reason in `summary`. Exit that session with `/exit` before running `integrate ID`: landing removes the worktree, so a session still working in it must not be running. Then run `integrate ID` again: it repeats the same steps from the rebase (a no-op unless `main` moved again) and lands the run, keeps it `needs_session` with a new reason if the receipt does not name the current head, or marks it `failed` (`{"outcome": "failed", ...}`) on a failed receipt so the task can be retried with `ready ID` or dropped with `cancel ID`. `--next` never picks a `needs_session` run; it is resumed explicitly. A parked or awaiting run keeps its task `in_progress`. The runtime never closes the cmux workspace of a failed or interrupted run: close it with `cmux workspace close <workspace_id>` (the ID is the run's `workspace_id` in `show ID`; `doctor` does not list finished runs) once you no longer need it.

An error before `main` moves (a missing worktree, a `main` checkout with local changes that collide with the landing, a Git failure) puts the run back where it was with the message in `last_error` and an `integration_error` event; fix the cause and run `integrate` again. An `integrate` process that dies leaves its run `integrating` with a stale lease: `doctor` lists it, and `recover RUN_ID` returns it to `awaiting_integration` once the process is gone. A task with no run awaiting integration or a session is an error, so a task cannot be landed twice.

The landing happens in the working directory's repository; pass `--repo PATH` to name another checkout, for example when using `--db` from elsewhere. Either way it must be the repository the queue is bound to: a queue resolved from the working directory is bound by `init`, a `--db` queue by its first `supervise`, and a mismatch is an error. Pushing `main` stays with you.

## Use from Claude Code

`plugins/claude-taskq` is a Claude Code plugin whose skills drive the binary; it has no hooks and never opens the queue database itself. Build the binary, then load the plugin for a session:

```sh
cargo build --locked
export CMUX_TASKQ_BIN="$PWD/target/debug/cmux-taskq"   # or put it on PATH
claude --plugin-dir "$PWD/plugins/claude-taskq"
```

The queue is the one of the repository you run Claude Code in, resolved by the binary as described above and shared by all of its worktrees; set `CMUX_TASKQ_DB=/path/to/queue.db` to use another file, which the launcher passes as `--db`. The plugin's launcher `bin/taskq` resolves the binary and forwards any command from the current directory (`bin/taskq --resolve` shows the binary, its version, and `locate`'s output).

| Skill | Covers |
| --- | --- |
| `/claude-taskq:taskq` | Locate the binary and queue, `init`, register a goal with `goal add` and decompose it into tasks with `add --goal` (description, acceptance, `--verify`, `--depends-on`, `--context`), `ready`, `list` / `show` / `candidates` / `locate` / `status` / `doctor`, how to read run states, close a goal |
| `/claude-taskq:taskq-maintain` | The maintainer's side: start the runtime with `up`, read `status` for a stale supervisor, watch runs with `show`, judge completion from the run state and receipt rather than a Stop hook, answer a run's prompts, review and land with `integrate`, resume a `needs_session` run, close the workspace of a failed run, report a receipt's `follow_ups`, stop the runtime with `down`, and find the supervisor's logs |
| `/claude-taskq:taskq-recover` | `doctor`, `recover RUN_ID` for one run without disturbing the others, retry with `ready` |

Claude picks the skill from the request ("queue a task to …", "start the runtime", "did task 3 finish?", "the supervisor died"). `up` opens the maintainer session with this plugin loaded when it is given `--plugin-dir`. `claude plugin validate plugins/claude-taskq` checks the manifest and skills; `tests/plugin.rs` checks them and the launcher in `cargo test`.

## Development checks

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

The tests use temporary databases and repositories, point `XDG_DATA_HOME` at temporary directories, and do not require cmux, Claude Code, or network access after dependencies have been fetched. Line coverage must stay at or above 80% (`cargo install cargo-llvm-cov`). The end-to-end happy paths in `tests/e2e.rs` (one task landed by `integrate`; two tasks in parallel followed by a dependent one, with a conflicting run parked as `needs_session` and landed after the test resolves it; and `up` → `status` → `down --wait` against the real launchd, with the plist under a disposable `HOME`) drive the real binary through cmux with a stub agent, resolving the queue from the disposable repository's working directory, and are ignored by default; run them with `cargo test --locked --test e2e -- --ignored` where cmux is available. The `up` / `down` test needs cmux to accept the launchd-run supervisor's connection (a socket password in cmux's Settings). The fourth e2e kills a `supervise` process while its stub worker runs and checks that the next `supervise --once` adopts and lands the run. `tests/lifecycle.rs` covers `up` and `down` against fakes for launchd, cmux and process signals.

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Active plan](docs/plans/current.md)
- [Task journals](docs/journal/README.md)
- [Architecture decisions](docs/adr/README.md)
- [Agent instructions](AGENTS.md)

## License

cmux-taskq is released under the [MIT License](LICENSE); see that file for the full text.
