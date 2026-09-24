# doctor and recover: details

Read this when diagnosing a run by hand (the `dagq-recover` skill, sections 1–3).

## doctor --full

Plain `doctor` is the compact form, one line's worth per supervisor and per run (`run_id`, `task_id`, `status`, `lease_stale`, `recoverable`, `blocker_count`, `workspace_id`, `worktree_path`); diagnosing needs `--full`, which prints the lease, processes, paths and the `blockers` described below.

- `supervisors`: one entry per registered `supervise` process (`registered: true`, with `pid`, `alive` (`kill -0`), `parallel`, `started_at`, `heartbeat_at`, `heartbeat_age_secs`, `run_ids`, and `stale` when the PID is dead or the heartbeat is older than 30 seconds), plus one per process that holds leases without a registration, such as a running `integrate` (`registered: false`). A resident supervisor is listed even with an empty `run_ids`; empty `supervisors` means no supervisor is registered and no run is owned by anyone. A stale registration is left by a killed or hung supervisor; the runtime never deletes it, so report it to the person rather than trying to remove it, and `recover` works on runs regardless of it.
- `runs`: every run in `claimed`, `starting`, `running`, `validating`, or `integrating` with `task_id`, `workspace_id`, `worktree_exists`, `run_dir_exists`, `receipt_exists`, `last_error`, its own `lease` (`pid`, `alive`, heartbeat age, `stale`; `null` when no supervisor owns it), and each registered `wrapper` / `agent` process with `pid`, `alive`, heartbeat age, and `exit_code` (`alive` is null once an exit is recorded).
- `blockers`: per run, what still prevents recovery. `recoverable: true` when empty. Only the run's own lease and processes count; other runs, healthy or not, never block it.

## Common cases

- The supervisor gave the run up (`last_error` set, `lease` null, `runtime_error` event with `lease_released: true`, for example after the wrapper's heartbeat was lost and its process died; a wrapper whose process lives on is not given up but asked to `/exit`, and raised as a `stuck_exit` ask if it does not) while its session may still be running; the supervisor keeps serving other runs, and recovers the run itself once the session's processes are gone.
- The supervisor was killed (lease stale, PID dead) while the sessions are still running.
- The whole machine restarted (everything dead).
- The supervisor is alive but its heartbeat stopped (the person stops it with `down --force`).
- An `integrate` process died while landing a run (the run is `integrating` with a stale lease and no wrapper/agent processes).

A `running` or `validating` run whose lease is stale while its wrapper is alive (heartbeat within 30 seconds) or has recorded its exit is not a case for `recover`: the next supervisor with a free slot adopts it (a `run_adopted` event; the lease and `supervisor_token` move to that supervisor) and finishes it. Start or wait for a supervisor (`up` reuses a live one) and watch `show ID`. Likewise a run with `exit_request_timed_out` and a live lease is not given up: its supervisor still watches it, and its `stuck_exit` ask, once answered, leads to `/exit` in its workspace (`reference/stuck-exit.md`). Only a run whose wrapper is dead or silent, a `claimed` / `starting` run, an `integrating` run, or a run without a lease needs `recover`.

## recover

`"$DAGQ" recover RUN_ID` prints `{"outcome": "recovered", "run": ...}` on success: the run is `interrupted` (an `integrating` run goes back to `awaiting_integration` instead, because its validated result is intact; land it again as in `reference/review-by-hand.md`), a `run_recovered` event records what was checked, and that run's lease (if any) is deleted. Other runs, their leases and processes are untouched. The worktree, branch, cmux workspace, and run directory are kept, and the task stays `in_progress`. The next supervisor triages the `interrupted` run.

Recovery is refused while any process registered for that run is alive, its lease heartbeat is fresh, or its lease PID is alive. The person ends those first: `/exit` in the task's cmux workspace (`cmux workspace list` shows it by `workspace_id`), and stopping a hung supervisor process. Do not kill processes unless the person asks. Runs owned by a live supervisor are not orphans; leave them to it.
