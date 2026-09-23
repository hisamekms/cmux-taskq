# up and down: outputs and edge cases

Read this when `up` or `down` returns something the `dagq-maintain` skill does not explain.

## up

`up` preflights cmux, Claude Code, the repository and an initialized queue (run `init` from the `dagq` skill first if the queue does not exist), deletes supervisor registrations whose process is dead (`pruned_supervisors`), keeps one supervisor resident as a launchd LaunchAgent, opens the maintainer's cmux workspace (`[<repo>]dagq maintainer`), and ends with a `doctor` summary. Pass `--plugin-dir "$CLAUDE_PLUGIN_ROOT"` so the maintainer session it opens loads this plugin; add `--repo PATH` only for a checkout other than the working directory, and `--cmux EXE` / `--claude EXE` when those are not on PATH.

- `supervisor`: `{"outcome": "started" | "reused" | "restarted", "mode", "version", "pid", "token", "workspace_id", "plist", "log_dir"}`. `reused` means a live, heartbeating supervisor of this binary's own version already served this queue and nothing was touched. `mode` is `launchd`, or `in_cmux` with the `workspace_id` it runs in; it is null for a supervisor someone started by hand.
- `maintainer`: `{"outcome": "created" | "reused" | "skipped", "workspace_id", "name"}`. `up` finds the maintainer workspace by the UUID it recorded in the queue, never by its title: `reused` while cmux still lists it (renaming it changes nothing), `created` when it is gone. `skipped` is the normal answer when `up` is called from inside the maintainer session: the workspace carries `DAGQ_ROLE=maintainer` and `DAGQ_QUEUE` in its own environment (`cmux workspace env <id> --json`), so `up` does not open a second one. The supervisor was still started or reused.
- `warnings`: why the queue's workspace group (`[<repo>]`, external ID the queue hash) could not be made; the workspace opened outside it. Report it; nothing else failed.
- `pruned_supervisors`: dead registrations `up` removed.
- `doctor`: `unfinished_runs` (with `lease_stale`), `awaiting_integration`, `needs_session`: the open work to report.

### A supervisor of another version

After the user has swapped the `dagq` binary, a plain `up` is the whole update: it unloads the LaunchAgent, signals the old supervisor, waits (with no timeout) for it to stop claiming and finish the runs it holds, closes its workspace if it ran in one, and starts a supervisor of the new version. The result is `"outcome": "restarted"` with `previous_version`, `version` and `replaced`. The wait is the length of the runs in flight, so tell the user what it waits for rather than killing it. `up --no-wait` refuses instead whenever a run is in flight (it names the count and the run IDs and changes nothing); with nothing in flight it replaces the supervisor but gives up after 30 seconds if that supervisor does not stop.

`up` does not replace a supervisor that is alive but no longer heartbeating (it starts a new one beside it and `status` shows the old row as `stale`; the user stops it with `down --force`), and it does not notice a rebuild that did not bump the version, since `binary_version` is `CARGO_PKG_VERSION`.

### cmux refuses the connection (launchd mode)

If `up` fails because cmux refuses a connection from outside its own terminals, no LaunchAgent was installed: the supervisor launchd would start cannot reach cmux. Report the message to the user with the remedies it names: a socket password saved in cmux's Settings, or `CMUX_SOCKET_PASSWORD` exported in the shell that runs `up` (both are theirs to do). The third remedy is `up --in-cmux`, which you may run once they have chosen it: it runs the supervisor in a cmux workspace named `[<repo>]dagq supervisor` and needs no password, but launchd no longer restarts it, so run `up --in-cmux` again whenever `status` shows nothing serving the queue.

## down

`--wait` and `--force` exclude each other: `--force` does not drain first.

- Plain `down` ends the day's work: the supervisor stops claiming, finishes its active runs and exits, and launchd does not restart it.
- `--wait` blocks while the runs drain, which can take as long as a run. Use it when the next step depends on the supervisor being gone (replacing the binary, shutting the machine down).
- `--force` loses the active runs: their leases go stale after 30 seconds and the runs are handled with the `dagq-recover` skill. Only with the user's consent.

The `outcome` is `draining` (plain `down`), `stopped` (`--wait`), `killed` (`--force`), or `not_running` when no live supervisor was registered; a lingering agent is unloaded in that case too, and `--force` also drops the dead registrations.

An `in_cmux` supervisor has no launchd agent, so `down` sends it SIGINT and closes its `[<repo>]dagq supervisor` workspace once it has seen the stop through (after the drain with `--wait`, after the kill with `--force`). Plain `down` returns while it still drains, leaves the workspace open and reports it under `supervisor_workspaces` as `left_open`; run `down --wait` (or have the user close it) before the next `up --in-cmux`, which refuses to open a second supervisor workspace while the one it recorded for the queue is still open.

## Logs

`"$DAGQ" locate` prints `log_dir` (plus `label` and `launch_agent`). Each supervisor start appends to its own `supervisor-<started_at>-<pid>.log` there (token, PID, `--parallel`, queue and repository, claims, workspaces, receipts, exit requests, rejections, the final result), and launchd's stdout/stderr go to `launchd.log`. Read them when a supervisor is missing from `status`, when `up` reports that no supervisor registered, or when a run failed for a reason `show` does not explain. Nothing rotates them; deleting old files is the user's call.
