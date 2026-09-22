# cmux-taskq

cmux-taskq is a Rust task orchestrator for running dependency-aware development tasks in cmux workspaces and isolated Git worktrees.

The runtime is distributed as a binary. Claude Code and Codex integrations are distributed as plugins that invoke the binary.

## Current status

The Rust/SQLite queue and a single-run supervisor are implemented. Tasks, dependencies, state transitions, candidate selection, run reservation, supervisor leases, process heartbeats, and events are persisted locally. `supervise` claims one ready task, creates a Git worktree and a cmux workspace, starts an interactive Claude Code session through a wrapper, records the session exit, validates the completion receipt against Git and the task's verification commands, and closes the workspace of an accepted run. `integrate` confirms that the validated commit was merged into `main` and completes the task. Claude Code's interactive lifecycle was [verified first](docs/journal/001-claude-lifecycle-spike.md).

A validated run stays in `awaiting_integration`; its workspace is closed, while its worktree and branch are kept until the result is merged into `main` by hand and confirmed with `integrate`. Stale supervisor leases are never taken over automatically: `doctor` reports an interrupted supervisor and `recover` releases its run once nothing is left running.

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
| `locate` | Show the queue this directory resolves to: `db`, `queue_dir`, `runs_dir`, `source` (`repository` or `db_flag`), `git_common_dir`, `db_exists` |
| `init` | Create or migrate the queue and bind it to this repository; preserves existing tasks |
| `add TITLE [--description TEXT] [--acceptance TEXT] [--verify COMMAND] [--depends-on ID]` | Register a draft; `--verify` and `--depends-on` can be repeated |
| `list` / `show ID` | Inspect tasks; `show` includes dependencies, runs and events |
| `ready ID` / `draft ID` | Move between draft and ready; also allowed from `in_progress` once every run has failed or been interrupted |
| `cancel ID` | Cancel a draft, ready, or retryable in-progress task; does not satisfy its dependents |
| `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` | Change prerequisites of a draft or ready task |
| `candidates` | List dependency-ready tasks in registration order without reserving them |
| `supervise [--repo PATH] [--cmux EXE] [--claude EXE]` | Claim one task, run it in a cmux workspace, request exit once the receipt is in and Claude is idle, validate the receipt, and close the workspace on success. The checkout is the working directory unless `--repo` says otherwise |
| `integrate ID [--repo PATH]` | Confirm that the task's awaiting run was merged into `main` (merge or fast-forward) and mark the task `completed`; checks the working directory's repository unless `--repo` says otherwise |
| `status` | Show the supervisor lease and whether its heartbeat is stale |
| `doctor` | Report the lease, unfinished runs, their wrapper/agent processes, heartbeats, and paths without changing state |
| `recover RUN_ID` | Mark an unfinished run `interrupted` and drop the stale lease once its processes and supervisor are gone; keeps its worktree and workspace |

Verification commands are shell lines that the supervisor runs in the worktree (`/bin/sh -c`) after the session exits; the agent is asked to run them too. Task descriptions and acceptance criteria are optional during registration.

## Run one task with the supervisor

Requires cmux and an authenticated Claude Code on PATH (or pass `--cmux` / `--claude`). Run the supervisor in a dedicated terminal inside the repository; it processes one task and exits.

```sh
cmux-taskq supervise
```

The base commit is the repository's `refs/heads/main`, whichever worktree you start from; pass `--repo PATH` to use a checkout other than the working directory. Runtime files live next to the database in `runs/<run-id>/` (see `locate`'s `runs_dir`): the prompt, a snapshot of the runtime binary, the worktree on branch `taskq/<run-id>`, Claude's per-run settings and debug log, the idle marker, the receipt, and the final terminal screen. The workspace command starts a hidden `session` wrapper that launches Claude with the run ID as its session ID and reports heartbeats and the exit code.

Claude may wait for trust or permission prompts in the workspace; answer them there. A receipt does not end the session. Claude is started with a per-run `--settings` file whose `Stop` hook writes `<run-dir>/idle.json` each time a response finishes; once that marker is newer than the receipt, the supervisor records `session_idle_observed` and sends `/exit` to the workspace once (`exit_requested`). You can still send `/exit` yourself at any time, and you must if the hook is disabled or Claude is waiting on a prompt. If the session has not exited 120 seconds after the request, the supervisor records `exit_request_timed_out` and stops with an error, leaving the run `running` with its lease, workspace, and worktree intact for you to finish by hand. A nonzero exit code marks the run `failed`. With exit code 0 the supervisor validates the receipt: it must name this run, report `succeeded`, give evidence for passed checks and a reason for `not_applicable` ones, and its commit must be the clean head of the run branch on top of the base commit. The supervisor then reruns the task's verification commands in the worktree, logging each to `<run-dir>/verify-N.log`. A run that passes becomes `awaiting_integration` with its `result_commit` recorded; anything else becomes `failed` with the reason in `last_error`. Only an accepted run has its cmux workspace closed (`workspace_closed_at` is set once cmux confirms); its worktree and branch stay until integration. If the close fails, the run stays `awaiting_integration` with a `cleanup_failed` event and the error in `last_error`, and `workspace_closed_at` stays null so the workspace is not treated as cleaned. A failed run keeps its workspace, worktree, and branch. Then the lease is released. If provisioning or validation itself errors, the run, its lease, and any created resources are kept for inspection; check `show ID` and `status`.

