---
name: dagq-land
description: Review a dagq run that is awaiting_integration and land it on main with integrate (which also pushes main) when the subagent review passes, without asking anyone; only on doubt register an approve_landing ask and act on its answer (land, send_back, cancel) when it arrives. The review runs in a subagent from the review.md file that review ID writes, so the diff never enters this session. Use when status or watch reports "review and integrate", when an approve_landing ask was answered, or when asked to review, integrate, land, or push a finished dagq task. A needs_session run is resumed (and, once integrate was called, landed) by the runtime; only one it gave up on is the dagq-session skill's. Not for a stuck integrating lease (dagq-recover).
---

# dagq: review a run and land it on main

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Never merge, rebase, cherry-pick or fast-forward a run's branch yourself: landing is the runtime's job, and it keeps `main` linear with one squash commit per task. Never read the full diff in this session.

Before reviewing, check the run: an open `approve_landing` ask on it in `"$DAGQ" asks` means the review is done and it waits for the answer, so move on; an answered one is step 5; a latest note `send_back` or `cancel` in `show ID` means it was already decided (step 5), so never review or land it again.

## 1. Write the review file

```sh
"$DAGQ" review ID
```

It takes the task's run in `awaiting_integration` (or `needs_session`), writes `<run_dir>/review.md` and prints only `{"run_id", "task_id", "path", "base", "head", "files_changed", "insertions", "deletions"}`. It refuses a task with no such run.

## 2. Review in a subagent

Start a subagent (the Agent tool) with `path` and this request: read the file; check the diff against the task's acceptance, the goal's constraints and the receipt's claims (tests, e2e, subagent review), and flag changes the task did not ask for; list the titles of the receipt's `follow_ups`; answer with a verdict, `pass` or `concern`, and for `concern` at most a few reasons with file and line, never the diff itself. Take only that answer back.

When the repository asks the maintainer to check something beyond the file (for example the receipt's `e2e` evidence and the run's logs for a run that changed the runtime), add it to the same request with the paths from `"$DAGQ" show ID --full` (`run_dir`).

## 3. Land on a pass, ask only on doubt

The verdict is a `concern` when any of these holds:

- the receipt or the diff disagrees with the task's acceptance;
- the diff has changes the task did not ask for;
- the subagent returned findings.

On `pass`, go straight to step 4: nobody is asked. On `concern`, do not land. Register the doubt as an ask for the person and go on with the next attention:

```sh
"$DAGQ" ask --kind approve_landing --run <run_id> \
  --question "task <id> <title>: <which condition holds>; <the reasons>; head <head>, <files_changed> files +<insertions> -<deletions>; follow_ups: <titles or none>" \
  --option land --option send_back --option cancel
```

Write the question so it can be answered without this session (the inbox shows it as is). Asking twice returns the same open ask. **Never run `integrate` on a run with a concern until its ask is answered `land`.** A watch returning is never a reason to land, and one run's answer says nothing about another.

## 4. Land it

```sh
"$DAGQ" integrate ID
```

`integrate` rebases the run onto the current `main`, re-validates it and runs the verification commands (their only run for the commit; validation does not run them), squashes it into one commit on `main` and removes the worktree and branch. Read `outcome`:

- `integrated`: `run.result_commit` is the new `main` head and the task is `completed`; dependents become candidates. `push` says what became of pushing `main` to `origin`: `pushed`, `skipped` (`reason`: `--no-push`, or no `origin`) or `failed` (`error`). The landing stands in every case. `follow_ups` lists the draft tasks registered from the receipt's `follow_ups` (`[{task_id, title}]`).
- `needs_session`: nothing reached `main`; `reason` names the conflicting files or the failed verification. The supervisor now resumes the run's session and, because this `integrate` approved the run, lands it itself: do not run `integrate` for it again, and do not open a session for it. `status` shows `resuming (runtime)` until the run lands; only after three failed resumes does it come back as `resume session` (the `dagq-session` skill).
- `failed`: the run's receipt reported `failed`; the task stays `in_progress` (`ready ID` retries, `cancel ID` drops it).

An error (exit status 1) leaves `main` untouched and puts the run back with the message in `last_error`; a `main` checkout with uncommitted changes that overlap the landing is a common cause. Fix it and run `integrate` again. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-land/reference/integrate.md` has the details (`--next`, re-validation, logs, what review.md holds, the answers without a command); read it only when an outcome is unclear.

## 5. Act on the answer of an approve_landing ask

It arrives from your `watch` as `ask_answered` (attention `read the answer of ask <id> and close it`). Read it with `"$DAGQ" asks --role maintainer`, then:

- `land`: run step 4 for the task, then `"$DAGQ" ask close <id>`.
- `send_back` (with what to change): the run goes back to a session to change it. No command reopens the session of an `awaiting_integration` run yet (the runtime's resume only follows a failed `integrate`), so record the answer on the run with `"$DAGQ" note --run <run_id> --kind send_back --text "<the answer>"`, close the ask and report it; `reference/integrate.md` says what follows.
- `cancel`: the task is dropped. `cancel` refuses a task whose run is still `awaiting_integration`, so record it with `note --run <run_id> --kind cancel`, close the ask and report it, as for `send_back`.
- Any other text: follow it when it says what to do; otherwise ask again with a `decide` ask on the run.

## 6. Report, and push only after a failed push

`integrate` pushes `main` itself; do not run `git push` after a `pushed` or `skipped` outcome (use `integrate ID --no-push` where the repository must not be pushed). When `push.outcome` is `failed`, or `status` / `watch` shows `push main` (`kind: push_failed`), fix its cause (register a `decide` ask when it needs the person, such as a rejected non-fast-forward or credentials), then run `git push origin main`. The attention clears at the next successful push by `integrate`.

Report the outcome: the task, the landed commit, what it unblocked, and each draft task in `follow_ups` (ID and title). `integrate` already registered them as `draft` on the task's goal (without a goal when the goal is closed); do not `add` or `ready` them. Deciding whether each becomes `ready` is the planner's, with the person (the `dagq-planner` skill); the supervisor never picks up a draft.
