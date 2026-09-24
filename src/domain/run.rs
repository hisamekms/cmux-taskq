//! The run aggregate: one attempt at a task, from its claim to its landing.
//! Its fields are private; [`TaskRun::new`] starts a claimed run,
//! [`TaskRun::restore`] rebuilds a stored one, and the commands below are
//! the only way its status moves (see the state diagram in
//! `docs/design/domain-model.md`). Each command takes the run and returns
//! it changed, or refuses with [`DomainError::RunTransitionNotAllowed`]
//! naming the status it found and the operation.

use serde::Serialize;

use super::{
    CommitSha, DomainError, Provider, RunId, RunPaths, RunPlan, RunRecord, RunStatus, Task, TaskId,
    TaskStatus, require,
};

/// A run of a task. `Serialize` is the JSON the CLI prints; there is no
/// `Deserialize`: a run is built by [`TaskRun::new`] or [`TaskRun::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRun {
    id: RunId,
    task_id: TaskId,
    status: RunStatus,
    requested_provider: Provider,
    actual_provider: Provider,
    base_commit: CommitSha,
    branch: Option<String>,
    worktree_path: Option<String>,
    workspace_id: Option<String>,
    receipt_path: Option<String>,
    log_path: Option<String>,
    result_commit: Option<CommitSha>,
    repo_path: Option<String>,
    run_dir: Option<String>,
    last_error: Option<String>,
    /// Set once cmux confirmed the close; null keeps the run out of any cleaned state.
    workspace_closed_at: Option<i64>,
    created_at: String,
}

impl TaskRun {
    /// The run a supervisor starts when it claims `task` (already moved to
    /// `in_progress` by [`super::task::claim`]): `claimed`, on `base_commit`
    /// (kept in lowercase), with `provider` requested and used, and nothing
    /// provisioned yet.
    pub fn new(
        id: RunId,
        task: &Task,
        base_commit: &CommitSha,
        provider: Provider,
        claimed_at: String,
    ) -> Result<Self, DomainError> {
        require(task.status() == TaskStatus::InProgress, || {
            DomainError::RunOfUnclaimedTask {
                task_id: task.id(),
                status: task.status(),
            }
        })?;
        Ok(Self {
            id,
            task_id: task.id(),
            status: RunStatus::Claimed,
            requested_provider: provider,
            actual_provider: provider,
            base_commit: CommitSha::parse(
                base_commit.as_str().to_ascii_lowercase(),
                "base commit",
            )?,
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: claimed_at,
        })
    }

    /// The run as it was saved. Queues written by older versions hold rows
    /// that later rules would not produce (a close time without a workspace,
    /// say), so only what every version kept is checked: an integrated run
    /// names its landed commit.
    pub fn restore(record: RunRecord) -> Result<Self, DomainError> {
        require(
            record.status != RunStatus::Integrated || record.result_commit.is_some(),
            || DomainError::RunInconsistent {
                run_id: record.id.clone(),
                reason: "is integrated without a result commit",
            },
        )?;
        Ok(Self {
            id: record.id,
            task_id: record.task_id,
            status: record.status,
            requested_provider: record.requested_provider,
            actual_provider: record.actual_provider,
            base_commit: record.base_commit,
            branch: record.branch,
            worktree_path: record.worktree_path,
            workspace_id: record.workspace_id,
            receipt_path: record.receipt_path,
            log_path: record.log_path,
            result_commit: record.result_commit,
            repo_path: record.repo_path,
            run_dir: record.run_dir,
            last_error: record.last_error,
            workspace_closed_at: record.workspace_closed_at,
            created_at: record.created_at,
        })
    }

