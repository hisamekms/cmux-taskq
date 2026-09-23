# dagq

dagq is a dependency DAG queue: a Rust task orchestrator for running dependency-aware development tasks in cmux workspaces and isolated Git worktrees. The name is the shape of the work — tasks form a dependency DAG, and the queue runs the ones whose predecessors have landed. (It is unrelated to DAQ, data acquisition.)

**It runs on macOS on Apple Silicon (`aarch64-apple-darwin`) only.** No other platform is built, released, or tested. It also needs cmux, which hosts a workspace per run, and an authenticated Claude Code on PATH, because every run is a Claude Code session.

The runtime is distributed as a binary from [GitHub Releases](https://github.com/hisamekms/dagq/releases). The Claude Code integration is distributed as a plugin that invokes that binary, and this repository is also its marketplace. Claude Code is the only provider today; a Codex plugin is planned but does not exist yet.

## Getting started

How to start using dagq in a repository of your own. Each step links to the section that covers it in full.

**1. Download the release.** Put `dagq-v<version>-aarch64-apple-darwin.tar.gz` and `SHA256SUMS` from the [latest release](https://github.com/hisamekms/dagq/releases/latest) in the same directory.

```sh
VERSION=0.2.0
gh release download "v$VERSION" --repo hisamekms/dagq
```

**2. Verify the checksum.**

```sh
shasum -a 256 -c SHA256SUMS
```

Do not unpack an archive that does not print `OK`.

**3. Install the binary into `~/.local/bin` and check your PATH.** The archive holds `dagq`, `LICENSE` and `README.md` at its top level.

```sh
tar -xzf "dagq-v$VERSION-aarch64-apple-darwin.tar.gz"
mkdir -p ~/.local/bin
install -m 755 dagq ~/.local/bin/dagq
command -v dagq   # expect the ~/.local/bin one, printed expanded
```

One installed file is enough to serve the queue: it is what you type, what the plugin's launcher resolves ([Use from Claude Code](#use-from-claude-code)), and what the resident supervisor runs, since `up` starts `supervise` from the absolute path of the binary it was invoked as rather than from PATH.

**4. Install the Claude Code plugin.** It carries the skills that drive the binary and the launcher they call, and nothing else; the binary is the one you just installed.

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

See [Use from Claude Code](#use-from-claude-code) for what each skill covers.

**5. Save a cmux socket password in cmux's Settings.** The supervisor runs under launchd, and cmux refuses a connection from outside its own terminals unless a socket password admits it, so `up` stops before it writes anything without one. Set `automation.socketControlMode: "password"` and `automation.socketPassword` in `~/.config/cmux/cmux.json`, then `cmux reload-config` ([Prerequisite: cmux must accept a connection from outside its terminals](#start-the-runtime-with-up)). Where that is not possible, `up --in-cmux` runs the supervisor inside a cmux workspace instead, without launchd and without an automatic restart.

**6. Trust the repository in Claude Code once.** Claude's folder-trust prompt is decided by the repository root, not the worktree, so run `claude` in the repository root and accept it once; every run worktree then starts without it ([Trust prompt](docs/design/provider-lifecycle.md#trust-prompt)).

**7. Create the queue.** Each Git repository has one, resolved from wherever you run the binary inside it.

```sh
cd /path/to/your/repository
dagq init
```

`locate` shows the queue a directory resolves to without creating anything ([The queue and its commands](#the-queue-and-its-commands)).

**8. Start the runtime.**

```sh
dagq up --parallel 4
```

This keeps the supervisor resident as a launchd LaunchAgent and opens the maintainer's Claude Code session in the cmux workspace `[<repo>]dagq maintainer`. Running it again changes nothing ([Start the runtime with `up`](#start-the-runtime-with-up)).

**9. Tell the maintainer session what you want done.** It registers the goal and its tasks, makes them ready, watches the runs the supervisor starts, and lands them with `integrate` one at a time ([Run tasks with the supervisor](#run-tasks-with-the-supervisor), [Land a run on main](#land-a-run-on-main)).

## Upgrade

A new release is installed exactly like the first one: download the tarball and `SHA256SUMS`, check them with `shasum -a 256 -c SHA256SUMS`, and `install -m 755 dagq ~/.local/bin/dagq` over the old file. Then, from the repository:

```sh
dagq up
```

Nothing has to be stopped first. `up` reuses a live supervisor only while its `binary_version` matches its own, so once the file is replaced it drains the running supervisor — the agent is unloaded, the supervisor stops claiming and finishes the runs it holds — and starts one of the new binary in its place (`{"outcome": "restarted", ...}`). Use `up --no-wait` when you cannot sit through that drain: it refuses, changing nothing, whenever a run is in flight ([Updating the binary](#start-the-runtime-with-up)). A rebuild that does not bump the version reports the same `binary_version` and is reused rather than replaced, which matters only between releases.

Update the plugin with Claude Code:

```sh
claude plugin marketplace update dagq
claude plugin update claude-dagq@dagq
```

The two are versioned together. When a session resolves the binary (`dagq --resolve`, which the skills run first), the launcher compares the plugin's version with the binary's and, when they differ in major.minor, writes one `{"warning": ...}` line to stderr and carries on — stdout and the exit status are untouched, so the command still works. Read it as "one of these two is out of date": update whichever is older, the plugin with `claude plugin update` and the binary from the releases page. A session that sees the warning passes it on rather than stopping.

## Current status

The Rust/SQLite queue and a parallel supervisor are implemented. Tasks, dependencies, state transitions, candidate selection, run reservation, per-run supervisor leases, process heartbeats, and events are persisted locally. `supervise` is a resident loop: it claims dependency-ready tasks up to `--parallel N` (default 4), creates a Git worktree and a cmux workspace for each, starts an interactive Claude Code session through a wrapper, records each session's exit, validates each completion receipt against Git and the task's verification commands, and closes the workspace of an accepted run. `integrate` is the merge queue: it lands one validated run at a time on `main` by rebasing its worktree onto the current `main`, re-validating it, and squashing it into a single commit, and the supervisor picks up the tasks that unblocks. Claude Code's interactive lifecycle was [verified first](docs/journal/001-claude-lifecycle-spike.md).

A validated run stays in `awaiting_integration`; its workspace is closed, while its worktree and branch are kept until `integrate` lands it. A run whose rebase conflicts, or whose verification fails after the rebase, waits as `needs_session` for a resumed Claude session to fix it. One run's failure never touches another: each run has its own lease, and `doctor` / `recover` judge and release one run at a time. A supervisor that dies while a run's session is still alive does not lose the run: the next supervisor with a free slot adopts the stale lease and finishes the run; every other stale lease waits for `recover`.

## The queue and its commands

Each Git repository has one queue. Run the binary from anywhere inside the repository (any worktree, including a task worktree) and it resolves the queue to `$XDG_DATA_HOME/dagq/<hash>/queue.db`, by default `~/.local/share/dagq/<hash>/queue.db`, where `<hash>` is the first 16 hex digits of the SHA-256 of the repository's canonical Git common directory. `locate` prints that resolution without opening anything; `init` creates the directory and the queue.

The examples assume the installed `dagq` is on PATH ([Getting started](#getting-started)); a source build is `target/debug/dagq` ([Development](#development)).

```sh
dagq_demo=$(mktemp -d) && git -C "$dagq_demo" init -q -b main && cd "$dagq_demo"
export XDG_DATA_HOME="$dagq_demo/data"   # keep the demo out of ~/.local/share
dagq locate
dagq init
dagq add "Improve setup docs" \
  --description "Explain the local setup procedure" \
  --acceptance "A new contributor can follow the documented commands" \
  --verify "git diff --check"
dagq add "Check setup examples" --depends-on 1
dagq ready 1
dagq ready 2
dagq candidates
dagq show 2
```

The fresh queue assigns task IDs 1 and 2. Only task 1 is a candidate; task 2 remains blocked until task 1 is completed. Every shell inside the same repository sees the same queue. Only `init` creates a database. `--db PATH` before the subcommand uses an explicit queue file instead, for disposable repositories and tests; `init` creates its directory as well.

Commands return JSON on stdout. Runtime errors return JSON on stderr with a nonzero exit status; argument errors and help use the standard CLI format. Outside a repository, every command except `--db` usage fails with an error that says so.

| Command | Effect |
| --- | --- |
| `locate` | Show the queue this directory resolves to: `db`, `queue_dir`, `runs_dir`, `log_dir`, `label` and `launch_agent` (the supervisor's launchd label and plist path, whether or not `up` has written it), `source` (`repository` or `db_flag`), `git_common_dir`, `db_exists` |
| `init` | Create or migrate the queue and bind it to this repository; preserves existing tasks |
| `add TITLE [--description TEXT] [--acceptance TEXT] [--verify COMMAND] [--depends-on ID] [--goal ID] [--context TEXT]` | Register a draft; `--verify` and `--depends-on` can be repeated. `--goal` joins an open goal, `--context` records why the task exists and what to read first |
| `list` | One page of tasks as `{"tasks", "next", "total"}`: unfinished ones (not `completed` / `canceled`) newest first, at most 20, each with `id`, `status`, `title`, `goal_id`, `dependencies` and `latest_run` (`id` and `status` of its newest run, or null). `--status a,b` (any of them), `--all` (terminal ones too) and `--goal ID` filter with AND; `--limit N` sets the page size; `next` is null on the last page, otherwise pass it to `--before` for the next one; `total` counts every match. `--full` adds `description`, `acceptance`, `context`, `verification_commands`, `created_at`, `updated_at`. An unknown `--status` exits 1 |
| `show ID [--events N] [--full]` | Inspect one task: the task (with `goal_id` and `context`), dependencies, its latest run (`id`, `status`, `branch`, `result_commit`, `last_error`, `worktree_path`, `workspace_id`) with that run's processes, and its latest 10 events (`--events N`) with only `status` / `reason` / `last_error` / `from` / `to` of their payload. Long texts are cut to 300 characters ending in `…` and marked `truncated: true`. `--full` prints every run, event and process in full |
| `ready ID` / `draft ID` | Move between draft and ready; also allowed from `in_progress` once every run has failed or been interrupted |
| `cancel ID` | Cancel a draft, ready, or retryable in-progress task; does not satisfy its dependents |
| `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` | Change prerequisites of a draft or ready task |
| `candidates` | List dependency-ready tasks in registration order without reserving them |
| `graph [--goal ID]` | Show unfinished tasks' dependencies, how many tasks each releases (`unblocks`), candidates in the supervisor's claim order and the critical chain |
| `goal add TITLE [--description TEXT] [--acceptance TEXT] [--constraints TEXT] [--doc PATH]` | Register a goal: the higher-level problem a group of tasks solves. A goal has no state machine and no verification commands |
| `goal list` / `goal show ID` | `list` gives each goal's `closed`, `verdict` and task counts by status; `show` adds the goal's tasks (id, title, status) and the `kind` and `created_at` of its latest 10 events, with long texts cut like `show`; `goal show ID --full` prints the goal and every event in full |
| `goal edit ID [--title TEXT] [--description TEXT] [--acceptance TEXT] [--constraints TEXT] [--doc PATH]` | Replace fields (an empty `--doc` clears it); the old and new goal go into a `goal_updated` event. Runs already started keep their prompt snapshot |
| `goal close ID --verdict achieved\|abandoned` | Record the verdict once. `achieved` is refused while a task is neither completed nor canceled; `abandoned` while a task is in progress. A closed goal accepts no more tasks |
| `set-goal TASK GOAL` / `set-goal TASK --none` | Move a draft or ready task into an open goal, or out of its goal (same rule as dependency changes) |
| `up [--parallel N] [--in-cmux] [--no-wait] [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE]` | Start the queue's runtime, idempotently: prune supervisor registrations whose process is gone, keep the supervisor resident as a launchd LaunchAgent (started only when no live one of this binary's version is registered), and open the maintainer's Claude Code session in the cmux workspace `[<repo>]dagq maintainer` (skipped inside that session, reused if open). A live supervisor of another version is drained and replaced (`--no-wait` refuses that instead whenever a run is in flight). `--in-cmux` runs the supervisor in the cmux workspace `[<repo>]dagq supervisor` instead, with no launchd and no automatic restart. Reports what needs attention: unfinished runs, runs awaiting integration, runs that need a session |
| `down [--wait] [--force] [--cmux EXE]` | Stop the supervisor: unload its LaunchAgent so it drains its runs and is not restarted, or, for an in-cmux one, SIGINT it and close its workspace once it is gone. `--wait` blocks until it has deregistered or exited, `--force` kills it and drops its registration. The maintainer workspace stays open |
| `supervise [--parallel N] [--once] [--repo PATH] [--cmux EXE] [--claude EXE] [--log-dir DIR]` | Resident loop: claim dependency-ready tasks up to `N` (default 4), run each in its own cmux workspace, request exit once its receipt is in and Claude is idle, validate the receipt, close the workspace on success, and keep polling for new candidates (including tasks unblocked by `integrate`). `--once` exits when nothing is active or claimable. `--log-dir` adds one `supervisor-<started_at>-<pid>.log` per start. The checkout is the working directory unless `--repo` says otherwise; `up` runs it under launchd with the queue's `logs/` directory |
| `integrate ID` / `integrate --next` `[--repo PATH]` | Land a validated run on `main`: rebase its worktree onto the current `main`, re-validate, squash into one commit with `Dagq-Task` / `Dagq-Run` trailers, mark the task `completed`, remove the worktree. `--next` takes the oldest awaiting run; `ID` also resumes a `needs_session` run. Lands in the working directory's repository unless `--repo` says otherwise |
| `review ID` | Write the review material of the task's run awaiting integration or a session to `<run_dir>/review.md` (task, goal, receipt, `git log`, diffstat and the full `git diff <base>...<head>`) and print only its path, `base`, `head` and the diff's `files_changed` / `insertions` / `deletions` |
| `status` | List every registered supervisor (PID, liveness, heartbeat age, `mode` and `workspace_id`, `binary_version`, `parallel`, the runs it holds), any other lease holder such as an `integrate` process, and the unfinished runs with their worktree paths and leases |
| `doctor [--full]` | Report the same supervisors and every unfinished run without changing state, one line's worth each (`run_id`, `task_id`, `status`, `lease_stale`, `recoverable`, `blocker_count`, `workspace_id`, `worktree_path`). `--full` adds each run's lease, wrapper/agent processes, heartbeats, paths, and recovery blockers |
| `rebind [--repo PATH]` | Bind the queue to the repository containing the working directory (or `--repo`) after that repository moved: the only command that changes the binding, and the only one that accepts a repository-resolved queue bound elsewhere. Refused while a registered supervisor or an `integrate` is alive. Prints the previous and new `git_common_dir`, `move_to` (where the repository now resolves its queue, when that is not where this one is), and `worktrees` (each remaining run worktree, repaired with `git worktree repair`); a change is appended to `<queue dir>/logs/rebind.jsonl`. See [Move the repository or the queue](#move-the-repository-or-the-queue) |
| `recover RUN_ID` | Mark one unfinished run `interrupted` and drop its lease once its processes and supervisor are gone; keeps its worktree and workspace, and leaves other runs alone |

Verification commands are shell lines that the supervisor runs in the worktree (`/bin/sh -c`) after the session exits; the agent is asked to run them too. Task descriptions and acceptance criteria are optional during registration.

## Start the runtime with `up`

Three roles share a queue: the **supervisor** is the resident `supervise` process that runs tasks, the **maintainer** is the resident Claude Code session that registers, watches, reviews and lands them, and a **worker** is the Claude session of one run. `up` starts the first two from a shell whose PATH has `dagq`, `cmux` and `claude`:

```sh
dagq up --parallel 4 --plugin-dir /path/to/dagq/plugins/claude-dagq
```

`up` checks cmux (`ping`), Claude (`--version`) and the queue (`init` first), deletes supervisor registrations whose process is dead (`pruned_supervisors`), and then keeps one supervisor resident as a launchd LaunchAgent. Before it writes anything it proves that cmux will admit that supervisor (see the prerequisite below): it runs `cmux ping` the way the supervisor will run, outside cmux's process tree (orphaned, so launchd is its parent) and with the environment the agent will carry and none of the `CMUX_*` variables of the terminal it runs in, and fails with the remedies if cmux refuses. Then it writes `~/Library/LaunchAgents/com.dagq.<queue hash>.plist` (`locate` shows the path as `launch_agent`) whose `ProgramArguments` run this binary's `supervise --parallel N --log-dir <queue dir>/logs` from the repository root with your shell's PATH, `KeepAlive` and `RunAtLoad` so it restarts after any exit, and stdout/stderr in `<queue dir>/logs/launchd.log`; loads it with `launchctl bootstrap gui/<uid>`; and waits for the supervisor to register itself (`{"supervisor": {"outcome": "started", "mode": "launchd", "pid": ...}}`). A supervisor that is already registered, alive, heartbeating **and running this binary's own version** is reused and neither the plist nor launchd is touched (`"reused"`, with whichever `mode` started it, and its `version`); one of any other version is replaced instead (see below). Then it opens the maintainer session: a cmux workspace named `[<repo>]dagq maintainer` (`<repo>` is the repository directory's name) whose command starts `claude` with `DAGQ_ROLE=maintainer` and `DAGQ_QUEUE=<db>` in its environment, `--plugin-dir` if given, and a generated prompt that names the queue, the roles and the log directory and asks the session to report `status` and `doctor` through the plugin's `dagq-maintain` skill and wait for you. The workspace is `"reused"` when one with that title is open and `"skipped"` when `up` runs inside the maintainer session itself (the plugin skill calls it), so running `up` twice changes nothing the second time. The result ends with a `doctor` summary: unfinished runs (with `lease_stale`), runs `awaiting_integration`, and runs in `needs_session`.

**Prerequisite: cmux must accept a connection from outside its terminals.** cmux decides by process ancestry, not by environment: a client that descends from one of its terminals is admitted whatever its variables, and one under launchd is refused (`only processes started inside cmux can connect`, cmux 0.64.25 in its default `cmuxOnly` mode) unless a socket password admits it (`cmux --help`, Socket Auth: `--password`, then `CMUX_SOCKET_PASSWORD`, then the password saved in cmux's Settings, which its CLI uses on its own). Save a socket password in cmux's Settings once (`automation.socketControlMode: "password"` and `automation.socketPassword` in `~/.config/cmux/cmux.json`, then `cmux reload-config`), or export `CMUX_SOCKET_PASSWORD` in the shell that runs `up`; in that case, and only then, `up` stores it in the plist's `EnvironmentVariables` next to PATH (the plist is written mode 0600), so prefer the Settings-saved password. Without either, `up` stops before the plist exists: `cmux refused a connection from outside its own terminals ... save a socket password in cmux Settings ... or export CMUX_SOCKET_PASSWORD ...`, followed by cmux's own error. (A ping that cannot be run or does not answer within 60 seconds is reported as `cmux could not be asked ...` instead; that is not a password problem.) A supervisor that is already registered and alive (one you started by hand in a cmux terminal, below) is reused without this check. Where no socket password is configured, `up --in-cmux` is the fallback.

**Fallback: `up --in-cmux`.** `--in-cmux` skips launchd entirely and runs the supervisor inside a cmux workspace of its own, so it is a child of a cmux terminal like any other client and needs no socket password. Nothing about launchd is touched: no plist is written, `launchctl` is not called, and the out-of-cmux ping is not run.

```sh
dagq up --in-cmux --parallel 4 --plugin-dir /path/to/dagq/plugins/claude-dagq
```

`up` opens a workspace named `[<repo>]dagq supervisor` whose command is this binary's `supervise --parallel N --log-dir <queue dir>/logs`, waits for the supervisor to register itself, and reports `{"supervisor": {"outcome": "started", "mode": "in_cmux", "workspace_id": "...", "name": "[<repo>]dagq supervisor", "plist": null}}`. Everything else is the same as the launchd mode: dead registrations are pruned first, the maintainer workspace follows, and a live supervisor is reused.

**There is no automatic restart in this mode.** launchd's `KeepAlive` is what brings a launchd-mode supervisor back after a crash, a `down`-less exit or a reboot; an in-cmux supervisor has nothing watching it. If it stops — including when you quit cmux, which takes its workspace with it — no work is claimed until you run `up --in-cmux` again. cmux keeps a workspace open after its command exits, so a crashed supervisor leaves `[<repo>]dagq supervisor` behind with its output on screen; `up --in-cmux` will not open a second one and instead tells you to read that workspace and close it (`cmux workspace close <id>`). The same holds for a supervisor that is alive but no longer heartbeating, which `up` neither reuses nor kills.

`status` and `doctor` show which mode is running: each entry under `supervisors` carries `mode` (`launchd`, `in_cmux`, or `null` for one you started by hand) and, for `in_cmux`, the `workspace_id` it runs in. `up` records this on the registration when the supervisor it started comes up, so it lives and dies with that registration. Each entry also carries `binary_version`, the `dagq` version of the process itself, which the supervisor writes when it registers (`null` for a lease holder without a registration, or for a supervisor that registered before the column existed).

**Updating the binary: replace the file, then run `up`.** `up` reuses a live supervisor only while its `binary_version` matches its own. When it does not — you replaced `~/.local/bin/dagq`, or the supervisor predates the column — `up` drains that supervisor and starts one of its own version in its place, so the new binary is what serves the queue from then on ([ADR-0014](docs/adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)). The stop is exactly `down --wait`'s: the LaunchAgent is unloaded (its bootout carries the SIGTERM, and unloading also stops `KeepAlive` from restarting the old binary at once), an in-cmux supervisor gets SIGINT, a supervisor launchd did not signal gets SIGTERM, and `up` then waits — with no timeout — until every one of those registrations is gone, which is after they have stopped claiming and finished the runs they hold. The workspace of an in-cmux supervisor is closed once its drain is over, before a new one could need the same name. When the replacement will run under launchd, the out-of-cmux connection is proved *before* the drain and not asked again, so a cmux that refuses it stops `up` with the working supervisor still serving the queue. The result is `{"supervisor": {"outcome": "restarted", "version": "...", "previous_version": "..." | null, "replaced": [{"token", "pid", "mode", "workspace_id", "version"}], "supervisor_workspaces": [...], ...}}`. Which mode the replacement starts in is the mode you asked this `up` for, not the one being replaced.

```sh
dagq up --no-wait   # refuse rather than sit through a drain
```

`--no-wait` is for when you cannot wait: if any run is in flight (`claimed`, `starting`, `running`, `validating` or `integrating`) it stops with an error naming how many and which, and nothing is signalled, unloaded or opened — the old supervisor keeps serving the queue exactly as before. With no run in flight the replacement goes ahead, and the drain itself is bounded too (30 seconds): a supervisor can fail to stop even with nothing to finish — a loop wedged on a hung `cmux` or `git` call keeps its registration while its heartbeat thread runs on — so `up --no-wait` gives up with what is still registered rather than waiting forever. It has already asked that supervisor to stop and unloaded its agent by then, so run `up` again once `status` shows it gone. The flag does nothing when the versions already match.

Two limits are worth knowing. `up` only replaces supervisors it considers live — alive **and** heartbeating within 30 seconds. A supervisor that is alive but silent is one `up` neither reuses nor kills (that predates this and is deliberate), so an old-binary one in that state is not replaced either: `up` starts a new supervisor beside it and `status` shows the old row as `stale`, for you to stop with `down --force`. And `binary_version` is `CARGO_PKG_VERSION`, so a rebuild that does not bump the version reports the same string and is reused: bump the version, or use `down --wait`, when you swap in a build from between releases.

```sh
dagq down          # unload the agent; the supervisor drains and exits
dagq down --wait   # ... and wait until it has deregistered
dagq down --force  # ... then SIGKILL it and drop its registration
```

`down` unloads the LaunchAgent (`launchctl bootout`) and removes the plist, so the supervisor receives SIGTERM, stops claiming, waits for its active runs, deregisters and exits without being restarted or coming back at the next login; a signal alone would only make launchd restart it. It unloads the agent whatever mode is running, which also clears one left behind by a queue that has since moved to `--in-cmux`. A supervisor started by hand (one that is not the agent's process) gets the SIGTERM from `down` itself. An in-cmux supervisor has no service manager to signal it, so `down` sends it SIGINT — the signal Ctrl-C in its terminal would send, which the runtime drains on exactly like SIGTERM — and closes its workspace whenever it has seen the stop through: after the drain with `--wait`, after the kill with `--force`, and when nothing was running to begin with. It does not consult the PID there, because a PID outlives the stop: `kill(2)` returns before the target is reaped, so a `kill -0` right after SIGKILL still succeeds, and a supervisor that has drained removes its registration before it exits. The workspace is never closed while the supervisor may still be draining, because that would cut the drain short: the default `down` returns immediately and reports its workspace as `left_open` with `down --wait` as the remedy. The `supervisor_workspaces` field lists what happened to each one (`closed`, `left_open`, or `close_failed` with cmux's message). `--force` also drops the registrations of supervisors that were already dead, so no row is left pointing at a workspace the same call just closed. Without a live registration `down` reports `not_running` (still unloading a lingering agent, and with `--force` also dropping dead registrations). It never closes the maintainer workspace. Each supervisor start writes its own `supervisor-<started_at>-<pid>.log` under `<queue dir>/logs` with its token, PID, `--parallel`, queue and repository, the progress messages that also go to stderr, and its final result.

## Run tasks with the supervisor

`up` is the normal way to start the supervisor. You can also run it yourself in a dedicated terminal inside the repository, with cmux and an authenticated Claude Code on PATH (or `--cmux` / `--claude`); it stays resident and runs dependency-ready tasks as they appear, up to `--parallel` at once.

```sh
dagq supervise --parallel 4
```

The supervisor registers itself in the queue when it starts (its PID, `--parallel`, and start time), refreshes that registration with its leases every 2 seconds, and removes it whenever it exits: after Ctrl-C drained its runs, after `--once` ran out of work, after a provisioning failure drained them, or on an error such as an unreadable `main`. Only a heartbeat failure keeps the row, since the database may be unreachable. `status` and `doctor` list it under `supervisors` from its first second, holding runs or not, so you can tell whether a resident loop exists before starting another. A supervisor that was killed or hangs leaves its row behind: it is shown with `alive: false` or a growing `heartbeat_age_secs` and `stale: true`; the next `up` removes the rows whose process is dead and reports them as `pruned_supervisors`, and nothing else removes them.

Its runs are not lost. Before claiming, whenever it has a free slot, a supervisor looks for `running` or `validating` runs whose lease belongs to another supervisor and is stale (its PID is dead, or its heartbeat is older than 30 seconds) while the run's session wrapper is still alive and heartbeating, or has already recorded its exit. It adopts such a run in one transaction: the lease and the run's `supervisor_token` move to the adopter and a `run_adopted` event records the previous token and PID, how old the heartbeat was, and the wrapper's state. The adopter then watches the run like one it claimed, without repeating an `/exit` that was already requested, and restarts validation for a `validating` run. So after `up` replaces a killed supervisor (a binary update, a `down --force`), the runs in flight simply continue under the new one. Nothing else is adopted: a `claimed` or `starting` run, a run whose wrapper is dead or silent, a run without a lease (given up by its supervisor) and an `integrating` run are left for `recover`. Two supervisors that find the same stale lease adopt it exactly once, and a supervisor whose lease was taken from it stops watching that run without writing anything more about it.

Every few seconds the supervisor looks for candidates and claims them until `--parallel` runs (default 4) are active. Each claim reads `refs/heads/main` again and uses it as the run's base commit, whichever worktree you start from, so a task released by `integrate` starts from the `main` that contains its predecessor's landing; pass `--repo PATH` to use a checkout other than the working directory. Each run gets its own lease (`lease_acquired` / `lease_released` events), a cmux workspace named `[<repo>]dagq#<task-id> <task title>` whose description is `run <run-id>` (ADR-0018), and a worktree, and goes through the same state machine independently of the others. Runtime files live next to the database in `runs/<run-id>/` (see `locate`'s `runs_dir`); their paths are resolved from the run ID and the queue's current directory on every read, so a queue directory moved as a whole (after `down --wait`) keeps its unfinished and awaiting runs (`integrate` runs `git worktree repair` on the run's worktree; do not `git worktree prune` before landing them, see ADR-0017): the prompt, a snapshot of the runtime binary, the worktree on branch `dagq/<run-id>`, Claude's per-run settings and debug log, the idle marker, the receipt, and the final terminal screen. The prompt also names the task's direct predecessors (each with the commit `integrate` landed and its receipt summary) and the other tasks in progress at claim time, so a dependent run knows what it builds on and what runs beside it. The workspace command starts a hidden `session` wrapper that launches Claude with the run ID as its session ID and reports heartbeats and the exit code.

The loop ends in three ways. Ctrl-C (or SIGTERM) once stops claiming and waits for the active runs to finish; a second Ctrl-C terminates the process immediately and its leases go stale after 30 seconds. `--once` exits as soon as no run is active and no task is claimable, which suits a single batch or a test. A provisioning failure (worktree or workspace creation) is treated as an environment problem: the supervisor gives that run up, stops claiming, waits for its other runs, and exits with an error. The final JSON lists the runs that came to rest under `runs` and the runs it gave up under `errors`.

Claude stops at its folder-trust prompt in a run's workspace only when the repository itself has never been trusted: the prompt is decided by the repository root, not by the worktree path, so run `claude` once in the repository root and accept the prompt before the first `supervise` on a repository, and every run worktree starts without it (see [Trust prompt](docs/design/provider-lifecycle.md#trust-prompt)). Permission prompts can still appear in each workspace; answer them there. A receipt does not end the session. Claude is started with a per-run `--settings` file whose `Stop` hook writes `<run-dir>/idle.json` each time a response finishes; once that marker is newer than the receipt, the supervisor records `session_idle_observed` and sends `/exit` to the workspace once (`exit_requested`). You can still send `/exit` yourself at any time, and you must if the hook is disabled or Claude is waiting on a prompt. If the session has not exited 120 seconds after the request, the supervisor records `exit_request_timed_out` once and keeps the run and its lease: it does not resend `/exit` (a dialog may be holding it back), `status` shows the attention `send /exit`, and once you deal with the dialog and send `/exit`, the run is validated as usual while the other runs continue. A nonzero exit code marks the run `failed` with `session exited with code N` in `last_error`. With exit code 0 the supervisor validates the receipt: it must name this run, report `succeeded`, give evidence for passed checks and a reason for `not_applicable` ones, and its commit must be the clean head of the run branch on top of the base commit. The supervisor then reruns the task's verification commands in the worktree, logging each to `<run-dir>/verify-N.log`. A run that passes becomes `awaiting_integration` with its `result_commit` recorded; anything else becomes `failed` with the reason in `last_error`. Only an accepted run has its cmux workspace closed (`workspace_closed_at` is set once cmux confirms); its worktree and branch stay until integration. If the close fails, the run stays `awaiting_integration` with a `cleanup_failed` event and the error in `last_error`, and `workspace_closed_at` stays null so the workspace is not treated as cleaned. A failed run keeps its workspace, worktree, and branch. A run that reached `awaiting_integration` or `failed` releases its lease.

If something goes wrong around a run rather than in it (its wrapper stops heartbeating, validation itself errors, or the close cannot be recorded), the supervisor gives that one run up: it records the cause in `last_error` and a `runtime_error` event, deletes the run's lease, and leaves its status, processes, workspace, and worktree untouched, then keeps serving the other runs. Such a run shows up in `doctor` without a lease. Nothing is rerun automatically.

A run's `last_error` (in `show` — cut to 300 characters unless `--full` —, `doctor --full`, and the supervisor's final `errors`) holds only the latest of these messages; `integrate` clears it when the run lands, and the full history is in the run's events. A nonzero session exit writes `session exited with code N` (128 for a session killed by a signal) as the run becomes `failed`. A rejected receipt writes the first failed check verbatim as the run becomes `failed`: `receipt was not submitted at <path>`, the parse or `run_id` error for a malformed receipt, `no commit was made on top of base <sha>`, `worktree is on <ref> instead of refs/heads/<branch>`, `receipt commit <sha> is not the head of <branch> (<head>)`, `commit <sha> does not descend from base <sha>`, `worktree is not clean:` followed by the `git status` output, or `verification command "<cmd>" exited with <code>; see <run-dir>/verify-N.log`. A runtime or provisioning error writes the error text as-is next to a `runtime_error` event without changing the status: `run <run-id> provisioning failed: ...` when the worktree or workspace could not be created, `wrapper heartbeat expired; session may still be alive`, or the Git or database error from validation itself. A cleanup failure writes `workspace <workspace-id> could not be closed: ...` (the run stays `awaiting_integration`) or, after landing, `landed worktree <path> could not be removed: ...` next to a `cleanup_failed` event. `integrate` writes it too: `needs_session` carries the rebase or re-validation reason, an error before `main` moves puts the run back with `integration stopped before main moved: ...`, and a rewritten `failed` receipt marks the run `failed` with the receipt's reason (see [Land a run on main](#land-a-run-on-main)). Read it together with the status and the last event: `failed` means a nonzero exit, a rejected receipt, or a failed receipt at integration; `claimed`, `starting`, `running`, or `validating` with a `last_error` means the supervisor gave the run up; `awaiting_integration` with a `last_error` means the close failed (`cleanup_failed`, `workspace_closed_at` null) or an `integrate` attempt stopped before `main` moved (`integration_error`); `needs_session` means `integrate` parked it.

## Recover an interrupted run

A run is orphaned when its supervisor gave it up, or was killed or lost its heartbeat while no supervisor with a free slot could adopt it (its session wrapper was dead or silent too, or it had not reached `running`). The task stays `in_progress` and the run keeps its resources; other runs, and a supervisor still running them, are unaffected. Inspect first:

```sh
dagq doctor --full
```

`doctor --full` lists the supervisors under `supervisors`: every registered `supervise` process (`registered: true`, with `pid`, `alive` from `kill -0`, `parallel`, `started_at`, `heartbeat_at`, `heartbeat_age_secs`, the `run_ids` it holds, and `stale` when the PID is dead or the heartbeat is older than 30 seconds) and any other process that holds a lease without a registration, such as a running `integrate` (`registered: false`). A stale registration is reported, never deleted; `recover` and `integrate` do not touch it. Then every run in `claimed`, `starting`, `running`, `validating`, or `integrating` with its workspace ID, whether its worktree and run directory exist, its own lease (or `null`), and each registered wrapper/agent process with its PID, liveness (`kill -0`), and heartbeat age. `blockers` names what would stop a recovery of that run, considering only its own lease and processes; `recoverable` is true when the list is empty. Stop the listed processes yourself, for example by exiting the session in its workspace.

```sh
dagq recover <run id>
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

Review the run branch `dagq/<run-id>`: `dagq review 1` writes the task, the receipt, the commits and the full diff to `review.md` in the run directory and prints only its path and the diff's size, so a maintainer session hands the file to a subagent instead of reading the diff itself. The `verify-N.log` files are in the same directory. Do not merge it yourself; landing is the runtime's job, one run at a time:

```sh
dagq integrate 1        # the task's run
dagq integrate --next   # the oldest run awaiting integration
```

`integrate` marks the run `integrating` (holding the queue's single integration slot with a lease of its own), rebases the run's worktree onto the current `refs/heads/main`, and re-validates it: the receipt must name the worktree's head and report `succeeded`, the rebased head must sit on `main` with a clean tree, and the task's verification commands are rerun (`<run-dir>/integrate-verify-N.log`). The rerun is skipped when the rebase was a no-op on the head the supervisor already validated, since that commit and tree passed the same commands during validation; the landing records an `integration_verification_skipped` event and reports `"verification_skipped": true`. It then squashes the rebased tree into **one** commit on top of `main` whose message is the task title, the receipt's summary, and the trailers `Dagq-Task: <id>` and `Dagq-Run: <run-id>`. Where `main` is checked out the commit is fast-forwarded through that checkout so its files move too; otherwise the ref is updated directly. No merge commit and no fast-forward of the run branch: `main` stays a straight line with one commit per task, and the landed tree equals the validated worktree's tree. The run becomes `integrated` with the landed commit as its `result_commit`, the task becomes `completed` (`run_integrated` and `task_status_changed` events), the run's own history is kept under `refs/dagq/runs/<run-id>`, and the worktree and branch are removed. Dependent tasks then appear in `candidates`; a running supervisor claims them on its next poll with the landed `main` as their base. `--next` lands runs in the order their validation finished and prints `{"outcome": "no_run_awaiting"}` when none is left.

If the rebase conflicts, `integrate` aborts it, leaves the worktree on its validated head, and parks the run as `needs_session` with the conflicting files in `last_error` (`{"outcome": "needs_session", "main": ..., "reason": ...}`). If the rebase applies but a verification command fails on the result, the run is parked the same way with the rebased tree left in place. Nothing reaches `main`. Reopen a Claude session in that worktree (`claude --resume <run-id>` in a cmux workspace whose `--cwd` is the worktree) and have it rebase onto `main` (`git rebase <main commit>` from the reason), resolve, rerun the verification commands, and rewrite `receipt.json` with the new head commit; if the change is no longer needed, have it write `"result": "failed"` with the reason in `summary`. Exit that session with `/exit` before running `integrate ID`: landing removes the worktree, so a session still working in it must not be running. Then run `integrate ID` again: it repeats the same steps from the rebase (a no-op unless `main` moved again) and lands the run, keeps it `needs_session` with a new reason if the receipt does not name the current head, or marks it `failed` (`{"outcome": "failed", ...}`) on a failed receipt so the task can be retried with `ready ID` or dropped with `cancel ID`. `--next` never picks a `needs_session` run; it is resumed explicitly. A parked or awaiting run keeps its task `in_progress`. The runtime never closes the cmux workspace of a failed or interrupted run: close it with `cmux workspace close <workspace_id>` (the ID is the run's `workspace_id` in `show ID`; `doctor` does not list finished runs) once you no longer need it.

An error before `main` moves (a missing worktree, a `main` checkout with local changes that collide with the landing, a Git failure) puts the run back where it was with the message in `last_error` and an `integration_error` event; fix the cause and run `integrate` again. An `integrate` process that dies leaves its run `integrating` with a stale lease: `doctor` lists it, and `recover RUN_ID` returns it to `awaiting_integration` once the process is gone. A task with no run awaiting integration or a session is an error, so a task cannot be landed twice.

The landing happens in the working directory's repository; pass `--repo PATH` to name another checkout, for example when using `--db` from elsewhere. Either way it must be the repository the queue is bound to: a queue resolved from the working directory is bound by `init`, a `--db` queue by its first `supervise`, and a mismatch is an error. After landing, `integrate` pushes `main` to `origin` (`push` in its output: `pushed`, `skipped` when there is no `origin` or with `--no-push`, or `failed`). A failed push keeps the landing, is recorded as `push_failed` and shows in `status` as the attention `push main`; fix the cause and run `git push origin main`.

## Move the repository or the queue

The queue directory can be moved as a whole on its own: stop the supervisor with `down --wait`, land or resolve `needs_session` runs, move `queue.db` with its WAL files, `runs/`, `logs/` and `repository` together, and point `--db` (or `DAGQ_DB`) at the new place; run paths are resolved from where the queue is opened, and `integrate` repairs each worktree's Git record (ADR-0017). Do not `git worktree prune` before those runs have landed.

Moving the repository (a directory move, or a GitHub rename that moves a ghq checkout) changes its Git common directory, so the checkout resolves to a new, empty queue location, and the old queue is still bound to the old path: once it sits where the new checkout resolves, every command fails with `queue is bound to another Git repository`, and through `--db` `supervise` and `integrate` do; `init` does not rebind it. `rebind` does, explicitly (ADR-0020). In this order:

1. In the old checkout, `dagq down --wait`: the supervisor drains its runs and its LaunchAgent (named after the old queue hash) is removed. Resolve and land `needs_session` runs; runs awaiting integration survive the move.
2. Move the repository. Do not run `init` in the new checkout: it would create an empty queue where the old one has to go.
3. From the new checkout, rebind the old queue in place: `dagq --db <old queue dir>/queue.db rebind` (with the plugin launcher, `DAGQ_DB=<old queue dir>/queue.db`). It prints `previous_git_common_dir` and `git_common_dir`, repairs the Git link of each run worktree still on disk, and names the new location as `move_to`. A running supervisor or `integrate` makes it fail before anything changed.
4. Move the old queue directory to `move_to` as a whole, e.g. `mv <old queue dir> <move_to>`. `move_to` must not exist yet; if it does (an `init` ran in the new checkout), check that it holds no tasks and remove it first, or `mv` puts the old queue inside it.
5. From the new checkout, `dagq list` and `dagq status` work without flags; start the runtime again with `up`.

If the repository was already moved without step 1, the old checkout is gone: stop an in-cmux supervisor with `dagq --db <old queue dir>/queue.db down --wait`, but the LaunchAgent is named after the old repository's hash, not the `--db` path, so remove it by hand with `launchctl bootout gui/$(id -u)/com.dagq.<old hash>` and delete its plist under `~/Library/LaunchAgents` (the old hash is the old queue directory's name).

The reverse order also works: move the queue directory to `dagq locate`'s `queue_dir` first, then run `dagq rebind` without `--db` (every other command, `init` included, refuses the queue until then). Rebinding first is preferred because the step that can be refused happens before anything was moved, and `move_to` says where to go. `rebind` from the repository the queue is already bound to reports `unchanged`. The database is never edited by hand for any of this.

## Use from Claude Code

`plugins/claude-dagq` is a Claude Code plugin whose skills drive the binary, plus one `SessionStart` hook that prints `status` into a maintainer session after compaction or `/clear`; it never opens the queue database itself. This repository is also its marketplace (`.claude-plugin/marketplace.json`), so installing it takes two commands:

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

The plugin does not carry the binary: install that separately from a [release](https://github.com/hisamekms/dagq/releases) into `~/.local/bin` ([Getting started](#getting-started)), or build it here. To work on the plugin itself, load it from the checkout for a session instead of installing it:

```sh
cargo build --locked
export DAGQ_BIN="$PWD/target/debug/dagq"   # or put it on PATH
claude --plugin-dir "$PWD/plugins/claude-dagq"
```

The queue is the one of the repository you run Claude Code in, resolved by the binary as described above and shared by all of its worktrees; set `DAGQ_DB=/path/to/queue.db` to use another file, which the launcher passes as `--db`. The plugin's launcher `bin/dagq` resolves the binary and forwards any command from the current directory (`bin/dagq --resolve` shows the binary, its `binary_version`, the plugin's `plugin_version`, and `locate`'s output; when the two versions differ in major.minor it also writes a `{"warning": ...}` to stderr and still exits 0).

| Skill | Covers |
| --- | --- |
| `/claude-dagq:dagq` | Locate the binary and queue, `init`, register a goal with `goal add` and decompose it into tasks with `add --goal` (description, acceptance, `--verify`, `--depends-on`, `--context`), `ready`, `list` / `show` / `candidates` / `locate` / `status` / `doctor`, how to read run states, close a goal |
| `/claude-dagq:dagq-maintain` | The maintainer's loop: start the runtime with `up`, read `status` (supervisor health, attention, cursor), run `watch --after <cursor>` in the background and report each attention, stop the runtime with `down`; landing goes through `dagq-land`, which lands on a passing review and asks the user only on doubt |
| `/claude-dagq:dagq-land` | Write `review.md` with `review ID`, have a subagent review it and return only a verdict, then `integrate` when it passes, asking the user only on doubt (`integrate` pushes `main` and registers a receipt's `follow_ups` as draft tasks), asking the user which drafts become ready |
| `/claude-dagq:dagq-session` | Act on a run's session: answer its trust or permission prompt, send `/exit` after `exit_request_timed_out`, close the workspace of a failed run, resume a `needs_session` run |
| `/claude-dagq:dagq-recover` | `doctor`, `recover RUN_ID` for one run without disturbing the others, retry with `ready` |

Claude picks the skill from the request ("queue a task to …", "start the runtime", "did task 3 finish?", "the supervisor died"). Each skill keeps its field lists and state tables in its own `reference/` directory, read only when needed. `up` opens the maintainer session with this plugin loaded when it is given `--plugin-dir`. The plugin's `SessionStart` hook (on `compact` and `clear`) prints `dagq status` into a session started with `DAGQ_ROLE=maintainer`, so the maintainer re-orients itself after compaction or `/clear`; every other session gets no output. `claude plugin validate plugins/claude-dagq` checks the manifest and skills; `tests/plugin.rs` checks them and the launcher in `cargo test`.

## Development

Building from source is for working on dagq itself; to use it, install the released binary ([Getting started](#getting-started)). Requires Rust 1.93 or newer and a C compiler for bundled SQLite; no separate SQLite installation is needed.

```sh
cargo build --locked   # target/debug/dagq
```

Every change runs these four:

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

GitHub Actions runs the same four commands on a macOS runner for every push to `main` and every pull request ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

The tests use temporary databases and repositories, point `XDG_DATA_HOME` at temporary directories, and do not require cmux, Claude Code, or network access after dependencies have been fetched. Line coverage must stay at or above 80% (`cargo install cargo-llvm-cov`). The end-to-end paths in `tests/e2e.rs` drive the real binary through cmux with a stub agent, resolving the queue from the disposable repository's working directory, and are ignored by default; run them with `cargo test --locked --test e2e -- --ignored` where cmux is available. They are: one task landed by `integrate`; two tasks in parallel followed by a dependent one, with a conflicting run parked as `needs_session` and landed after the test resolves it; one that kills a `supervise` process while its stub worker runs and checks that the next `supervise --once` adopts and lands the run; and `up` → `status` → `down --wait` once per supervisor mode, with the plist under a disposable `HOME`. The launchd `up` / `down` test needs cmux to accept the launchd-run supervisor's connection (a socket password in cmux's Settings); the `up --in-cmux` one does not, and asserts that no plist is written and that launchd has no agent for the queue. `tests/lifecycle.rs` covers `up` and `down` in both modes against fakes for launchd, cmux and process signals, including the version-mismatch replacement (drain and restart in each mode, the `--no-wait` refusal with a run in flight, and the reuse of a supervisor of this binary's own version).

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Active plan](docs/plans/current.md)
- [Task journals](docs/journal/README.md)
- [Architecture decisions](docs/adr/README.md)
- [Agent instructions](AGENTS.md)

## License

dagq is released under the [MIT License](LICENSE); see that file for the full text.
