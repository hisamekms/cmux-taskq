# up and down: outputs and edge cases

Read this when `up` or `down` returns something the `dagq-recover` skill (section 5) does not explain. Both are typed from the inbox or a planner session, on the person's word.

## up

`up` preflights cmux, Claude Code, the repository and an initialized queue (run `init` from the `dagq` skill first if the queue does not exist), deletes supervisor registrations whose process is dead (`pruned_supervisors`), keeps one supervisor resident as a launchd LaunchAgent, opens the inbox's cmux workspace (`[<repo>]inbox`), and ends with a `doctor` summary. It opens no planner (ADR-0044 decision 6): a person opens one per plan with `dagq plan` (each call a new `[<repo>]planner#<id>`, unpinned; `dagq planners` lists them), and the supervisor opens its own for revises and drafts. Pass `--plugin-dir "$CLAUDE_PLUGIN_ROOT"` so the sessions it and the supervisor open load this plugin; add `--repo PATH` only for a checkout other than the working directory, and `--cmux EXE` / `--claude EXE` when those are not on PATH.

- `supervisor`: `{"outcome": "started" | "reused" | "restarted", "mode", "version", "pid", "token", "workspace_id", "plist", "log_dir", "auto_update"}`. `reused` means a live, heartbeating supervisor of this binary's own build already served this queue and nothing was touched but its `auto_update`, which follows this `up`'s `--auto-update`. `mode` is `launchd`, or `in_cmux` with the `workspace_id` it runs in; it is null for a supervisor someone started by hand.
- `inbox`: `{"outcome": "created" | "reused" | "skipped", "workspace_id", "name"}`. `up` finds the workspace by the UUID it recorded in the queue, never by its title: `reused` while cmux still lists it (renaming it changes nothing), `created` when it is gone. `skipped` is the normal answer when `up` is called from the inbox: the workspace carries `DAGQ_ROLE=inbox` and `DAGQ_QUEUE` in its own environment (`cmux workspace env <id> --json`), so `up` does not open a second one. `down` closes neither the inbox nor any planner.
- `up` pins the inbox's workspace (ADR-0031), and cmux refuses `cmux workspace close` on a pinned workspace (`Error: protected: ...`). To close one by hand, unpin it first: `cmux workspace-action --action unpin --workspace <id>`, then `cmux workspace close <id>`. The unpin succeeds on an unpinned workspace too. The supervisor's, the planners' and the workers' workspaces are not pinned.
- A workspace the queue recorded for a role `up` no longer opens (the resident sessions ADR-0024 and ADR-0044 retired, including the old `[<repo>]planner`) is forgotten by `up`, which leaves the workspace itself open: the person closes it with `cmux workspace close <id>`.
- `warnings`: why the queue's workspace group (`[<repo>]`, external ID the queue hash) could not be made; the workspace opened outside it. Report it; nothing else failed.
- `pruned_supervisors`: dead registrations `up` removed.
- `doctor`: `unfinished_runs` (with `lease_stale`), `awaiting_integration`, `needs_session`: the open work to report.

### A supervisor of another build