    pub fn id(&self) -> &RunId {
        &self.id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn status(&self) -> RunStatus {
        self.status
    }

    pub fn requested_provider(&self) -> Provider {
        self.requested_provider
    }

    pub fn actual_provider(&self) -> Provider {
        self.actual_provider
    }

    pub fn base_commit(&self) -> &CommitSha {
        &self.base_commit
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn worktree_path(&self) -> Option<&str> {
        self.worktree_path.as_deref()
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }

    pub fn receipt_path(&self) -> Option<&str> {
        self.receipt_path.as_deref()
    }

    pub fn log_path(&self) -> Option<&str> {
        self.log_path.as_deref()
    }

    pub fn result_commit(&self) -> Option<&CommitSha> {
        self.result_commit.as_ref()
    }

    pub fn repo_path(&self) -> Option<&str> {
        self.repo_path.as_deref()
    }

    pub fn run_dir(&self) -> Option<&str> {
        self.run_dir.as_deref()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn workspace_closed_at(&self) -> Option<i64> {
        self.workspace_closed_at
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    /// The run with its queue-local paths re-derived under `runs_dir`. The
    /// stored values are the absolute paths of the queue at claim time and go
    /// stale when the queue directory moves; a path that was never planned
    /// stays absent. `repo_path` names the repository, not the queue, and is kept.
    pub fn relocated(self, runs_dir: &std::path::Path) -> Self {
        let paths = RunPaths::new(runs_dir, &self.id);
        let resolve = |stored: Option<String>, path: &std::path::Path| {
            stored.map(|_| path.to_string_lossy().into_owned())
        };
        Self {
            run_dir: resolve(self.run_dir, &paths.run_dir),
            worktree_path: resolve(self.worktree_path, &paths.worktree),
            receipt_path: resolve(self.receipt_path, &paths.receipt),
            log_path: resolve(self.log_path, &paths.log),
            ..self
        }
    }

    /// Written by the provider's stop hook each time the agent finishes a
    /// response; newer than the receipt means the session is idle after submitting.
    pub fn idle_marker_path(&self) -> Result<std::path::PathBuf, DomainError> {
        let run_dir = self
            .run_dir
            .as_ref()
            .ok_or(DomainError::MissingRunDirectory)?;
        Ok(std::path::Path::new(run_dir).join("idle.json"))
    }
}

/// Refuses `operation` unless the run is in one of `allowed`.
fn require_status(
    run: &TaskRun,
    allowed: &[RunStatus],
    operation: &'static str,
) -> Result<(), DomainError> {
    require(allowed.contains(&run.status), || {
        DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation,
        }
    })
}

/// Executing under a supervisor or being landed: what `recover` takes back.
pub const UNFINISHED: [RunStatus; 5] = [
    RunStatus::Claimed,
    RunStatus::Starting,
    RunStatus::Running,
    RunStatus::Validating,
    RunStatus::Integrating,
];

/// `claimed` → `starting`: the supervisor planned where the run lives.
pub fn start_provisioning(mut run: TaskRun, plan: &RunPlan) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Claimed], "provision")?;
    run.status = RunStatus::Starting;
    run.repo_path = Some(plan.repo_path.clone());
    run.run_dir = Some(plan.run_dir.clone());
    run.branch = Some(plan.branch.clone());
    run.worktree_path = Some(plan.worktree_path.clone());
    run.receipt_path = Some(plan.receipt_path.clone());
    run.log_path = Some(plan.log_path.clone());
    Ok(run)
}

/// A starting run gets its cmux workspace once.
pub fn attach_workspace(mut run: TaskRun, workspace_id: String) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Starting], "attach a workspace to")?;
    require(run.workspace_id.is_none(), || {
        DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "attach a second workspace to",
        }
    })?;
    run.workspace_id = Some(workspace_id);
    Ok(run)
}

/// Whether the session wrapper may register: the run is starting in its workspace.
pub fn check_ready_for_wrapper(run: &TaskRun) -> Result<(), DomainError> {
    require_status(run, &[RunStatus::Starting], "start the wrapper of")?;
    require(run.workspace_id.is_some(), || {
        DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "start the wrapper without a workspace of",
        }
    })
}

