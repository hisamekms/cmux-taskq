# review and integrate: details

Read this when the `dagq-land` skill's short form is not enough.

## What review.md holds

`review ID` writes `<run_dir>/review.md` through a temporary file and a rename. It has a header (run, base, head, branch, worktree, where `integrate-verify-N.log` goes), the task (description, acceptance, verification commands), the goal's acceptance and constraints when the task has a goal, the receipt (summary, tests, e2e, subagent_review, follow_ups), `git log --oneline <base>..<head>`, `git diff --stat <base>...<head>`, and the full `git diff <base>...<head>`. `base` is the run's base commit, or the current `main` once a session rebased `head` onto it; `head` is the receipt's commit.

Validation checks only the receipt, the commit, a clean worktree and required evidence; it runs no verification command and writes no `verify-N.log`. The task's verification commands run once per commit, in `integrate` after its rebase (ADR-0023 decision 1), and write `integrate-verify-N.log` in `run_dir` (`show ID --full`). Before the first `integrate` there is no verification output to read; the receipt's `tests` evidence is the worker's own claim.

## integrate

```sh
"$DAGQ" integrate ID        # this task's run (also a needs_session one; the supervisor lands those itself)
"$DAGQ" integrate --next    # the oldest run awaiting integration
```

`integrate` takes the single integration slot, rebases the run's worktree onto the current `main` and re-validates it: the receipt must name the worktree head, the rebased head must sit on `main` with a clean tree, and the task's verification commands run, their only run for this commit. It then squashes the rebased tree into one commit on `main` (title, receipt summary, trailers `Dagq-Task: ID` and `Dagq-Run: RUN_ID`) and removes the worktree and branch; the run's history stays under `refs/dagq/runs/<run-id>`.

The commands run on every landing, also when the rebase is a no-op: they show as `verification_command` events with `phase: "integration"` and write `integrate-verify-N.log` in `run_dir`. A failing command parks the run as `needs_session` and the supervisor resumes its session. `verification_skipped` stays in the output and the `run_integrated` payload and is always `false`; older runs may still carry an `integration_verification_skipped` event from before ADR-0023. Files left by an earlier attempt can remain, so judge by the events after the last `integration_rebased` (`show ID --full`), not by the files.

After landing, `integrate` pushes `main` to `origin` (`git push origin main`) and reports it as `push: {outcome, remote, error, reason}` on `integrated`, with the event `push_finished`, `push_skipped` (`--no-push`, or no `origin` remote) or `push_failed` (attention `push main`) on the run. A failed push never undoes the landing or fails `integrate`.

Outcomes: `integrated`, `needs_session` (the rebase was aborted and the worktree is back on `result_commit`, or the failed verification left the rebased tree in the worktree), `failed`, and `no_run_awaiting` (`--next` only; `needs_session` runs are not picked by `--next`).

The landing happens in the current directory's repository; pass `--repo PATH` only when using `DAGQ_DB` from outside it.

## follow_ups

A receipt's `follow_ups` is an optional array of `{title, description}` for work the worker found outside its task. It is in review.md and in the `receipt` of the `validation_finished` event (`show ID --full`). When the run lands, `integrate` registers each entry whose `title` is a non-blank string and whose `description` is a string as a `draft` task (ADR-0019 decision 4): the title and description as proposed, no acceptance, verification commands or dependencies, the landed task's goal (none when the task has no goal; none with `goal_closed: true` in the event when the goal is closed), and the context "task <id>（<title>）の run <run-id> の receipt が提案した follow_up". Each registration records `follow_up_registered` (`task_id`, `title`, `index`) on the run, and an entry already recorded is never registered again, so a landing after `needs_session` registers once. An entry that is not registered gets a `follow_up_registered` with `task_id: null`, `skipped` and the entry as `follow_up`; report it to the user with the others. The `integrated` JSON lists them as `follow_ups: [{task_id, title}]`. A draft is never claimed: report the IDs and ask the user whether each becomes `ready` or is canceled.
