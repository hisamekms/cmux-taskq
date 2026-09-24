//! Read-only views the store assembles and the CLI prints, and the
//! outcomes and receipt that travel between the runtime's steps. They are
//! not aggregates, so their fields are public.

use serde::{Deserialize, Serialize};

use super::{
    CheckStatus, CommitSha, DomainError, EventId, EvidenceCheck, Goal, GoalId, GoalStatus,
    GoalVerdict, PushReport, ReceiptResult, RunId, SupervisorMode, Task, TaskId, TaskRun,
    TaskStatus, require,
};

/// Number of a goal's tasks in each status; progress is derived from these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusCounts {
    pub total: usize,
    pub draft: usize,
    pub ready: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub canceled: usize,
}

impl TaskStatusCounts {
    pub fn count(&mut self, status: TaskStatus, n: usize) {
        self.total += n;
        *match status {
            TaskStatus::Draft => &mut self.draft,
            TaskStatus::Ready => &mut self.ready,
            TaskStatus::InProgress => &mut self.in_progress,
            TaskStatus::Completed => &mut self.completed,
            TaskStatus::Canceled => &mut self.canceled,
        } += n;
    }
}

/// One row of `goal list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSummary {
    pub id: GoalId,
    pub title: String,
    pub status: GoalStatus,
    pub closed: bool,
    pub verdict: Option<GoalVerdict>,
    pub tasks: TaskStatusCounts,
}

/// A task as `goal show` lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTask {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalDetail {
    pub goal: Goal,
    pub closed: bool,
    pub tasks: Vec<GoalTask>,
    /// Unfinished tasks that depend on this goal (ADR-0038), ascending.
    pub dependents: Vec<GoalTask>,
    pub events: Vec<RunEvent>,
}

/// A direct dependency of a task as the worker's prompt describes it: the
/// predecessor and the run that landed it on `main`. A claimed task's
/// predecessors are all completed, so the run is absent only when the task
/// was completed by hand or its integrated run is gone.
#[derive(Debug, Clone, Serialize)]
pub struct Predecessor {
    pub task: Task,
    pub integrated_run: Option<TaskRun>,
}

/// A goal a task depends on (ADR-0038) as the worker's prompt describes it:
/// the goal and its completed tasks in ID order, each with the run that
/// landed it. A claimed task's goal dependencies are all closed as achieved.
#[derive(Debug, Clone, Serialize)]
pub struct GoalPredecessor {
    pub goal: Goal,
    pub tasks: Vec<Predecessor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: EventId,
    /// Absent only for goal-level events (`goal_created`, `goal_updated`, `goal_closed`).
    pub task_id: Option<TaskId>,
    pub goal_id: Option<GoalId>,
    pub run_id: Option<RunId>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub dependencies: Vec<TaskId>,
    /// Goals the task depends on (ADR-0038), ascending.
    pub goal_dependencies: Vec<GoalId>,
    pub runs: Vec<TaskRun>,
    pub events: Vec<RunEvent>,
    pub processes: Vec<RunProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProcess {
    pub run_id: RunId,
    pub role: String,
    pub pid: u32,
    pub heartbeat_at: i64,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// A supervisor's ownership of one executing run. The row exists while the
/// supervisor watches the run and heartbeats it; it is deleted when the run
/// comes to rest, when the supervisor gives the run up, or by `recover`.
/// `token` is the owning process's token, shared with its
/// [`SupervisorRegistration`] when the owner is a resident `supervise`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLease {
    pub run_id: RunId,
    pub token: String,
    pub pid: u32,
    pub heartbeat_at: i64,
}

/// A resident `supervise` process as it registered itself, whether or not it
/// holds any lease. The row is heartbeated with the leases and deleted on a
/// graceful exit; a row left by a killed supervisor stays until `up` prunes
/// it or a person deals with it (`down --force`). `mode`, `workspace_id` and
/// `binary_version` describe the process itself, so they share the row's
/// lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorRegistration {
    pub token: String,
    pub pid: u32,
    pub parallel: u32,
    pub started_at: i64,
    pub heartbeat_at: i64,
    /// Written by the `up` that started this process, once it registered;
    /// `None` for a supervisor started by hand.
    pub mode: Option<SupervisorMode>,
    /// The cmux workspace `supervise` runs in, in [`SupervisorMode::InCmux`]
    /// only; `down` closes it when the supervisor is gone.
    pub workspace_id: Option<String>,
    /// The `dagq` version of the process, written by that process
    /// itself when it registers. `None` is a supervisor that registered
    /// before the column existed; `up` treats it as a version that is not
    /// its own (ADR-0014).
    pub binary_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    Claimed { run: Box<TaskRun> },
    NoReadyTask,
}

/// Result of one `integrate` invocation. `Integrated` landed the run on
/// `main` (`run.result_commit` is the landed commit); its
/// `verification_skipped` is always false since the verification commands
/// run on every landing (ADR-0023 decision 1), and is kept for the output's
/// shape.
/// `NeedsSession` parked the run for a session to resolve; `Failed` ended it
/// because its rewritten receipt reported `failed`. `NoRunAwaiting` is
/// `--next` on an empty queue. `Integrated` also reports the push of the
/// landed `main` (ADR-0019 decision 3); a failed push leaves the landing as it is.
/// Its `follow_ups` are the draft tasks registered from the landed receipt's
/// `follow_ups` (ADR-0019 decision 4).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum IntegrationOutcome {
    Integrated {
        task: Box<Task>,
        run: Box<TaskRun>,
        #[serde(default)]
        verification_skipped: bool,
        #[serde(default)]
        push: Box<PushReport>,
        #[serde(default)]
        follow_ups: Vec<RegisteredFollowUp>,
    },
    NeedsSession {
        run: Box<TaskRun>,
        main: CommitSha,
        reason: String,
    },
    Failed {
        run: Box<TaskRun>,
        reason: String,
    },
    NoRunAwaiting,
}

