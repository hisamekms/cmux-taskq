# Review by hand, integrate, and a failed push

Read this for attention `review by hand` (`kind` `review_failed`), `review and integrate`, or `push main` (the `dagq-recover` skill, section 6). Everything here is done on the person's word. Never merge, rebase, cherry-pick or fast-forward a run's branch yourself: landing is the runtime's job, and it keeps `main` linear with one squash commit per task. Never read the full diff in the inbox or planner session.

## The supervisor reviews first

The supervisor reviews every run it accepts with the session still open (ADR-0027) and records `review_finished` (`verdict`, `reasons`, `summary`) in `show ID --full`. While it holds the run, `status` shows `reviewing (runtime)`: do nothing, and `integrate` refuses it. `pass`: it exits the session, lands the run and pushes. `revise`: it types the reasons into the live session, which fixes them; the run is reviewed again, at most twice. `concern`, or a third review that does not pass: it closes the session and opens an `approve_landing` ask for the inbox, and applies the answer (`land`, `send_back`, `cancel`; below). Do not review those runs again.

Only `review by hand` (the headless review exited non-zero, printed no verdict or timed out; `run_dir` keeps `review-N.out` / `.err`) and `review and integrate` without a review come here.

## Steps

1. `"$DAGQ" review ID` writes `<run_dir>/review.md` and prints only `{"run_id", "task_id", "path", "base", "head", "files_changed", "insertions", "deletions"}`. It refuses a task with no run `awaiting_integration` (or `needs_session`).
2. Start a subagent (the Agent tool) with `path` and this request: read the file; check the diff against the task's acceptance, the goal's constraints and the receipt's claims (tests, e2e, subagent review), and flag changes the task did not ask for; list the titles of the receipt's `follow_ups`; answer `pass` or `concern`, and for `concern` at most a few reasons with file and line, never the diff itself. When the repository asks for more (for example the receipt's `e2e` evidence and the run's logs for a run that changed the runtime), add it with the paths from `show ID --full` (`run_dir`).
3. Report the verdict to the person. On `pass` and their word, `"$DAGQ" integrate ID`. On `concern` (the receipt or diff disagrees with the acceptance, the diff has changes the task did not ask for, or the subagent returned findings), do not land: register `"$DAGQ" ask --kind approve_landing --run <run_id> --question "<task, the reasons, head, size, follow_ups>" --option land --option send_back --option cancel`, which the supervisor applies once answered.

`integrate` outcomes: `integrated` (`run.result_commit` is the new `main` head, the task is `completed`, `push` says whether `main` reached `origin`, `follow_ups` lists the draft tasks registered from the receipt), `needs_session` (nothing reached `main`; the supervisor resumes the run's session and, because this `integrate` approved the run, lands it itself: do not `integrate` it again), `failed` (the receipt reported `failed`; the supervisor triages the run). An error leaves `main` untouched and puts the run back with the message in `last_error`; a `main` checkout with uncommitted changes that overlap the landing is a common cause. Fix it and run `integrate` again.

## A failed push

`integrate` and the supervisor's landing push `main` themselves. When `push.outcome` is `failed`, or `status` shows `push main` (`kind: push_failed`), fix the cause with the person (a rejected non-fast-forward, credentials), then `git push origin main`. The attention clears at the next successful push.

Report the landed commit, what it unblocked, and the draft tasks from `follow_ups`; whether each becomes `ready` is the planner's with the person.

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

A receipt's `follow_ups` is an optional array of `{title, description}` for work the worker found outside its task. It is in review.md and in the `receipt` of the `validation_finished` event (`show ID --full`). When the run lands, `integrate` registers each entry whose `title` is a non-blank string and whose `description` is a string as a `draft` task (ADR-0019 decision 4): the title and description as proposed, no acceptance, verification commands or dependencies, the landed task's goal (none when the task has no goal; none with `goal_closed: true` in the event when the goal is closed), and the context "task <id>（<title>）の run <run-id> の receipt が提案した follow_up". Each registration records `follow_up_registered` (`task_id`, `title`, `index`) on the run, and an entry already recorded is never registered again, so a landing after `needs_session` registers once. An entry that is not registered gets a `follow_up_registered` with `task_id: null`, `skipped` and the entry as `follow_up`; report it to the person with the others. The `integrated` JSON lists them as `follow_ups: [{task_id, title}]`. A draft is never claimed: report the IDs; whether each becomes `ready` or is canceled is decided by the planner with the person (the `dagq-planner` skill).

## approve_landing answers

A run with a `concern` (the supervisor's review, or a review by hand) stays `awaiting_integration` without a lease while its `approve_landing` ask is open; `status` shows only the ask (`answer ask <id>`, for the inbox), not the run. Asking again for the same run returns the same ask (`created: false`) while it is open.

The supervisor applies the answer itself on its next pass (ADR-0027), for a run still `awaiting_integration` that nobody leases, and then closes the ask: `land` records `integration_approved` (with `ask_id`, `push: true`) and lands the run in the single integration slot as `integrate` would; `send_back` records `landing_decided` (`status: needs_session`, `reason`: the latest `review_finished` reasons) and the supervisor resumes the run's session with that reason, after which the run is validated and reviewed again; `cancel` records `landing_decided` (`status: failed`), fails the run and cancels the task. The ask's `ask_answered` carries `runtime_delivers: true` then and wakes nobody. Another answer, or one for a run that is no longer `awaiting_integration`, is the inbox's to read and close. Never edit the run's worktree, `recover` it or open a session for it.
