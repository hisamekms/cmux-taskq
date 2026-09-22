# cmux-taskq

cmux-taskq is a Rust task orchestrator for running dependency-aware development tasks in cmux workspaces and isolated Git worktrees.

The runtime is distributed as a binary. Claude Code and Codex integrations are distributed as plugins that invoke the binary.

## Current status

The Rust/SQLite queue and a single-run supervisor are implemented. Tasks, dependencies, state transitions, candidate selection, run reservation, supervisor leases, process heartbeats, and events are persisted locally. `supervise` claims one ready task, creates a Git worktree and a cmux workspace, starts an interactive Claude Code session through a wrapper, records the session exit, validates the completion receipt against Git and the task's verification commands, and closes the workspace of an accepted run. Claude Code's interactive lifecycle was [verified first](docs/journal/001-claude-lifecycle-spike.md).

Integration confirmation and `completed` transitions are the next steps. A validated run stays in `awaiting_integration`; its workspace is closed, while its worktree and branch are kept until the result is merged into `main` by hand. There is no manual `complete` command, and stale supervisor leases are never taken over automatically.

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
| `supervise --repo PATH [--cmux EXE] [--claude EXE]` | Claim one task, run it in a cmux workspace, wait for the session to exit, validate its receipt, and close the workspace on success |
| `status` | Show the supervisor lease and whether its heartbeat is stale |

Verification commands are shell lines that the supervisor runs in the worktree (`/bin/sh -c`) after the session exits; the agent is asked to run them too. Task descriptions and acceptance criteria are optional during registration.

## Run one task with the supervisor

Requires cmux and an authenticated Claude Code on PATH (or pass `--cmux` / `--claude`). Run the supervisor in a dedicated terminal; it processes one task and exits.

```sh
target/debug/cmux-taskq --db "$taskq_demo_dir/queue.db" supervise --repo /path/to/repository
```

The base commit is the repository's `main`. Runtime files live next to the database in `<db>.runs/<run-id>/`: the prompt, a snapshot of the runtime binary, the worktree on branch `taskq/<run-id>`, Claude's debug log, the receipt, and the final terminal screen. The workspace command starts a hidden `session` wrapper that launches Claude with the run ID as its session ID and reports heartbeats and the exit code.

Claude may wait for trust or permission prompts in the workspace; answer them there. A receipt does not end the session. After Claude reports completion, send `/exit` in the workspace. A nonzero exit code marks the run `failed`. With exit code 0 the supervisor validates the receipt: it must name this run, report `succeeded`, give evidence for passed checks and a reason for `not_applicable` ones, and its commit must be the clean head of the run branch on top of the base commit. The supervisor then reruns the task's verification commands in the worktree, logging each to `<run-dir>/verify-N.log`. A run that passes becomes `awaiting_integration` with its `result_commit` recorded; anything else becomes `failed` with the reason in `last_error`. Only an accepted run has its cmux workspace closed (`workspace_closed_at` is set once cmux confirms); its worktree and branch stay until integration. If the close fails, the run stays `awaiting_integration` with a `cleanup_failed` event and the error in `last_error`, and `workspace_closed_at` stays null so the workspace is not treated as cleaned. A failed run keeps its workspace, worktree, and branch. Then the lease is released. If provisioning or validation itself errors, the run, its lease, and any created resources are kept for inspection; check `show ID` and `status`.

The receipt is JSON at `<run-dir>/receipt.json`, written by atomic rename:

```json
{"run_id": "<run id>", "result": "succeeded", "commit": "<full SHA>",
 "tests": {"status": "passed", "evidence_or_reason": "cargo test: 15 passed"},
 "e2e": {"status": "not_applicable", "evidence_or_reason": "library change"},
 "subagent_review": {"status": "passed", "evidence_or_reason": "no findings"},
 "summary": "..."}
```

`status` is `passed`, `failed`, or `not_applicable`; `result` is `succeeded` or `failed`. The receipt's claims never make a run succeed on their own.

## Development checks

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

The tests use temporary databases and do not require cmux, Claude Code, or network access after dependencies have been fetched. Line coverage must stay at or above 80% (`cargo install cargo-llvm-cov`). The end-to-end happy path in `tests/e2e.rs` drives the real binary through cmux with a stub agent and is ignored by default; run it with `cargo test --locked --test e2e -- --ignored` where cmux is available.

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Active plan](docs/plans/current.md)
- [Task journals](docs/journal/README.md)
- [Architecture decisions](docs/adr/README.md)
- [Agent instructions](AGENTS.md)