/// A draft task `integrate` registered from one of the landed receipt's
/// `follow_ups` (ADR-0019 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredFollowUp {
    pub task_id: TaskId,
    pub title: String,
}

/// Completion receipt written by the agent. Its claims are cross-checked by
/// the supervisor; the receipt alone never marks a run successful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub run_id: String,
    pub result: ReceiptResult,
    pub commit: String,
    pub tests: ReceiptCheck,
    pub e2e: ReceiptCheck,
    pub subagent_review: ReceiptCheck,
    #[serde(default)]
    pub summary: String,
    /// Follow-up tasks the agent proposes, as `{"title", "description"}`
    /// objects. Only its shape (an array) is checked here; `integrate`
    /// registers each entry with a title as a draft task once the run lands,
    /// and a person decides whether it becomes ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_ups: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptCheck {
    pub status: CheckStatus,
    #[serde(default)]
    pub evidence_or_reason: String,
}

impl Receipt {
    pub fn parse(text: &str) -> Result<Self, DomainError> {
        serde_json::from_str(text).map_err(|error| DomainError::MalformedReceipt {
            reason: error.to_string(),
        })
    }

    /// Structural consistency only; Git state and verification commands are checked by the supervisor.
    pub fn check(&self, run_id: &RunId) -> Result<(), DomainError> {
        self.check_requiring(run_id, &[])
    }

    /// [`Self::check`], except that a `required` check reported `failed` or
    /// with a blank `evidence_or_reason` is left to
    /// [`Self::missing_evidence`]: the run then waits for a session instead
    /// of failing (ADR-0019 decision 5).
    pub fn check_requiring(
        &self,
        run_id: &RunId,
        required: &[EvidenceCheck],
    ) -> Result<(), DomainError> {
        require(self.run_id == run_id.as_str(), || {
            DomainError::ReceiptRunMismatch {
                receipt_run_id: self.run_id.clone(),
                run_id: run_id.clone(),
            }
        })?;
        require(self.result == ReceiptResult::Succeeded, || {
            DomainError::AgentReportedResult {
                result: self.result,
                summary: self.summary.clone(),
            }
        })?;
        for evidence in [
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
            EvidenceCheck::SubagentReview,
        ] {
            let name = evidence.as_str();
            let check = self.evidence(evidence);
            if required.contains(&evidence) {
                continue;
            }
            require(check.status != CheckStatus::Failed, || {
                DomainError::ReceiptCheckFailed {
                    check: name,
                    evidence_or_reason: check.evidence_or_reason.clone(),
                }
            })?;
            require(!check.evidence_or_reason.trim().is_empty(), || {
                DomainError::ReceiptCheckUnexplained {
                    check: name,
                    status: check.status,
                }
            })?;
        }
        CommitSha::parse(self.commit.as_str(), "receipt commit")?;
        require(
            self.follow_ups.as_ref().is_none_or(|f| f.is_array()),
            || DomainError::FollowUpsNotArray,
        )
    }
}

impl Receipt {
    fn evidence(&self, check: EvidenceCheck) -> &ReceiptCheck {
        match check {
            EvidenceCheck::Tests => &self.tests,
            EvidenceCheck::E2e => &self.e2e,
            EvidenceCheck::SubagentReview => &self.subagent_review,
        }
    }

    /// The `required` checks this receipt does not back: a status other
    /// than `passed`, or no evidence.
    pub fn missing_evidence(&self, required: &[EvidenceCheck]) -> Vec<EvidenceCheck> {
        required
            .iter()
            .copied()
            .filter(|check| {
                let claim = self.evidence(*check);
                claim.status != CheckStatus::Passed || claim.evidence_or_reason.trim().is_empty()
            })
            .collect()
    }
}

/// The `last_error` of a run parked for `missing` evidence, such as
/// `evidence missing: e2e`.
pub fn evidence_missing_reason(missing: &[EvidenceCheck]) -> String {
    let names: Vec<&str> = missing.iter().map(|c| c.as_str()).collect();
    format!("evidence missing: {}", names.join(", "))
}

/// Where a run's files live: `<runs dir>/<run id>/` holds the worktree, the
/// receipt and the provider log. The layout is fixed, so these paths are
/// derived from the run ID and the queue's current `runs/` directory rather
/// than trusted from the database (ADR-0017).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    pub run_dir: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub receipt: std::path::PathBuf,
    pub log: std::path::PathBuf,
}

impl RunPaths {
    pub fn new(runs_dir: &std::path::Path, run_id: &RunId) -> Self {
        let run_dir = runs_dir.join(run_id.as_str());
        Self {
            worktree: run_dir.join("worktree"),
            receipt: run_dir.join("receipt.json"),
            log: run_dir.join("claude.debug.log"),
            run_dir,
        }
    }
}
