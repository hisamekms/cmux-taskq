---
name: dagq-land
description: Review a dagq run that is awaiting_integration and, when the subagent review passes, land it on main with integrate, which also pushes main to origin; the user is asked only on doubt. The review runs in a subagent from the review.md file that review ID writes, so the diff never enters this session. Use when status or watch reports "review and integrate", or when the user asks to review, integrate, land, or push a finished dagq task. A needs_session run is resumed (and, once integrate was called, landed) by the runtime; only one it gave up on is the dagq-session skill's. Not for a stuck integrating lease (dagq-recover).
---

# dagq: review a run and land it on main

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Never merge, rebase, cherry-pick or fast-forward a run's branch yourself: landing is the runtime's job, and it keeps `main` linear with one squash commit per task. Never read the full diff in this session.

## 1. Write the review file

```sh
"$DAGQ" review ID
```

It takes the task's run in `awaiting_integration` (or `needs_session`), writes `<run_dir>/review.md` and prints only `{"run_id", "task_id", "path", "base", "head", "files_changed", "insertions", "deletions"}`. It refuses a task with no such run.

## 2. Review in a subagent

Start a subagent (the Agent tool) with `path` and this request: read the file; check the diff against the task's acceptance, the goal's constraints and the receipt's claims (tests, e2e, subagent review), and flag changes the task did not ask for; list the titles of the receipt's `follow_ups`; answer with a verdict (`approve` or `changes needed`) and at most a few findings with file and line, never the diff itself. Take only that answer back.

When the repository asks the maintainer to check something beyond the file (for example the receipt's `e2e` evidence and the run's logs for a run that changed the runtime), add it to the same request with the paths from `"$DAGQ" show ID --full` (`run_dir`).

## 3. Land on a pass, ask only on doubt

If the verdict is `approve` and none of these holds, go on to step 4 without waiting:

- the receipt or the diff disagrees with the task's acceptance;
- the diff has changes the task did not ask for;
- the subagent returned findings.

If any holds, do not land: tell the user, in a few lines, the task and its title, `branch` and `head`, the diffstat numbers, the verdict and findings, which condition holds, and the titles of the receipt's `follow_ups` if any (`integrate` registers them as draft tasks when the run lands), then wait for their answer. **Do not run `integrate` on a run with doubt until the user approves it.** A watch returning is never a reason to land, and approving one run says nothing about another.

If the user wants changes, the run goes back to a session: see the `dagq-session` skill, or have the user `ready` the task again after editing it.

## 4. Land it

```sh
"$DAGQ" integrate ID
```

`integrate` rebases the run onto the current `main`, re-validates it and runs the verification commands (their only run for the commit; validation does not run them), squashes it into one commit on `main` and removes the worktree and branch. Read `outcome`:

- `integrated`: `run.result_commit` is the new `main` head and the task is `completed`; dependents become candidates. `push` says what became of pushing `main` to `origin`: `pushed`, `skipped` (`reason`: `--no-push`, or the repository has no `origin`) or `failed` (`error`). The landing stands in every case. `follow_ups` lists the draft tasks registered from the landed receipt's `follow_ups` (`[{task_id, title}]`, empty when there were none).
- `needs_session`: nothing reached `main`; `reason` names the conflicting files or the failed verification. The supervisor now resumes the run's session to resolve it and, because this `integrate` approved the run, lands it itself: do not run `integrate` for it again, and do not open a session for it. Report it; `status` shows `resuming (runtime)` until the run lands, and only after three failed resumes does it come back as `resume session` (the `dagq-session` skill).
- `failed`: the run's receipt reported `failed`; the task stays `in_progress` (`ready ID` retries, `cancel ID` drops it).

An error (exit status 1) leaves `main` untouched and puts the run back with the message in `last_error`; a `main` checkout with uncommitted changes that overlap the landing is a common cause. Fix it and run `integrate` again. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-land/reference/integrate.md` has the details (`--next`, re-validation, logs, what review.md holds); read it only when an outcome is unclear.

## 5. Report, and push only after a failed push

`integrate` pushes `main` to `origin` itself; do not run `git push` after a `pushed` or `skipped` outcome (use `integrate ID --no-push` where the repository must not be pushed). When `push.outcome` is `failed`, or `status` / `watch` shows the attention `push main` (`kind: push_failed`), report `error` to the user, fix its cause (with the user's go-ahead when it needs one, such as a rejected non-fast-forward or credentials), then run `git push origin main`. The attention clears at the next successful push by `integrate`.

Report the outcome: the task, the landed commit, what it unblocked, and each draft task in `follow_ups` (ID and title). `integrate` already registered them as `draft` on the task's goal (without a goal when the goal is closed), so do not `add` them again; ask the user whether each should become `ready` (`ready ID`, after `edit` when it needs acceptance or verification commands) or be canceled, and leave it `draft` until they answer. The supervisor never picks up a draft.