Update the fixed binary with `install` (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/update.md`), or let `up --auto-update` do it on each runtime landing; never replace the file with `cp`. `up` still replaces a supervisor whose `binary_version` differs from its own when the file changed some other way. `binary_version` is the build identifier (`X.Y.Z` for a release, `X.Y.Z-dev+<commit>[.dirty]` for a build of main), so a rebuild from another commit is replaced like a new release, with no version bump.

- **Handoff** (every live supervisor takes it): `up` asks each to exec its own binary at its next short step; the pid, token, lease, workspace and mode stay, and the sessions and runs in flight go on without waiting for them. The result is `"outcome": "restarted"` with `"handoff": true`, `previous_version`, `version` and `replaced`. It gives up after `--handoff-timeout` (default 1800 s: the supervisor may finish one landing's verification first).
- **Drain** (only for a supervisor from before the handoff, or a launchd agent whose plist names another binary path): `up` stops it and waits, with no timeout, for the runs it holds, then starts one of the new build in the mode this `up` asks for. Tell the person what it waits for rather than killing it. `up --no-wait` refuses instead whenever a run is in flight.
- To change the mode (`--in-cmux` or not), `--claude` or `--max-waiting` of a supervisor of the same build, `down --wait` and `up` again: a plain `up` reuses it as it is.

Opening a queue never migrates it (ADR-0073). If a command stops with "run `dagq migrate`", the binary has migrations the queue lacks; `dagq migrate --check` lists them with `compatible`. `up` and `install` apply the compatible ones while everything runs. A breaking one stops `up` with an error naming `dagq install --allow-breaking`, the drain in `update.md`; tell the person before it. `unsupported queue schema version ... install a newer dagq` means the queue was migrated past this binary: use the newer fixed binary.

`up` does not replace a supervisor that is alive but no longer heartbeating (it starts a new one beside it and `status` shows the old row as `stale`; the person stops it with `down --force`).

### Runs waiting for a person

`--max-waiting N` (default 4) is how many runs may wait for a person's answer outside the `--parallel` slots (ADR-0071): a run whose session only waits on a `worker_question`, `answer_prompt`, `stalled` or `stuck_exit` ask leaves its slot and another task is claimed; once its wait ends it returns to a free slot. Runs whose wait ended and that wait for a slot count toward the limit too; at the limit a new ask leaves its run in its slot (`run_waiting_deferred`). `0` keeps every run in its slot. `up` passes `--max-waiting` to the supervisor only when it is not the default, and `status` shows it as each registration's `waiting.limit`. To change it, `down --wait` and `up --max-waiting N` (a plain `up` reuses a supervisor of the same build as it is).

A waiting run keeps its lease and session, so a drain (plain `down`, `down --wait`, `install --allow-breaking`, an `up` draining a supervisor from before the handoff) goes on until each waiting run's ask is answered and the run finishes, or its session exits. Before a drain, show the person the open asks (`status`'s `waiting` lists the runs) so that `down --wait` does not block on a question nobody sees. An exec handoff (`handoff: true`) does not wait for them: the new process rebuilds each wait from the run's events and goes on. `down --force` loses waiting runs like any other active run.

### cmux refuses the connection (launchd mode)

If `up` fails because cmux refuses a connection from outside its own terminals, no LaunchAgent was installed: the supervisor launchd would start cannot reach cmux. Report the message to the person with the remedies it names: a socket password saved in cmux's Settings, or `CMUX_SOCKET_PASSWORD` exported in the shell that runs `up` (both are theirs to do). The third remedy is `up --in-cmux`, which you may run once they have chosen it: it runs the supervisor in a cmux workspace named `[<repo>]supervisor` and needs no password, but launchd no longer restarts it, so run `up --in-cmux` again whenever `status` shows nothing serving the queue (`restart supervisor`, which the inbox's `watch` reports).

## down

`--wait` and `--force` exclude each other: `--force` does not drain first.

- Plain `down` ends the day's work: the supervisor stops claiming, finishes its active runs and exits, and launchd does not restart it.
- `--wait` blocks while the runs drain, which can take as long as a run. Use it when the next step depends on the supervisor being gone (changing its mode or `--claude`, shutting the machine down).
- `--force` loses the active runs: their leases go stale after 30 seconds and the runs are handled with the `dagq-recover` skill. Only with the person's explicit word.

The `outcome` is `draining` (plain `down`), `stopped` (`--wait`), `killed` (`--force`), or `not_running` when no live supervisor was registered; a lingering agent is unloaded in that case too, and `--force` also drops the dead registrations.

An `in_cmux` supervisor has no launchd agent, so `down` sends it SIGINT and closes its `[<repo>]supervisor` workspace once it has seen the stop through (after the drain with `--wait`, after the kill with `--force`). Plain `down` returns while it still drains, leaves the workspace open and reports it under `supervisor_workspaces` as `left_open`; run `down --wait` (or have the person close it) before the next `up --in-cmux`, which refuses to open a second supervisor workspace while the one it recorded for the queue is still open.

## Logs

`"$DAGQ" locate` prints `log_dir` (plus `label` and `launch_agent`). Each `supervise`, `integrate`, `observe` and session wrapper process writes its own JSON Lines file there, `<process>-<UTC time>-<pid>.jsonl` (one record per line: `timestamp`, `level`, `target`, `message`, `fields` such as `run_id`, `task_id`, `op` and `error`, and `spans`). A supervisor's file has its token, PID, `--parallel`, queue and repository, claims, workspaces, receipts, exit requests, rejections, the landings and pushes it ran, and the final result; `jq 'select(.fields.run_id == "<run-id>")'` picks one run. launchd's stdout/stderr go to `launchd.log`. Older binaries wrote `supervisor-<started_at>-<pid>.log` text files; they stay and nothing reads them. Read them when a supervisor is missing from `status`, when `up` reports that no supervisor registered, or when a run failed for a reason `show` does not explain. Nothing rotates them; deleting old files is the person's call.
