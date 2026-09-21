# cmux-taskq

cmux-taskq is a Rust task orchestrator for running dependency-aware development tasks in cmux workspaces and isolated Git worktrees.

The runtime is distributed as a binary. Claude Code and Codex integrations are distributed as plugins that invoke the binary.

## Current status

The Rust/SQLite queue and a single-run supervisor are implemented. Tasks, dependencies, state transitions, candidate selection, run reservation, supervisor leases, process heartbeats, and events are persisted locally. `supervise` claims one ready task, creates a Git worktree and a cmux workspace, starts an interactive Claude Code session through a wrapper, and records the session exit. Claude Code's interactive lifecycle was [verified first](docs/plans/claude-lifecycle-spike.md).

Receipt validation, workspace cleanup, and `completed` transitions are the next steps. A run that exits with code 0 stays in `validating`; its workspace, worktree, and branch are kept. There is no manual `complete` command, and stale supervisor leases are never taken over automatically.

## Build and try the queue

Requires Rust 1.93 or newer and a C compiler for bundled SQLite. No separate SQLite installation is required.

```sh
cargo build --locked
taskq_demo_dir=$(mktemp -d)
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" init
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" add "Improve setup docs" \
  --description "Explain the local setup procedure" \
  --acceptance "A new contributor can follow the documented commands" \
  --verify "git diff --check"
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" add "Check setup examples" --depends-on 1
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" ready 1
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" ready 2
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" candidates
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" show 2
```

The fresh queue assigns task IDs 1 and 2. Only task 1 is a candidate; task 2 remains blocked until task 1 is completed. Reuse the same `--db` path in subsequent shells. Only `init` creates a database; the parent directory must already exist. Store real queue files outside worktrees, such as under the repository's Git common directory.

Commands return JSON on stdout. Runtime errors return JSON on stderr with a nonzero exit status; argument errors and help use the standard CLI format. `--db` precedes the subcommand.

| Command | Effect |
| --- | --- |
| `init` | Create or migrate the queue; preserves existing tasks |
| `add TITLE [--description TEXT] [--acceptance TEXT] [--verify COMMAND] [--depends-on ID]` | Register a draft; `--verify` and `--depends-on` can be repeated |
| `list` / `show ID` | Inspect tasks; `show` includes dependencies, runs and events |
| `ready ID` / `draft ID` | Move between draft and ready |
| `cancel ID` | Cancel a draft or ready task; does not satisfy its dependents |
| `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` | Change prerequisites of a draft or ready task |
| `candidates` | List dependency-ready tasks in registration order without reserving them |
| `supervise --repo PATH [--cmux EXE] [--claude EXE]` | Claim one task, run it in a cmux workspace, and wait for the session to exit |
| `status` | Show the supervisor lease and whether its heartbeat is stale |

Verification commands are stored as task instructions and are not executed by this queue CLI yet. Task descriptions and acceptance criteria are optional during registration.

## Run one task with the supervisor

Requires cmux and an authenticated Claude Code on PATH (or pass `--cmux` / `--claude`). Run the supervisor in a dedicated terminal; it processes one task and exits.

```sh
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" supervise --repo /path/to/repository
```

The base commit is the repository's `main`. Runtime files live next to the database in `<db>.runs/<run-id>/`: the prompt, a snapshot of the runtime binary, the worktree on branch `taskq/<run-id>`, Claude's debug log, the receipt, and the final terminal screen. The workspace command starts a hidden `session` wrapper that launches Claude with the run ID as its session ID and reports heartbeats and the exit code.

Claude may wait for trust or permission prompts in the workspace; answer them there. A receipt does not end the session. After Claude reports completion, send `/exit` in the workspace; the supervisor then records `validating` (exit code 0) or `failed` and releases its lease. If provisioning fails, the run, its lease, and any created resources are kept for inspection; check `show ID` and `status`.

## Development checks

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The tests use temporary databases and do not require cmux, Claude Code, or network access after dependencies have been fetched.

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Active plan](docs/plans/current.md)
- [Architecture decisions](docs/adr/README.md)
