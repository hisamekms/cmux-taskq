# review and integrate: details

Read this when the `dagq-land` skill's short form is not enough.

## What review.md holds

`review ID` writes `<run_dir>/review.md` through a temporary file and a rename. It has a header (run, base, head, branch, worktree, verify logs), the task (description, acceptance, verification commands), the goal's acceptance and constraints when the task has a goal, the receipt (summary, tests, e2e, subagent_review, follow_ups), `git log --oneline <base>..<head>`, `git diff --stat <base>...<head>`, and the full `git diff <base>...<head>`. `base` is the run's base commit, or the current `main` once a session rebased `head` onto it; `head` is the receipt's commit.

The `verify-N.log` files in `run_dir` (`show ID --full`) hold the supervisor's own verification output.

## integrate

```sh
"$DAGQ" integrate ID        # this task's run (also resumes a needs_session run)
"$DAGQ" integrate --next    # the oldest run awaiting integration
```

`integrate` takes the single integration slot, rebases the run's worktree onto the current `main` and re-validates it: the receipt must name the worktree head, the rebased head must sit on `main` with a clean tree, and the task's verification commands are rerun. It then squashes the rebased tree into one commit on `main` (title, receipt summary, trailers `Dagq-Task: ID` and `Dagq-Run: RUN_ID`) and removes the worktree and branch; the run's history stays under `refs/dagq/runs/<run-id>`.

When the rebase is a no-op and the worktree head is still the run's `result_commit` (the commit the supervisor's validation ran the commands on), `integrate` skips the rerun and writes no `integrate-verify-N.log`; this shows as an `integration_verification_skipped` event (`main`, `head`, `reason`) and `verification_skipped: true` in the output and the `run_integrated` payload. A head a session rewrote after `needs_session` is always rerun, which shows as `verification_command` events with `phase: "integration"` and writes `integrate-verify-N.log` in `run_dir`. Files left by an earlier attempt can remain, so judge by the events after the last `integration_rebased` (`show ID --full`), not by the files.

After landing, `integrate` pushes `main` to `origin` (`git push origin main`) and reports it as `push: {outcome, remote, error, reason}` on `integrated`, with the event `push_finished`, `push_skipped` (`--no-push`, or no `origin` remote) or `push_failed` (attention `push main`) on the run. A failed push never undoes the landing or fails `integrate`.

Outcomes: `integrated`, `needs_session` (the rebase was aborted and the worktree is back on `result_commit`, or the failed verification left the rebased tree in the worktree), `failed`, and `no_run_awaiting` (`--next` only; `needs_session` runs are not picked by `--next`).

The landing happens in the current directory's repository; pass `--repo PATH` only when using `DAGQ_DB` from outside it.

## follow_ups

A receipt's `follow_ups` is an optional array of `{title, description}` for work the worker found outside its task. It is in review.md and in the `receipt` of the `validation_finished` event (`show ID --full`). Report it before `integrate` and again with the outcome, so the user has it registered rather than lost with the run.