/// `starting` → `running`: the agent registered.
pub fn mark_running(mut run: TaskRun) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Starting], "mark running")?;
    run.status = RunStatus::Running;
    Ok(run)
}

/// The session ended (`exit_code`) or went idle after its receipt with the
/// session still open (`None`, ADR-0027): `validating`, except that a
/// non-zero exit fails the run with the code as `last_error`.
pub fn finish_session(mut run: TaskRun, exit_code: Option<i32>) -> Result<TaskRun, DomainError> {
    require_status(
        &run,
        &[RunStatus::Starting, RunStatus::Running],
        "finish the session of",
    )?;
    match exit_code {
        Some(code) if code != 0 => {
            run.status = RunStatus::Failed;
            run.last_error = Some(format!("session exited with code {code}"));
        }
        _ => run.status = RunStatus::Validating,
    }
    Ok(run)
}

/// `validating` → `awaiting_integration`: the receipt names `result_commit`,
/// the clean head of the run branch.
pub fn accept(mut run: TaskRun, result_commit: CommitSha) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Validating], "accept")?;
    run.status = RunStatus::AwaitingIntegration;
    run.result_commit = Some(result_commit);
    Ok(run)
}

/// Validation refused the receipt. `resumable` (only required evidence is
/// missing, or the diff leaves the task's paths) parks the run as
/// `needs_session`; otherwise it fails. `result_commit` is the commit if
/// it was verified; `reason`, when given, becomes `last_error`.
pub fn reject(
    mut run: TaskRun,
    result_commit: Option<CommitSha>,
    reason: Option<String>,
    resumable: bool,
) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Validating], "reject")?;
    run.status = if resumable {
        RunStatus::NeedsSession
    } else {
        RunStatus::Failed
    };
    run.result_commit = result_commit;
    if reason.is_some() {
        run.last_error = reason;
    }
    Ok(run)
}

/// `awaiting_integration` → `validating`: the live session rewrote its
/// receipt for a `revise` verdict (ADR-0027 decision 2).
pub fn restart_validation(mut run: TaskRun) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::AwaitingIntegration], "validate again")?;
    run.status = RunStatus::Validating;
    Ok(run)
}

/// A person's `send_back` (`needs_session`) or `cancel` (`failed`) answer
/// to an `approve_landing` ask, with `reason` as `last_error`.
pub fn decide_landing(
    mut run: TaskRun,
    to: RunStatus,
    reason: String,
) -> Result<TaskRun, DomainError> {
    require_status(
        &run,
        &[RunStatus::AwaitingIntegration],
        "decide the landing of",
    )?;
    require(
        matches!(to, RunStatus::NeedsSession | RunStatus::Failed),
        || DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "decide a landing other than a send-back or a cancel for",
        },
    )?;
    run.status = to;
    run.last_error = Some(reason);
    Ok(run)
}

/// A runtime error on the run: `message` becomes `last_error` and the
/// status stays, whether or not its supervisor lets it go.
pub fn abandon(mut run: TaskRun, message: String) -> Result<TaskRun, DomainError> {
    run.last_error = Some(message);
    Ok(run)
}

/// Take back a run whose processes are gone: an executing run becomes
/// `interrupted`, one left mid-integration goes back to
/// `awaiting_integration`, since its validated result is intact.
pub fn interrupt(mut run: TaskRun) -> Result<TaskRun, DomainError> {
    require_status(&run, &UNFINISHED, "recover")?;
    run.status = if run.status == RunStatus::Integrating {
        RunStatus::AwaitingIntegration
    } else {
        RunStatus::Interrupted
    };
    Ok(run)
}

/// A run awaiting integration, or one back from a session, takes the
/// integration slot.
pub fn begin_integration(mut run: TaskRun) -> Result<TaskRun, DomainError> {
    require_status(
        &run,
        &[RunStatus::AwaitingIntegration, RunStatus::NeedsSession],
        "integrate",
    )?;
    run.status = RunStatus::Integrating;
    Ok(run)
}

