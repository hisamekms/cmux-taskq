---
name: taskq
description: Register and inspect cmux-taskq tasks through the locally built cmux-taskq binary. Use when the user wants to queue a development task for cmux-taskq (title, description, acceptance criteria, verification commands, dependencies), make it ready, list tasks, check a task's status or run result, or find the cmux-taskq binary and queue database.
---

# cmux-taskq: register and inspect tasks

cmux-taskq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `cmux-taskq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

Starting a run and confirming integration are in the `taskq-run` skill; recovering an interrupted run is in `taskq-recover`.

## 1. Locate the binary and the queue

Run the plugin launcher, which resolves the binary and forwards every command to it from the current directory:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/taskq" --resolve
```

It prints `{"binary", "version", "repo", "db", "db_exists", "queue_dir", "runs_dir", "source", "git_common_dir"}`.

- Binary: `CMUX_TASKQ_BIN` if set, otherwise `cmux-taskq` on PATH. If the launcher prints an error instead, tell the user to build it in the cmux-taskq repository with `cargo build --locked` and either put `target/debug/cmux-taskq` on PATH or `export CMUX_TASKQ_BIN=/absolute/path/to/cmux-taskq`, then retry.
- Queue: one per repository. The binary resolves it from the current directory's Git common directory to `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db` (default `~/.local/share/cmux-taskq/<hash>/queue.db`); `source` is `repository`. Every worktree of the repository, including task worktrees, resolves to the same queue, and runs live in `runs_dir` next to it. Run the launcher from inside the repository the tasks belong to; outside a repository it fails. `CMUX_TASKQ_DB=/path/to/queue.db` uses another queue file instead (`source` becomes `db_flag`; the launcher passes it as `--db`).
- If `db_exists` is false, create the queue once: `"${CLAUDE_PLUGIN_ROOT}/bin/taskq" init` (it creates the directory, binds the queue to this repository, and also migrates an existing queue while keeping its tasks). A queue bound to a different repository is refused by every command; that only happens with `CMUX_TASKQ_DB`, so unset it or point it at the right file.

Report the version and the database path to the user the first time in a session. Use `TASKQ="${CLAUDE_PLUGIN_ROOT}/bin/taskq"` below.

## 2. Register a task

Collect from the user, asking only for what is missing:

- title (required, one line)
- description: what to change and where
- acceptance: how a reviewer decides the task is done
- verification commands: shell lines the supervisor reruns in the worktree after the session (for example `cargo test --locked`); repeat `--verify`
- dependencies: task IDs that must be `completed` first; repeat `--depends-on`

```sh
"$TASKQ" add "TITLE" \
  --description "DESCRIPTION" \
  --acceptance "ACCEPTANCE" \
  --verify "cargo test --locked" --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --depends-on 3
```

The task is registered as `draft` and the JSON includes its `id`. Then make it runnable and confirm it is a candidate:

```sh
"$TASKQ" ready ID
"$TASKQ" candidates
```

`candidates` lists ready tasks whose dependencies are all `completed`, in registration order, without reserving them. A ready task missing from `candidates` is blocked by a dependency; show the blocking IDs from `show ID`'s `dependencies`. Use `draft ID` to take a task back for editing, `cancel ID` to drop it (its dependents are not satisfied), and `dependency add TASK PREDECESSOR` / `dependency remove TASK PREDECESSOR` to change prerequisites of a draft or ready task. Self-dependencies and cycles are rejected.

## 3. Inspect

| Command | Use |
| --- | --- |
| `"$TASKQ" list` | All tasks with `id`, `title`, `status` |
| `"$TASKQ" show ID` | `task`, `dependencies`, `runs`, `events`, `processes` |
| `"$TASKQ" candidates` | What the next `supervise` would pick |
| `"$TASKQ" locate` | The queue this directory resolves to (`db`, `runs_dir`, `git_common_dir`, `db_exists`) without opening it |
| `"$TASKQ" status` | Supervisor lease (`supervisor` null when idle) and `heartbeat_stale` |
| `"$TASKQ" doctor` | Lease liveness, unfinished runs, processes, worktree/receipt existence, `blockers`, `recoverable` |

Task `status`: `draft` → `ready` → `in_progress` → `completed`, or `canceled`. A task stays `in_progress` while any run is unfinished or awaiting integration.

Run `status` in `runs` (latest last): `claimed`, `starting`, `running`, `validating` are unfinished; `awaiting_integration` means the receipt and verification passed and the branch waits for a manual merge into `main`; `integrated` means the merge was confirmed and the task is `completed`; `failed` and `interrupted` keep their worktree and workspace for inspection, with the reason in `last_error`.

Useful run fields: `branch` (`taskq/<run-id>`), `worktree_path`, `workspace_id` (cmux), `run_dir` (prompt, logs, `receipt.json`, `verify-N.log`), `receipt_path`, `result_commit`, `last_error`, `workspace_closed_at`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. Neither a Stop hook firing, an idle session, nor the receipt file's existence means success; the supervisor validates the receipt against Git and the verification commands before a run becomes `awaiting_integration`. Summarize for the user: task status, latest run status, branch and commit to review, and the next step (`taskq-run` to start or integrate, `taskq-recover` if the run is stuck).
