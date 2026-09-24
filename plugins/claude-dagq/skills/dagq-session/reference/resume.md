# needs_session runs: what the runtime's resume does

A run becomes `needs_session` only when its landing conflicted or its verification failed after the rebase, when validation found required evidence missing, or when a person answered `send_back` to the `approve_landing` ask of a review concern. A `revise` verdict goes to the still-open worker session without any resume (ADR-0027).

Most landing conflicts never reach a resume either. Before it sends `/exit` to a run whose review passed, the supervisor compares the run's head with the current `main` with `git merge-tree` (the worktree is not touched). When they conflict it records `conflict_precheck` (`main`, `head`, `conflicts`) and types the same resolution request as a resume into the still-open session (`run_dir` keeps it as `conflict-N.txt`); once the session has rebased, rewritten the receipt and gone idle (`conflict_resolved`), the run is validated, reviewed and prechecked again. The precheck counts its own requests and the run's resumes together: once they reach three, the session gets `/exit` and the supervisor opens an `approve_landing` ask for the person instead. The resume's own limit still counts resumes only. Only a conflict that appears after the `/exit` (main moved while the run waited for the landing slot), or a failed verification after the rebase, parks the run as `needs_session`. Nothing here is yours: do not type into the session while it resolves the conflict.

The supervisor opens a workspace titled like the run's worker (`[<repo>]worker#<task-id> - <task title>`, description `run <run-id> resume`) with the run's own session and types the fixed resolution request: the reason, the `main` to rebase onto, the tasks landed since the run's base, and the steps. `run_dir` keeps it as `resume-N.txt`.

Once the session has rewritten the receipt for the new head and gone idle:

- a run whose `integrate` was already called (`integration_approved`) gets `/exit` and is landed by the runtime;
- any other run keeps the session open and goes through validation and the supervisor's review like the worker's (the `dagq-land` skill, section 0).

A session that goes idle without resolving it, or runs past the resume timeout, gets `/exit` and its workspace is closed (the final screen is `terminal-resume-N.txt`). The attention `answer the prompt in workspace <id>` does not cover resumed sessions.