/// Leave the integration slot for `to` with `reason` as `last_error`.
fn leave_integration(
    mut run: TaskRun,
    to: RunStatus,
    reason: String,
    operation: &'static str,
) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Integrating], operation)?;
    run.status = to;
    run.last_error = Some(reason);
    Ok(run)
}

/// The landing needs a session (a conflict or a failed re-validation):
/// `integrating` → `needs_session`.
pub fn defer_integration(run: TaskRun, reason: String) -> Result<TaskRun, DomainError> {
    leave_integration(
        run,
        RunStatus::NeedsSession,
        reason,
        "defer the integration of",
    )
}

/// The rewritten receipt reports `failed`: `integrating` → `failed`.
pub fn fail_integration(run: TaskRun, reason: String) -> Result<TaskRun, DomainError> {
    leave_integration(run, RunStatus::Failed, reason, "fail the integration of")
}

/// An error before `main` moved: back to `revert_to`, the status the run
/// had when the landing started.
pub fn abort_integration(
    run: TaskRun,
    revert_to: RunStatus,
    message: String,
) -> Result<TaskRun, DomainError> {
    require(
        matches!(
            revert_to,
            RunStatus::AwaitingIntegration | RunStatus::NeedsSession
        ),
        || DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "return to a status it could not have integrated from",
        },
    )?;
    leave_integration(run, revert_to, message, "abort the integration of")
}

/// The run landed as `commit`: `integrating` → `integrated`, and the error
/// of any earlier attempt is cleared.
pub fn finish_integration(mut run: TaskRun, commit: CommitSha) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::Integrating], "finish the integration of")?;
    run.status = RunStatus::Integrated;
    run.result_commit = Some(commit);
    run.last_error = None;
    Ok(run)
}

/// Whether the run may be leased for a resume: it is waiting for a session.
pub fn check_resumable(run: &TaskRun) -> Result<(), DomainError> {
    require_status(run, &[RunStatus::NeedsSession], "resume")
}

/// A resume is skipped because an earlier attempt already resolved the run:
/// an `approved` one stays `needs_session` for its landing, any other is
/// validated again.
pub fn skip_resume(mut run: TaskRun, approved: bool) -> Result<TaskRun, DomainError> {
    check_resumable(&run)?;
    if !approved {
        run.status = RunStatus::Validating;
    }
    Ok(run)
}

/// A resumed session ended with the run `to` (`validating`,
/// `awaiting_integration` or `failed`); `reason`, when given, becomes `last_error`.
pub fn finish_resume(
    mut run: TaskRun,
    to: RunStatus,
    reason: Option<String>,
) -> Result<TaskRun, DomainError> {
    require_status(&run, &[RunStatus::NeedsSession], "finish the resume of")?;
    require(
        matches!(
            to,
            RunStatus::Validating | RunStatus::AwaitingIntegration | RunStatus::Failed
        ),
        || DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "end a resume in a status a session cannot reach for",
        },
    )?;
    run.status = to;
    if reason.is_some() {
        run.last_error = reason;
    }
    Ok(run)
}

/// Resumes are used up: `needs_session` → `failed` with `reason`, for a
/// person to decide (ADR-0024).
pub fn exhaust_resumes(mut run: TaskRun, reason: String) -> Result<TaskRun, DomainError> {
    check_resumable(&run)?;
    run.status = RunStatus::Failed;
    run.last_error = Some(reason);
    Ok(run)
}

/// Only a `failed` or `interrupted` run is triaged.
pub fn check_triageable(run: &TaskRun) -> Result<(), DomainError> {
    require_status(run, &[RunStatus::Failed, RunStatus::Interrupted], "triage")
}

/// The triage (or a person's answer to it) resumes the run:
/// `needs_session` with `instruction` as `last_error`.
pub fn resume_after_triage(mut run: TaskRun, instruction: String) -> Result<TaskRun, DomainError> {
    check_triageable(&run)?;
    run.status = RunStatus::NeedsSession;
    run.last_error = Some(instruction);
    Ok(run)
}