## Recover an interrupted run

If the supervisor is killed or loses its heartbeat, the run keeps the execution slot and its lease, and `supervise` refuses to start. Nothing is rerun automatically. Inspect first:

```sh
cmux-taskq doctor
```

`doctor` lists the lease (PID, whether it is alive, heartbeat age, stale after 30 seconds) and every run in `claimed`, `starting`, `running`, or `validating` with its workspace ID, whether its worktree and run directory exist, and each registered wrapper/agent process with its PID, liveness (`kill -0`), and heartbeat age. `blockers` names what would stop a recovery; `recoverable` is true when the list is empty. Stop the listed processes yourself, for example by exiting the session in its workspace.

```sh
cmux-taskq recover <run id>
```

`recover` refuses while any process registered for the run is still alive, the lease heartbeat is fresh, or the supervisor PID is alive. Otherwise it marks the run `interrupted`, records a `run_recovered` event with the state it checked, and deletes the lease. The worktree, branch, and workspace are kept for inspection, and the task stays `in_progress`. To retry, make the task ready again with `ready ID` (or `draft ID` to edit it first); the next `supervise` creates a new run with its own worktree. The same applies to a task whose last run `failed`.

The receipt is JSON at `<run-dir>/receipt.json`, written by atomic rename:

```json
{"run_id": "<run id>", "result": "succeeded", "commit": "<full SHA>",
 "tests": {"status": "passed", "evidence_or_reason": "cargo test: 15 passed"},
 "e2e": {"status": "not_applicable", "evidence_or_reason": "library change"},
 "subagent_review": {"status": "passed", "evidence_or_reason": "no findings"},
 "summary": "..."}
```

`status` is `passed`, `failed`, or `not_applicable`; `result` is `succeeded` or `failed`. The receipt's claims never make a run succeed on their own.

## Complete a task after merging

Review the run branch `taskq/<run-id>` and merge it into `main` yourself (merge commit or fast-forward). Then confirm it:

```sh
cmux-taskq integrate 1
```

`integrate` checks with Git that the run's `result_commit` is an ancestor of `refs/heads/main`. If it is, the run becomes `integrated`, the task becomes `completed`, `run_integrated` and `task_status_changed` events are recorded, and tasks depending on it can appear in `candidates`. If it is not, the command prints `{"outcome": "not_integrated", ...}` with the current `main` commit and changes nothing; a squash merge or cherry-pick produces a different commit and is therefore not recognized. A task with no run awaiting integration is an error, so a task cannot be integrated twice.

The check runs against the working directory's repository; pass `--repo PATH` to name another checkout, for example when using `--db` from elsewhere. Either way it must be the repository the queue is bound to: a queue resolved from the working directory is bound by `init`, a `--db` queue by its first `supervise`, and a mismatch is an error. The worktree and branch are left for you to remove.

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
| `/claude-taskq:taskq` | Locate the binary and queue, `init`, register with `add` (description, acceptance, `--verify`, `--depends-on`), `ready`, `list` / `show` / `candidates` / `locate` / `status` / `doctor`, how to read run states |
| `/claude-taskq:taskq-run` | Launch `supervise` in a dedicated cmux workspace whose `--cwd` is the repository, watch the run with `show`, judge completion from the run state and receipt rather than a Stop hook, review and `integrate` after the manual merge |
| `/claude-taskq:taskq-recover` | `doctor`, `recover RUN_ID`, retry with `ready` |

Claude picks the skill from the request ("queue a task to …", "did task 3 finish?", "the supervisor died"). `claude plugin validate plugins/claude-taskq` checks the manifest and skills; `tests/plugin.rs` checks them and the launcher in `cargo test`.

## Development checks

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

The tests use temporary databases and repositories, point `XDG_DATA_HOME` at temporary directories, and do not require cmux, Claude Code, or network access after dependencies have been fetched. Line coverage must stay at or above 80% (`cargo install cargo-llvm-cov`). The end-to-end happy path in `tests/e2e.rs` drives the real binary through cmux with a stub agent, resolving the queue from the disposable repository's working directory, and is ignored by default; run it with `cargo test --locked --test e2e -- --ignored` where cmux is available.

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Active plan](docs/plans/current.md)
- [Task journals](docs/journal/README.md)
- [Architecture decisions](docs/adr/README.md)
- [Agent instructions](AGENTS.md)