/// cmux confirmed the close of the workspace of a run that came to rest
/// (awaiting integration or a session) at `closed_at`.
pub fn workspace_closed(mut run: TaskRun, closed_at: i64) -> Result<TaskRun, DomainError> {
    require_open_workspace_at_rest(&run, "close the workspace of")?;
    require(run.workspace_id.is_some(), || {
        DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation: "close the workspace it never had of",
        }
    })?;
    run.workspace_closed_at = Some(closed_at);
    Ok(run)
}

/// The triage closed `workspace_id` at `closed_at`: the run's own workspace
/// is recorded as closed; a resume's leaves the run as it is.
pub fn triage_closed_workspace(
    mut run: TaskRun,
    workspace_id: &str,
    closed_at: i64,
) -> Result<TaskRun, DomainError> {
    if run.workspace_id.as_deref() == Some(workspace_id) && run.workspace_closed_at.is_none() {
        run.workspace_closed_at = Some(closed_at);
    }
    Ok(run)
}

/// The workspace of a run at rest could not be closed: `message` becomes
/// `last_error`, and the workspace stays open (never treated as cleaned).
pub fn record_close_failure(mut run: TaskRun, message: String) -> Result<TaskRun, DomainError> {
    require_open_workspace_at_rest(&run, "record a failed close of the workspace of")?;
    run.last_error = Some(message);
    Ok(run)
}

fn require_open_workspace_at_rest(
    run: &TaskRun,
    operation: &'static str,
) -> Result<(), DomainError> {
    require_status(
        run,
        &[RunStatus::AwaitingIntegration, RunStatus::NeedsSession],
        operation,
    )?;
    require(run.workspace_closed_at.is_none(), || {
        DomainError::RunTransitionNotAllowed {
            status: run.status,
            operation,
        }
    })
}

/// A cleanup after the landing (worktree or branch removal) failed:
/// `message` becomes `last_error` of a run whose status no longer changes.
pub fn record_cleanup_failure(mut run: TaskRun, message: String) -> Result<TaskRun, DomainError> {
    run.last_error = Some(message);
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewTask, task};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn record(status: RunStatus) -> RunRecord {
        RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(3),
            status,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: CommitSha::parse(SHA, "base commit").unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: "c".into(),
        }
    }

    fn run(status: RunStatus) -> TaskRun {
        TaskRun::restore(record(status)).unwrap()
    }

    fn sha() -> CommitSha {
        CommitSha::parse(SHA, "commit").unwrap()
    }

    fn plan() -> RunPlan {
        RunPlan {
            repo_path: "/repo".into(),
            run_dir: "/runs/r1".into(),
            branch: "dagq/r1".into(),
            worktree_path: "/runs/r1/worktree".into(),
            receipt_path: "/runs/r1/receipt.json".into(),
            log_path: "/runs/r1/claude.debug.log".into(),
        }
    }

    fn refused(result: Result<TaskRun, DomainError>, status: RunStatus) -> &'static str {
        match result.unwrap_err() {
            DomainError::RunTransitionNotAllowed {
                status: found,
                operation,
            } => {
                assert_eq!(found, status);
                operation
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn new_starts_a_claimed_run_of_a_claimed_task() {
        let ready = crate::domain::Task::new(
            TaskId::new(3),
            NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
            },
            "now".into(),
        )
        .unwrap();
        let base = CommitSha::parse(SHA.to_ascii_uppercase(), "base commit").unwrap();
        let error = TaskRun::new(
            RunId::new("r1").unwrap(),
            &ready,
            &base,
            Provider::Claude,
            "t0".into(),
        )
        .unwrap_err();
        assert!(matches!(error, DomainError::RunOfUnclaimedTask { .. }));
        assert!(error.to_string().starts_with("task 3 is "));
        let ready = task::transition(ready, task::TaskAction::Ready, false).unwrap();
        let claimed = task::claim(ready).unwrap();
        let run = TaskRun::new(
            RunId::new("r1").unwrap(),
            &claimed,
            &base,
            Provider::Claude,
            "t0".into(),
        )
        .unwrap();
        assert_eq!(run.status(), RunStatus::Claimed);
        assert_eq!(run.task_id(), TaskId::new(3));
        assert_eq!(run.base_commit().as_str(), SHA);
        assert_eq!(run.created_at(), "t0");
        assert_eq!(run.requested_provider(), Provider::Claude);
        assert_eq!(run.actual_provider(), Provider::Claude);
        assert!(run.run_dir().is_none() && run.last_error().is_none());
    }

    #[test]
    fn restore_checks_the_stored_invariants() {
        let error = TaskRun::restore(record(RunStatus::Integrated)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "run r1 is integrated without a result commit"
        );
        let mut integrated = record(RunStatus::Integrated);
        integrated.result_commit = Some(sha());
        assert_eq!(
            TaskRun::restore(integrated).unwrap().result_commit(),
            Some(&sha())
        );
        // An older version's row is restored as it is.
        let mut legacy = record(RunStatus::AwaitingIntegration);
        legacy.workspace_closed_at = Some(1);
        assert_eq!(
            TaskRun::restore(legacy).unwrap().workspace_closed_at(),
            Some(1)
        );
    }

    #[test]
    fn a_run_goes_from_claim_to_integration() {
        let run = start_provisioning(run(RunStatus::Claimed), &plan()).unwrap();
        assert_eq!(run.status(), RunStatus::Starting);
        assert_eq!(run.branch(), Some("dagq/r1"));
        assert_eq!(run.repo_path(), Some("/repo"));
        assert_eq!(run.worktree_path(), Some("/runs/r1/worktree"));
        assert_eq!(run.receipt_path(), Some("/runs/r1/receipt.json"));
        assert_eq!(run.log_path(), Some("/runs/r1/claude.debug.log"));
        assert_eq!(
            refused(
                check_ready_for_wrapper(&run).map(|()| run.clone()),
                RunStatus::Starting
            ),
            "start the wrapper without a workspace of"
        );
        let run = attach_workspace(run, "ws".into()).unwrap();
        assert_eq!(run.workspace_id(), Some("ws"));
        check_ready_for_wrapper(&run).unwrap();
        assert_eq!(
            refused(
                attach_workspace(run.clone(), "ws2".into()),
                RunStatus::Starting
            ),
            "attach a second workspace to"
        );
        let run = mark_running(run).unwrap();
        assert_eq!(run.status(), RunStatus::Running);
        let run = finish_session(run, Some(0)).unwrap();
        assert_eq!(run.status(), RunStatus::Validating);
        let run = accept(run, sha()).unwrap();
        assert_eq!(run.status(), RunStatus::AwaitingIntegration);
        let run = restart_validation(run).unwrap();
        let run = accept(run, sha()).unwrap();
        let run = workspace_closed(run, 7).unwrap();
        assert_eq!(run.workspace_closed_at(), Some(7));
        let run = begin_integration(run).unwrap();
        assert_eq!(run.status(), RunStatus::Integrating);
        let run = abandon(run, "boom".into()).unwrap();
        let run = finish_integration(run, sha()).unwrap();
        assert_eq!(run.status(), RunStatus::Integrated);
        assert_eq!(run.last_error(), None);
        let run = record_cleanup_failure(run, "rm failed".into()).unwrap();
        assert_eq!(run.last_error(), Some("rm failed"));
        assert_eq!(
            refused(begin_integration(run), RunStatus::Integrated),
            "integrate"
        );
    }

    #[test]
    fn transitions_refuse_other_statuses() {
        assert_eq!(
            refused(
                start_provisioning(run(RunStatus::Running), &plan()),
                RunStatus::Running
            ),
            "provision"
        );
        assert_eq!(
            refused(mark_running(run(RunStatus::Claimed)), RunStatus::Claimed),
            "mark running"
        );
        let error = finish_session(run(RunStatus::Validating), Some(0)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot finish the session of a run in validating state"
        );
        refused(accept(run(RunStatus::Running), sha()), RunStatus::Running);
        refused(
            reject(run(RunStatus::Running), None, None, false),
            RunStatus::Running,
        );
        refused(
            restart_validation(run(RunStatus::Validating)),
            RunStatus::Validating,
        );
        refused(
            decide_landing(run(RunStatus::Failed), RunStatus::Failed, "r".into()),
            RunStatus::Failed,
        );
        refused(
            decide_landing(
                run(RunStatus::AwaitingIntegration),
                RunStatus::Integrated,
                "r".into(),
            ),
            RunStatus::AwaitingIntegration,
        );
        refused(interrupt(run(RunStatus::Failed)), RunStatus::Failed);
        refused(
            defer_integration(run(RunStatus::AwaitingIntegration), "r".into()),
            RunStatus::AwaitingIntegration,
        );
        refused(
            abort_integration(run(RunStatus::Integrating), RunStatus::Failed, "m".into()),
            RunStatus::Integrating,
        );
        refused(
            finish_integration(run(RunStatus::NeedsSession), sha()),
            RunStatus::NeedsSession,
        );
        refused(skip_resume(run(RunStatus::Failed), true), RunStatus::Failed);
        refused(
            finish_resume(run(RunStatus::NeedsSession), RunStatus::Integrated, None),
            RunStatus::NeedsSession,
        );
        refused(
            exhaust_resumes(run(RunStatus::Validating), "r".into()),
            RunStatus::Validating,
        );
        refused(
            resume_after_triage(run(RunStatus::NeedsSession), "i".into()),
            RunStatus::NeedsSession,
        );
        refused(
            workspace_closed(run(RunStatus::AwaitingIntegration), 1),
            RunStatus::AwaitingIntegration,
        );
        refused(
            record_close_failure(run(RunStatus::Running), "m".into()),
            RunStatus::Running,
        );
    }

    #[test]
    fn session_and_validation_outcomes() {
        let failed = finish_session(run(RunStatus::Running), Some(2)).unwrap();
        assert_eq!(failed.status(), RunStatus::Failed);
        assert_eq!(failed.last_error(), Some("session exited with code 2"));
        let live = finish_session(run(RunStatus::Starting), None).unwrap();
        assert_eq!(live.status(), RunStatus::Validating);
        let kept = abandon(run(RunStatus::Validating), "old".into()).unwrap();
        let parked = reject(kept.clone(), Some(sha()), None, true).unwrap();
        assert_eq!(parked.status(), RunStatus::NeedsSession);
        assert_eq!(parked.last_error(), Some("old"));
        assert_eq!(parked.result_commit(), Some(&sha()));
        let rejected = reject(kept, None, Some("bad".into()), false).unwrap();
        assert_eq!(rejected.status(), RunStatus::Failed);
        assert_eq!(rejected.last_error(), Some("bad"));
        let sent_back = decide_landing(
            run(RunStatus::AwaitingIntegration),
            RunStatus::NeedsSession,
            "why".into(),
        )
        .unwrap();
        assert_eq!(sent_back.status(), RunStatus::NeedsSession);
        assert_eq!(sent_back.last_error(), Some("why"));
    }

    #[test]
    fn recovery_integration_and_resume() {
        assert_eq!(
            interrupt(run(RunStatus::Running)).unwrap().status(),
            RunStatus::Interrupted
        );
        assert_eq!(
            interrupt(run(RunStatus::Integrating)).unwrap().status(),
            RunStatus::AwaitingIntegration
        );
        let deferred = defer_integration(run(RunStatus::Integrating), "conflict".into()).unwrap();
        assert_eq!(deferred.status(), RunStatus::NeedsSession);
        assert_eq!(deferred.last_error(), Some("conflict"));
        let failed = fail_integration(run(RunStatus::Integrating), "f".into()).unwrap();
        assert_eq!(failed.status(), RunStatus::Failed);
        let aborted = abort_integration(
            run(RunStatus::Integrating),
            RunStatus::NeedsSession,
            "m".into(),
        )
        .unwrap();
        assert_eq!(aborted.status(), RunStatus::NeedsSession);
        assert_eq!(aborted.last_error(), Some("m"));
        let back = begin_integration(run(RunStatus::NeedsSession)).unwrap();
        assert_eq!(back.status(), RunStatus::Integrating);

        check_resumable(&run(RunStatus::NeedsSession)).unwrap();
        assert_eq!(
            skip_resume(run(RunStatus::NeedsSession), true)
                .unwrap()
                .status(),
            RunStatus::NeedsSession
        );
        assert_eq!(
            skip_resume(run(RunStatus::NeedsSession), false)
                .unwrap()
                .status(),
            RunStatus::Validating
        );
        let resolved = finish_resume(
            abandon(run(RunStatus::NeedsSession), "old".into()).unwrap(),
            RunStatus::AwaitingIntegration,
            None,
        )
        .unwrap();
        assert_eq!(resolved.status(), RunStatus::AwaitingIntegration);
        assert_eq!(resolved.last_error(), Some("old"));
        let failed = finish_resume(
            run(RunStatus::NeedsSession),
            RunStatus::Failed,
            Some("r".into()),
        )
        .unwrap();
        assert_eq!(failed.last_error(), Some("r"));
        let exhausted = exhaust_resumes(run(RunStatus::NeedsSession), "used up".into()).unwrap();
        assert_eq!(exhausted.status(), RunStatus::Failed);
        assert_eq!(exhausted.last_error(), Some("used up"));
    }

    #[test]
    fn triage_and_workspace_close() {
        check_triageable(&run(RunStatus::Interrupted)).unwrap();
        let resumed = resume_after_triage(run(RunStatus::Failed), "fix it".into()).unwrap();
        assert_eq!(resumed.status(), RunStatus::NeedsSession);
        assert_eq!(resumed.last_error(), Some("fix it"));

        let mut open = record(RunStatus::Failed);
        open.workspace_id = Some("ws".into());
        let open = TaskRun::restore(open).unwrap();
        let other = triage_closed_workspace(open.clone(), "resume-ws", 5).unwrap();
        assert_eq!(other.workspace_closed_at(), None);
        let closed = triage_closed_workspace(open, "ws", 5).unwrap();
        assert_eq!(closed.workspace_closed_at(), Some(5));
        let again = triage_closed_workspace(closed, "ws", 9).unwrap();
        assert_eq!(again.workspace_closed_at(), Some(5));

        let mut resting = record(RunStatus::NeedsSession);
        resting.workspace_id = Some("ws".into());
        let resting = TaskRun::restore(resting).unwrap();
        let noted = record_close_failure(resting.clone(), "close failed".into()).unwrap();
        assert_eq!(noted.last_error(), Some("close failed"));
        let closed = workspace_closed(resting, 3).unwrap();
        refused(workspace_closed(closed.clone(), 4), RunStatus::NeedsSession);
        refused(
            record_close_failure(closed, "m".into()),
            RunStatus::NeedsSession,
        );
    }

    #[test]
    fn idle_marker_and_relocation() {
        assert_eq!(
            run(RunStatus::Failed).idle_marker_path().unwrap_err(),
            DomainError::MissingRunDirectory
        );
        let run = start_provisioning(run(RunStatus::Claimed), &plan()).unwrap();
        let moved = run.relocated(std::path::Path::new("/moved"));
        assert_eq!(moved.run_dir(), Some("/moved/r1"));
        assert_eq!(moved.repo_path(), Some("/repo"));
        assert_eq!(
            moved.idle_marker_path().unwrap(),
            std::path::Path::new("/moved/r1/idle.json")
        );
    }
}
