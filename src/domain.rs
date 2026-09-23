use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = DomainError;
            fn from_str(value: &str) -> Result<Self, DomainError> {
                match value {
                    $($value => Ok(Self::$variant)),+,
                    _ => Err(DomainError::UnknownValue {
                        kind: stringify!($name),
                        value: value.to_owned(),
                    }),
                }
            }
        }
    };
}

string_enum!(TaskStatus {
    Draft => "draft",
    Ready => "ready",
    InProgress => "in_progress",
    Completed => "completed",
    Canceled => "canceled",
});

string_enum!(RunStatus {
    Claimed => "claimed",
    Starting => "starting",
    Running => "running",
    Validating => "validating",
    AwaitingIntegration => "awaiting_integration",
    Integrating => "integrating",
    NeedsSession => "needs_session",
    Integrated => "integrated",
    Succeeded => "succeeded",
    Failed => "failed",
    Interrupted => "interrupted",
});

string_enum!(Provider { Claude => "claude" });

// How `up` started a supervisor (ADR-0011). `Launchd` is the resident
// LaunchAgent; `InCmux` is the fallback that runs `supervise` inside the cmux
// workspace `[<repo>]supervisor`, which nothing restarts. A registration
// without a mode was started by hand.
string_enum!(SupervisorMode {
    Launchd => "launchd",
    InCmux => "in_cmux",
});

// The part a cmux workspace plays for a queue, carried in its `DAGQ_ROLE`
// environment variable and its description (ADR-0026). `Planner` and `Inbox`
// are named here for the sessions `up` is to open later; no workspace of
// theirs exists yet. `Observer` is the periodic job of ADR-0024: it has no
// workspace, and the CLI refuses queue changes from its environment.
string_enum!(SessionRole {
    Maintainer => "maintainer",
    Supervisor => "supervisor",
    Worker => "worker",
    Planner => "planner",
    Inbox => "inbox",
    Observer => "observer",
});

// Whether a goal's tasks may run (ADR-0024 decision 5). A `draft` goal is a
// proposal, typically the observer's: its tasks are not candidates until
// `goal ready` opens it. Existing goals are `open`. Closing is independent
// and recorded in the verdict.
string_enum!(GoalStatus {
    Draft => "draft",
    Open => "open",
});

// How a goal was closed. Apart from draft/open, a goal has no state machine:
// it is open until one close records the verdict, and its progress derives
// from its tasks.
string_enum!(GoalVerdict {
    Achieved => "achieved",
    Abandoned => "abandoned",
});

string_enum!(ReceiptResult {
    Succeeded => "succeeded",
    Failed => "failed",
});

string_enum!(CheckStatus {
    Passed => "passed",
    Failed => "failed",
    NotApplicable => "not_applicable",
});

/// A business rejection by the domain: an invalid value, a transition the
/// task's status does not allow, or a condition that does not hold. Each
/// variant carries only what its message needs, and `Display` is the message
/// the CLI prints and the runtime writes to `last_error`. I/O failures are not
/// domain errors; the layers that perform I/O convert this at their boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A stored or given string is not a value of the enum `kind`.
    UnknownValue {
        kind: &'static str,
        value: String,
    },
    /// A manual transition of an in-progress task that still owns an unfinished run.
    TaskHasUnfinishedRun {
        action: TaskAction,
    },
    /// `action` is not allowed from `status`.
    TransitionNotAllowed {
        status: TaskStatus,
        action: TaskAction,
    },
    /// A required text field is blank.
    Blank {
        field: &'static str,
    },
    /// An ID field is zero or negative.
    NonPositiveId {
        field: &'static str,
    },
    /// A goal records its verdict once.
    GoalAlreadyClosed {
        goal_id: i64,
        verdict: Option<GoalVerdict>,
    },
    /// Tasks in `blocking` (status and count) do not allow `verdict`.
    GoalCloseBlocked {
        goal_id: i64,
        verdict: GoalVerdict,
        blocking: Vec<(TaskStatus, usize)>,
    },
    /// The receipt text is not a completion receipt; `reason` is the parser's.
    MalformedReceipt {
        reason: String,
    },
    ReceiptRunMismatch {
        receipt_run_id: String,
        run_id: String,
    },
    /// The agent itself reported the run as not succeeded.
    AgentReportedResult {
        result: ReceiptResult,
        summary: String,
    },
    /// The receipt reports the check `check` as failed.
    ReceiptCheckFailed {
        check: &'static str,
        evidence_or_reason: String,
    },
    /// The receipt claims `status` for `check` without evidence or reason.
    ReceiptCheckUnexplained {
        check: &'static str,
        status: CheckStatus,
    },
    /// `field` is not a full Git object ID.
    InvalidCommit {
        field: &'static str,
    },
    FollowUpsNotArray,
    /// The run has no run directory yet.
    MissingRunDirectory,
    /// `goal ready` on a goal that is not a draft.
    GoalNotDraft {
        goal_id: i64,
    },
    /// A note kind that is not a lowercase slug.
    InvalidNoteKind {
        kind: String,
    },
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownValue { kind, value } => write!(f, "unknown {kind}: {value}"),
            Self::TaskHasUnfinishedRun { action } => write!(
                f,
                "task has an unfinished run; recover or integrate it before applying {action:?}"
            ),
            Self::TransitionNotAllowed { status, action } => write!(
                f,
                "cannot apply {action:?} to task in {} state",
                status.as_str()
            ),
            Self::Blank { field } => write!(f, "{field} must not be blank"),
            Self::NonPositiveId { field } => write!(f, "{field} must be positive"),
            Self::GoalAlreadyClosed { goal_id, verdict } => write!(
                f,
                "goal {goal_id} is already closed as {}",
                verdict.map_or("?", GoalVerdict::as_str)
            ),
            Self::GoalCloseBlocked {
                goal_id,
                verdict,
                blocking,
            } => write!(
                f,
                "goal {goal_id} cannot be closed as {}: {}",
                verdict.as_str(),
                blocking
                    .iter()
                    .map(|(status, n)| format!("{n} task(s) {}", status.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::MalformedReceipt { reason } => {
                write!(f, "receipt is not a valid completion receipt: {reason}")
            }
            Self::ReceiptRunMismatch {
                receipt_run_id,
                run_id,
            } => write!(
                f,
                "receipt run_id {receipt_run_id} does not match run {run_id}"
            ),
            Self::AgentReportedResult { result, summary } => {
                write!(f, "agent reported result {}: {summary}", result.as_str())
            }
            Self::ReceiptCheckFailed {
                check,
                evidence_or_reason,
            } => write!(f, "receipt reports {check} as failed: {evidence_or_reason}"),
            Self::ReceiptCheckUnexplained { check, status } => write!(
                f,
                "receipt {check} is {} without evidence or reason",
                status.as_str()
            ),
            Self::InvalidCommit { field } => write!(
                f,
                "{field}: must be a full 40- or 64-character hexadecimal Git object ID"
            ),
            Self::FollowUpsNotArray => f.write_str("receipt follow_ups must be an array"),
            Self::MissingRunDirectory => f.write_str("missing run directory"),
            Self::GoalNotDraft { goal_id } => write!(f, "goal {goal_id} is not a draft"),
            Self::InvalidNoteKind { kind } => write!(
                f,
                "note kind {kind:?} must be a slug of lowercase letters, digits, '-' and '_'"
            ),
        }
    }
}

impl std::error::Error for DomainError {}

/// Fails with `error()` unless `condition` holds.
fn require(condition: bool, error: impl FnOnce() -> DomainError) -> Result<(), DomainError> {
    if condition { Ok(()) } else { Err(error()) }
}

/// User operations cannot mark a task in progress or completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    Ready,
    Draft,
    Cancel,
}

impl TaskStatus {
    /// `unfinished_run` is whether the task still owns a run that is executing,
    /// awaiting or undergoing integration, or waiting for a session. An in-progress task whose runs have all failed or
    /// been interrupted may be retried or canceled by hand; a retry is a new run.
    pub fn transition(self, action: TaskAction, unfinished_run: bool) -> Result<Self, DomainError> {
        match (self, action) {
            (Self::Draft, TaskAction::Ready) => Ok(Self::Ready),
            (Self::Ready, TaskAction::Draft) => Ok(Self::Draft),
            (Self::Draft | Self::Ready, TaskAction::Cancel) => Ok(Self::Canceled),
            (Self::InProgress, _) if unfinished_run => {
                Err(DomainError::TaskHasUnfinishedRun { action })
            }
            (Self::InProgress, TaskAction::Ready) => Ok(Self::Ready),
            (Self::InProgress, TaskAction::Draft) => Ok(Self::Draft),
            (Self::InProgress, TaskAction::Cancel) => Ok(Self::Canceled),
            _ => Err(DomainError::TransitionNotAllowed {
                status: self,
                action,
            }),
        }
    }

    /// Dependencies and the goal may change only before the task is claimed.
    pub fn dependencies_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Ready)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

impl GoalVerdict {
    /// `achieved` needs every task finished; `abandoned` only needs nothing
    /// executing, so unstarted tasks stay in the queue as they are.
    pub fn allows(self, task: TaskStatus) -> bool {
        match self {
            Self::Achieved => task.is_terminal(),
            Self::Abandoned => task != TaskStatus::InProgress,
        }
    }

    /// Whether `goal`, whose tasks number `counts` by status, may be closed
    /// with this verdict. A goal is closed once; the rejection names the
    /// statuses that do not allow the verdict.
    pub fn check_close(self, goal: &Goal, counts: &TaskStatusCounts) -> Result<(), DomainError> {
        require(!goal.is_closed(), || DomainError::GoalAlreadyClosed {
            goal_id: goal.id,
            verdict: goal.verdict,
        })?;
        let blocking: Vec<(TaskStatus, usize)> = [
            (TaskStatus::Draft, counts.draft),
            (TaskStatus::Ready, counts.ready),
            (TaskStatus::InProgress, counts.in_progress),
        ]
        .into_iter()
        .filter(|(status, n)| *n > 0 && !self.allows(*status))
        .collect();
        require(blocking.is_empty(), || DomainError::GoalCloseBlocked {
            goal_id: goal.id,
            verdict: self,
            blocking,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub dependencies: Vec<i64>,
    /// Goal the task belongs to; must be open at registration.
    pub goal_id: Option<i64>,
    /// Why the task exists and what to read first; carried into the prompt.
    pub context: String,
}

impl NewTask {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.title.trim().is_empty(), || DomainError::Blank {
            field: "task title",
        })?;
        require(
            self.verification_commands
                .iter()
                .all(|s| !s.trim().is_empty()),
            || DomainError::Blank {
                field: "verification commands",
            },
        )?;
        require(self.dependencies.iter().all(|id| *id > 0), || {
            DomainError::NonPositiveId {
                field: "dependency IDs",
            }
        })?;
        require(self.goal_id.is_none_or(|id| id > 0), || {
            DomainError::NonPositiveId { field: "goal ID" }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub status: TaskStatus,
    pub goal_id: Option<i64>,
    pub context: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A higher-level problem that several tasks solve together (ADR-0009). The
/// description, acceptance and constraints align the judgement of sibling
/// tasks; `doc` is a path inside the repository. There are no verification
/// commands: machine checks belong to a task that depends on the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub constraints: String,
    pub doc: Option<String>,
    pub status: GoalStatus,
    pub closed_at: Option<String>,
    pub verdict: Option<GoalVerdict>,
    pub created_at: String,
    pub updated_at: String,
}

impl Goal {
    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }

    /// An unclosed draft: the only goal `goal ready` opens.
    pub fn is_draft(&self) -> bool {
        self.status == GoalStatus::Draft && !self.is_closed()
    }

    /// `goal ready` takes a draft that is not closed.
    pub fn check_ready(&self) -> Result<(), DomainError> {
        require(!self.is_closed(), || DomainError::GoalAlreadyClosed {
            goal_id: self.id,
            verdict: self.verdict,
        })?;
        require(self.status == GoalStatus::Draft, || {
            DomainError::GoalNotDraft { goal_id: self.id }
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewGoal {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub constraints: String,
    pub doc: Option<String>,
    /// Register the goal as a draft whose tasks are not candidates.
    pub draft: bool,
}

impl NewGoal {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.title.trim().is_empty(), || GOAL_TITLE_BLANK)
    }
}

const GOAL_TITLE_BLANK: DomainError = DomainError::Blank {
    field: "goal title",
};

/// Fields of a goal to replace; `None` keeps the current value. An empty
/// `doc` clears the reference.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub constraints: Option<String>,
    pub doc: Option<String>,
}

impl GoalEdit {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.acceptance.is_none()
            && self.constraints.is_none()
            && self.doc.is_none()
    }

    /// The goal as it would be after this edit.
    pub fn apply(&self, goal: &Goal) -> Result<Goal, DomainError> {
        let mut next = goal.clone();
        if let Some(title) = &self.title {
            require(!title.trim().is_empty(), || GOAL_TITLE_BLANK)?;
            next.title = title.clone();
        }
        if let Some(description) = &self.description {
            next.description = description.clone();
        }
        if let Some(acceptance) = &self.acceptance {
            next.acceptance = acceptance.clone();
        }
        if let Some(constraints) = &self.constraints {
            next.constraints = constraints.clone();
        }
        if let Some(doc) = &self.doc {
            next.doc = Some(doc.clone()).filter(|d| !d.trim().is_empty());
        }
        Ok(next)
    }
}

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
    pub id: i64,
    pub title: String,
    pub status: GoalStatus,
    pub closed: bool,
    pub verdict: Option<GoalVerdict>,
    pub tasks: TaskStatusCounts,
}

/// A task as `goal show` lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTask {
    pub id: i64,
    pub title: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalDetail {
    pub goal: Goal,
    pub closed: bool,
    pub tasks: Vec<GoalTask>,
    pub events: Vec<RunEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub task_id: i64,
    pub status: RunStatus,
    pub requested_provider: Provider,
    pub actual_provider: Provider,
    pub base_commit: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub workspace_id: Option<String>,
    pub receipt_path: Option<String>,
    pub log_path: Option<String>,
    pub result_commit: Option<String>,
    pub repo_path: Option<String>,
    pub run_dir: Option<String>,
    pub last_error: Option<String>,
    /// Set once cmux confirmed the close; null keeps the run out of any cleaned state.
    pub workspace_closed_at: Option<i64>,
    pub created_at: String,
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
    pub fn new(runs_dir: &std::path::Path, run_id: &str) -> Self {
        let run_dir = runs_dir.join(run_id);
        Self {
            worktree: run_dir.join("worktree"),
            receipt: run_dir.join("receipt.json"),
            log: run_dir.join("claude.debug.log"),
            run_dir,
        }
    }
}

impl TaskRun {
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

/// A direct dependency of a task as the worker's prompt describes it: the
/// predecessor and the run that landed it on `main`. A claimed task's
/// predecessors are all completed, so the run is absent only when the task
/// was completed by hand or its integrated run is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Predecessor {
    pub task: Task,
    pub integrated_run: Option<TaskRun>,
}

pub mod stats;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: i64,
    /// Absent only for goal-level events (`goal_created`, `goal_updated`, `goal_closed`).
    pub task_id: Option<i64>,
    pub goal_id: Option<i64>,
    pub run_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

/// Run event kind of a note (ADR-0024 decision 4): a free-form observation
/// attached to a task, a run or a goal, with payload `{text, kind, by}`.
pub const OBSERVATION_KIND: &str = "observation";
/// `kind` of a note registered without one.
pub const DEFAULT_NOTE_KIND: &str = "note";

/// What a note is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteTarget {
    Task(i64),
    Run(String),
    Goal(i64),
}

/// A note to record as an `observation` run event.
#[derive(Debug, Clone)]
pub struct NewNote {
    pub target: NoteTarget,
    pub text: String,
    /// A lowercase slug classifying the note; [`DEFAULT_NOTE_KIND`] when absent.
    pub kind: Option<String>,
    /// `DAGQ_ROLE` of the writer, or `human`.
    pub by: String,
}

impl NewNote {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.text.trim().is_empty(), || DomainError::Blank {
            field: "note text",
        })?;
        if let Some(kind) = &self.kind {
            require(
                !kind.is_empty()
                    && kind.len() <= 64
                    && kind.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                    }),
                || DomainError::InvalidNoteKind { kind: kind.clone() },
            )?;
        }
        Ok(())
    }

    /// The payload of the `observation` event.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "text": self.text,
            "kind": self.kind.as_deref().unwrap_or(DEFAULT_NOTE_KIND),
            "by": self.by,
        })
    }
}

/// Which notes `notes` lists: past `since` (oldest first), or the latest
/// `limit` without it, narrowed to a goal (its own notes and those of its
/// tasks and their runs) and/or a task (its own and its runs').
#[derive(Debug, Clone, Default)]
pub struct NoteQuery {
    pub goal_id: Option<i64>,
    pub task_id: Option<i64>,
    pub since: Option<i64>,
    pub limit: usize,
}

/// One page of `notes`, oldest first; `cursor` is the last note's event id
/// (or `since` when the page is empty), to pass back as `--since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotePage {
    pub notes: Vec<RunEvent>,
    pub cursor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub dependencies: Vec<i64>,
    pub runs: Vec<TaskRun>,
    pub events: Vec<RunEvent>,
    pub processes: Vec<RunProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProcess {
    pub run_id: String,
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
    pub run_id: String,
    pub token: String,
    pub pid: u32,
    pub heartbeat_at: i64,
}

/// A resident `supervise` process as it registered itself, whether or not it
/// holds any lease. The row is heartbeated with the leases and deleted on a
/// graceful exit; a row left by a killed supervisor stays until `up` prunes
/// it or the maintainer deals with it. `mode`, `workspace_id` and
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    Claimed { run: Box<TaskRun> },
    NoReadyTask,
}

/// Result of one `integrate` invocation. `Integrated` landed the run on
/// `main` (`run.result_commit` is the landed commit); its
/// `verification_skipped` is true when the rebase was a no-op on the
/// validated head and the landing reused the validation's run of the
/// verification commands.
/// `NeedsSession` parked the run for a session to resolve; `Failed` ended it
/// because its rewritten receipt reported `failed`. `NoRunAwaiting` is
/// `--next` on an empty queue. `Integrated` also reports the push of the
/// landed `main` (ADR-0019 decision 3); a failed push leaves the landing as it is.
/// Its `follow_ups` are the draft tasks registered from the landed receipt's
/// `follow_ups` (ADR-0019 decision 4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum IntegrationOutcome {
    Integrated {
        task: Task,
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
        main: String,
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
    pub task_id: i64,
    pub title: String,
}

/// The remote `integrate` pushes the landed `main` to (ADR-0019 decision 3).
pub const PUSH_REMOTE: &str = "origin";

string_enum!(PushResult {
    Pushed => "pushed",
    Skipped => "skipped",
    Failed => "failed",
});

/// What became of the push after a landing: `pushed` (`push_finished`),
/// `skipped` with its `reason` (`--no-push` or no such remote,
/// `push_skipped`) or `failed` with Git's `error` (`push_failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushReport {
    pub outcome: PushResult,
    pub remote: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Default for PushReport {
    fn default() -> Self {
        Self {
            outcome: PushResult::Skipped,
            remote: PUSH_REMOTE.to_owned(),
            error: None,
            reason: None,
        }
    }
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
    pub fn check(&self, run_id: &str) -> Result<(), DomainError> {
        require(self.run_id == run_id, || DomainError::ReceiptRunMismatch {
            receipt_run_id: self.run_id.clone(),
            run_id: run_id.to_owned(),
        })?;
        require(self.result == ReceiptResult::Succeeded, || {
            DomainError::AgentReportedResult {
                result: self.result,
                summary: self.summary.clone(),
            }
        })?;
        for (name, check) in [
            ("tests", &self.tests),
            ("e2e", &self.e2e),
            ("subagent_review", &self.subagent_review),
        ] {
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
        validate_commit(&self.commit, "receipt commit")?;
        require(
            self.follow_ups.as_ref().is_none_or(|f| f.is_array()),
            || DomainError::FollowUpsNotArray,
        )
    }
}

pub fn validate_base_commit(commit: &str) -> Result<(), DomainError> {
    validate_commit(commit, "base commit")
}

fn validate_commit(commit: &str, field: &'static str) -> Result<(), DomainError> {
    require(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()),
        || DomainError::InvalidCommit { field },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(verdict: Option<GoalVerdict>) -> Goal {
        Goal {
            id: 7,
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            status: GoalStatus::Open,
            closed_at: verdict.map(|_| "2026-09-23T00:00:00Z".into()),
            verdict,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn only_an_unclosed_draft_goal_becomes_ready() {
        let draft = Goal {
            status: GoalStatus::Draft,
            ..goal(None)
        };
        assert!(draft.is_draft());
        draft.check_ready().unwrap();
        assert_eq!(
            goal(None).check_ready().unwrap_err().to_string(),
            "goal 7 is not a draft"
        );
        let closed = Goal {
            status: GoalStatus::Draft,
            ..goal(Some(GoalVerdict::Abandoned))
        };
        assert!(!closed.is_draft());
        assert_eq!(
            closed.check_ready().unwrap_err().to_string(),
            "goal 7 is already closed as abandoned"
        );
    }

    #[test]
    fn a_note_needs_text_and_a_slug_kind() {
        let note = |text: &str, kind: Option<&str>| NewNote {
            target: NoteTarget::Goal(1),
            text: text.into(),
            kind: kind.map(Into::into),
            by: "human".into(),
        };
        note("x", None).validate().unwrap();
        note("x", Some("slow-land_2")).validate().unwrap();
        assert_eq!(
            note(" ", None).validate().unwrap_err().to_string(),
            "note text must not be blank"
        );
        for kind in ["", "Upper", "a b", &"k".repeat(65)] {
            assert!(
                matches!(
                    note("x", Some(kind)).validate(),
                    Err(DomainError::InvalidNoteKind { .. })
                ),
                "{kind}"
            );
        }
        assert_eq!(
            note("x", None).payload(),
            serde_json::json!({"text": "x", "kind": "note", "by": "human"})
        );
    }

    #[test]
    fn rejections_carry_their_facts_and_keep_the_cli_messages() {
        assert_eq!(
            "bogus".parse::<TaskStatus>(),
            Err(DomainError::UnknownValue {
                kind: "TaskStatus",
                value: "bogus".into()
            })
        );
        assert_eq!(
            "x".parse::<GoalVerdict>().unwrap_err().to_string(),
            "unknown GoalVerdict: x"
        );
        let error = TaskStatus::Completed
            .transition(TaskAction::Ready, false)
            .unwrap_err();
        assert_eq!(
            error,
            DomainError::TransitionNotAllowed {
                status: TaskStatus::Completed,
                action: TaskAction::Ready
            }
        );
        assert_eq!(
            error.to_string(),
            "cannot apply Ready to task in completed state"
        );
        assert_eq!(
            TaskStatus::InProgress
                .transition(TaskAction::Cancel, true)
                .unwrap_err()
                .to_string(),
            "task has an unfinished run; recover or integrate it before applying Cancel"
        );
        let task = NewTask {
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            dependencies: vec![0],
            goal_id: None,
            context: String::new(),
        };
        assert_eq!(
            task.validate().unwrap_err().to_string(),
            "dependency IDs must be positive"
        );
        let edit = GoalEdit {
            title: Some(" ".into()),
            ..GoalEdit::default()
        };
        assert_eq!(
            edit.apply(&goal(None)).unwrap_err().to_string(),
            "goal title must not be blank"
        );
        assert_eq!(
            validate_base_commit("abc").unwrap_err().to_string(),
            "base commit: must be a full 40- or 64-character hexadecimal Git object ID"
        );
        assert!(
            Receipt::parse("{}")
                .unwrap_err()
                .to_string()
                .starts_with("receipt is not a valid completion receipt: ")
        );
    }

    #[test]
    fn a_goal_closes_once_and_only_when_its_tasks_allow_the_verdict() {
        let mut counts = TaskStatusCounts::default();
        counts.count(TaskStatus::Ready, 2);
        counts.count(TaskStatus::InProgress, 1);
        assert_eq!(
            GoalVerdict::Achieved
                .check_close(&goal(None), &counts)
                .unwrap_err()
                .to_string(),
            "goal 7 cannot be closed as achieved: 2 task(s) ready, 1 task(s) in_progress"
        );
        assert_eq!(
            GoalVerdict::Abandoned
                .check_close(&goal(None), &counts)
                .unwrap_err(),
            DomainError::GoalCloseBlocked {
                goal_id: 7,
                verdict: GoalVerdict::Abandoned,
                blocking: vec![(TaskStatus::InProgress, 1)]
            }
        );
        counts.in_progress = 0;
        GoalVerdict::Abandoned
            .check_close(&goal(None), &counts)
            .unwrap();
        assert_eq!(
            GoalVerdict::Abandoned
                .check_close(&goal(Some(GoalVerdict::Achieved)), &counts)
                .unwrap_err()
                .to_string(),
            "goal 7 is already closed as achieved"
        );
    }
}

/// A process whose heartbeat is older than this has no working process behind
/// it, whatever its PID says: the rule for leases, wrappers and supervisors.
pub const HEARTBEAT_TIMEOUT_SECS: i64 = 30;

/// What the maintainer (or the user) does about an attention (ADR-0016). The
/// values are short fixed phrases, part of the public contract of `status`,
/// `events` and `watch`; only `answer the prompt in workspace <id>` carries
/// the workspace the dialog is open in (ADR-0019).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionNext {
    ReviewAndIntegrate,
    ResumeSession,
    InspectAndClose,
    SendExit,
    RestartSupervisor,
    PushMain,
    RecoverRun,
    AnswerPrompt { workspace_id: String },
}

impl fmt::Display for AttentionNext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReviewAndIntegrate => f.write_str("review and integrate"),
            Self::ResumeSession => f.write_str("resume session"),
            Self::InspectAndClose => f.write_str("inspect and close workspace"),
            Self::SendExit => f.write_str("send /exit"),
            Self::RestartSupervisor => f.write_str("restart supervisor"),
            Self::PushMain => f.write_str("push main"),
            Self::RecoverRun => f.write_str("recover run"),
            Self::AnswerPrompt { workspace_id } => {
                write!(f, "answer the prompt in workspace {workspace_id}")
            }
        }
    }
}

impl Serialize for AttentionNext {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The `run_events` kinds that can mark an attention. The kind names are a
/// public contract (ADR-0016); whether one of these events is an attention
/// also depends on its payload, see [`event_attention`].
pub const ATTENTION_KINDS: &[&str] = &[
    "validation_finished",
    "supervision_finished",
    "integration_deferred",
    "integration_failed",
    "integration_error",
    "exit_request_timed_out",
    "push_failed",
    "runtime_error",
    "prompt_waiting",
];

/// Whether a run event is a transition that stops at the maintainer's or the
/// user's judgment, and what to do about it. The run comes to rest in
/// `status` (`awaiting_integration`, `needs_session`, `failed`), or the
/// session did not answer `/exit`. `integration_error` back to
/// `awaiting_integration` is not one: the `integrate` caller got the error.
/// `integration_rebase_aborted` is not one either: the landing goes on and
/// its outcome is its own event. A `runtime_error` is one only when the
/// supervisor released the run's lease with it (`lease_released: true`, the
/// abandon): nothing moves the run on until it is recovered. A
/// `runtime_error` recorded without releasing the lease is a note.
/// `prompt_waiting` is one: the session waits at a dialog in the payload's
/// `workspace_id`.
pub fn event_attention(kind: &str, payload: &serde_json::Value) -> Option<AttentionNext> {
    let status = payload
        .get("status")
        .and_then(serde_json::Value::as_str)
        .and_then(|status| status.parse::<RunStatus>().ok());
    match (kind, status) {
        ("validation_finished", Some(RunStatus::AwaitingIntegration)) => {
            Some(AttentionNext::ReviewAndIntegrate)
        }
        (
            "validation_finished" | "supervision_finished" | "integration_failed",
            Some(RunStatus::Failed),
        ) => Some(AttentionNext::InspectAndClose),
        ("integration_deferred" | "integration_error", Some(RunStatus::NeedsSession)) => {
            Some(AttentionNext::ResumeSession)
        }
        ("exit_request_timed_out", _) => Some(AttentionNext::SendExit),
        ("push_failed", _) => Some(AttentionNext::PushMain),
        ("runtime_error", _)
            if payload.get("lease_released") == Some(&serde_json::Value::Bool(true)) =>
        {
            Some(AttentionNext::RecoverRun)
        }
        ("prompt_waiting", _) => Some(answer_prompt(
            payload
                .get("workspace_id")
                .and_then(serde_json::Value::as_str),
        )),
        _ => None,
    }
}

fn answer_prompt(workspace_id: Option<&str>) -> AttentionNext {
    AttentionNext::AnswerPrompt {
        workspace_id: workspace_id.unwrap_or("?").to_owned(),
    }
}

/// Whether a run in `status` waits for the maintainer now. `exit_pending` is
/// a `running` run whose `/exit` request timed out with no session exit
/// since. `push_pending` is the `integrated` run whose push of `main`
/// failed with no successful push since, which the task being completed does
/// not end. `leased` is whether the run has a lease row, stale or not: an
/// unfinished run without one was given up by its owner (the supervisor's
/// abandon), and neither adoption, which takes only stale leases, nor
/// anything else moves it on until it is recovered. A stale lease is the
/// supervisor's attention, not the run's. `prompt_waiting` is the workspace
/// of a `running` run whose latest `prompt_waiting` has no `prompt_cleared`
/// or `receipt_observed` after it (`Some("?")` when the payload named none).
/// The caller passes only the latest run of an `in_progress` task, so a
/// failed run stops counting once the task is retried or canceled.
pub fn run_attention(
    status: RunStatus,
    exit_pending: bool,
    push_pending: bool,
    leased: bool,
    prompt_waiting: Option<&str>,
) -> Option<AttentionNext> {
    match status {
        RunStatus::Integrated if push_pending => Some(AttentionNext::PushMain),
        RunStatus::Claimed
        | RunStatus::Starting
        | RunStatus::Running
        | RunStatus::Validating
        | RunStatus::Integrating
            if !leased =>
        {
            Some(AttentionNext::RecoverRun)
        }
        RunStatus::AwaitingIntegration => Some(AttentionNext::ReviewAndIntegrate),
        RunStatus::NeedsSession => Some(AttentionNext::ResumeSession),
        RunStatus::Failed => Some(AttentionNext::InspectAndClose),
        RunStatus::Running if exit_pending => Some(AttentionNext::SendExit),
        RunStatus::Running if prompt_waiting.is_some() => Some(answer_prompt(prompt_waiting)),
        _ => None,
    }
}

/// One thing that waits for the maintainer or the user: a run (`run_id`,
/// `task_id`) or a supervisor (`pid`, or neither when none is registered).
/// `kind` is the run event that brought the run there, or
/// `supervisor_stale` / `supervisor_stopped`, which are derived from the
/// `supervisors` table and never written to `run_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attention {
    pub run_id: Option<String>,
    pub task_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub status: String,
    pub kind: String,
    pub last_error: Option<String>,
    pub next: AttentionNext,
}

/// Whether a process no longer works: its PID is dead or its heartbeat is
/// older than [`HEARTBEAT_TIMEOUT_SECS`].
pub fn heartbeat_stale(alive: bool, heartbeat_age_secs: i64) -> bool {
    !alive || heartbeat_age_secs > HEARTBEAT_TIMEOUT_SECS
}

/// The health of one registered supervisor that `watch` compares: a change
/// in the set of tokens, a PID, `alive` or `stale` wakes the maintainer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupervisorPulse {
    pub token: String,
    pub pid: u32,
    pub alive: bool,
    pub stale: bool,
}

impl SupervisorPulse {
    pub fn judge(registration: &SupervisorRegistration, alive: bool, now: i64) -> Self {
        Self {
            token: registration.token.clone(),
            pid: registration.pid,
            alive,
            stale: heartbeat_stale(alive, now - registration.heartbeat_at),
        }
    }
}

/// Supervisors that need a restart: every stale registration, or a queue
/// with no registration at all (stopped, or never started).
pub fn supervisor_attention(pulses: &[SupervisorPulse]) -> Vec<Attention> {
    let restart = |pid, status: &str, kind: &str| Attention {
        run_id: None,
        task_id: None,
        pid,
        status: status.into(),
        kind: kind.into(),
        last_error: None,
        next: AttentionNext::RestartSupervisor,
    };
    if pulses.is_empty() {
        return vec![restart(None, "stopped", "supervisor_stopped")];
    }
    pulses
        .iter()
        .filter(|pulse| pulse.stale)
        .map(|pulse| {
            let status = if pulse.alive { "stale" } else { "dead" };
            restart(Some(pulse.pid), status, "supervisor_stale")
        })
        .collect()
}

#[cfg(test)]
mod attention_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_attention_covers_every_kind_by_its_status() {
        use AttentionNext::*;
        let cases = [
            (
                "validation_finished",
                json!({"status": "awaiting_integration"}),
                Some(ReviewAndIntegrate),
            ),
            (
                "validation_finished",
                json!({"status": "failed", "reason": "x"}),
                Some(InspectAndClose),
            ),
            (
                "supervision_finished",
                json!({"status": "failed", "exit_code": 1}),
                Some(InspectAndClose),
            ),
            (
                "supervision_finished",
                json!({"status": "validating", "exit_code": 0}),
                None,
            ),
            (
                "integration_deferred",
                json!({"status": "needs_session", "reason": "x"}),
                Some(ResumeSession),
            ),
            (
                "integration_failed",
                json!({"status": "failed", "reason": "x"}),
                Some(InspectAndClose),
            ),
            (
                "integration_error",
                json!({"status": "needs_session", "reason": "x"}),
                Some(ResumeSession),
            ),
            (
                "integration_error",
                json!({"status": "awaiting_integration"}),
                None,
            ),
            (
                "exit_request_timed_out",
                json!({"workspace_id": "w", "timeout_secs": 120}),
                Some(SendExit),
            ),
            (
                "push_failed",
                json!({"remote": "origin", "commit": "c", "error": "x"}),
                Some(PushMain),
            ),
            (
                "push_finished",
                json!({"remote": "origin", "commit": "c"}),
                None,
            ),
            (
                "push_skipped",
                json!({"remote": "origin", "reason": "x"}),
                None,
            ),
            (
                "runtime_error",
                json!({"message": "x", "lease_released": true}),
                Some(RecoverRun),
            ),
            (
                "runtime_error",
                json!({"message": "x", "lease_released": false}),
                None,
            ),
            ("runtime_error", json!({"message": "x"}), None),
            (
                "prompt_waiting",
                json!({"workspace_id": "w", "excerpt": "x", "screen_hash": "h"}),
                Some(AnswerPrompt {
                    workspace_id: "w".into(),
                }),
            ),
            ("prompt_cleared", json!({"workspace_id": "w"}), None),
            ("integration_rebase_aborted", json!({"reason": "x"}), None),
            ("run_integrated", json!({"result_commit": "x"}), None),
            (
                "lease_released",
                json!({"reason": "integration_failed"}),
                None,
            ),
            ("validation_finished", json!({"status": "bogus"}), None),
            ("validation_finished", json!({}), None),
        ];
        for (kind, payload, expected) in cases {
            assert_eq!(
                event_attention(kind, &payload),
                expected,
                "{kind} {payload}"
            );
            if expected.is_some() {
                assert!(ATTENTION_KINDS.contains(&kind), "{kind}");
            }
        }
        assert_eq!(SendExit.to_string(), "send /exit");
        assert_eq!(RecoverRun.to_string(), "recover run");
        assert_eq!(PushMain.to_string(), "push main");
        assert_eq!(
            serde_json::to_value(RestartSupervisor).unwrap(),
            json!("restart supervisor")
        );
        assert_eq!(
            serde_json::to_value(AnswerPrompt {
                workspace_id: "w".into()
            })
            .unwrap(),
            json!("answer the prompt in workspace w")
        );
        assert_eq!(
            event_attention("prompt_waiting", &json!({})),
            Some(AnswerPrompt {
                workspace_id: "?".into()
            })
        );
    }

    #[test]
    fn run_attention_follows_the_resting_status() {
        use AttentionNext::*;
        assert_eq!(
            run_attention(RunStatus::AwaitingIntegration, false, false, false, None),
            Some(ReviewAndIntegrate)
        );
        assert_eq!(
            run_attention(RunStatus::NeedsSession, false, false, false, None),
            Some(ResumeSession)
        );
        assert_eq!(
            run_attention(RunStatus::Failed, false, false, false, None),
            Some(InspectAndClose)
        );
        assert_eq!(
            run_attention(RunStatus::Running, true, false, true, None),
            Some(SendExit)
        );
        assert_eq!(
            run_attention(RunStatus::Running, false, false, true, None),
            None
        );
        assert_eq!(
            run_attention(RunStatus::Running, false, false, true, Some("w")),
            Some(AnswerPrompt {
                workspace_id: "w".into()
            })
        );
        assert_eq!(
            run_attention(RunStatus::Running, true, false, true, Some("w")),
            Some(SendExit)
        );
        // An abandoned run is recovered before any dialog is answered.
        assert_eq!(
            run_attention(RunStatus::Running, false, false, false, Some("w")),
            Some(RecoverRun)
        );
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Validating,
            RunStatus::Integrating,
            RunStatus::Integrated,
            RunStatus::Succeeded,
            RunStatus::Interrupted,
        ] {
            assert_eq!(
                run_attention(status, true, false, true, Some("w")),
                None,
                "{}",
                status.as_str()
            );
        }
        assert_eq!(
            run_attention(RunStatus::Integrated, false, true, false, None),
            Some(PushMain)
        );
        assert_eq!(
            run_attention(RunStatus::Succeeded, false, true, false, None),
            None
        );
    }

    #[test]
    fn run_attention_asks_to_recover_an_unfinished_run_without_a_lease() {
        use AttentionNext::*;
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Validating,
            RunStatus::Integrating,
        ] {
            assert_eq!(
                run_attention(status, false, false, false, None),
                Some(RecoverRun),
                "{}",
                status.as_str()
            );
        }
        // Nothing moves an abandoned run, so `/exit` alone would not do.
        assert_eq!(
            run_attention(RunStatus::Running, true, false, false, None),
            Some(RecoverRun)
        );
        for status in [
            RunStatus::Integrated,
            RunStatus::Succeeded,
            RunStatus::Interrupted,
        ] {
            assert_eq!(
                run_attention(status, false, false, false, None),
                None,
                "{}",
                status.as_str()
            );
        }
    }

    #[test]
    fn supervisor_attention_reports_stale_registrations_or_a_stopped_queue() {
        let registration = |token: &str, heartbeat_at| SupervisorRegistration {
            token: token.into(),
            pid: 7,
            parallel: 1,
            started_at: 0,
            heartbeat_at,
            mode: None,
            workspace_id: None,
            binary_version: None,
        };
        let fresh =
            SupervisorPulse::judge(&registration("a", 100), true, 100 + HEARTBEAT_TIMEOUT_SECS);
        let hung =
            SupervisorPulse::judge(&registration("b", 100), true, 101 + HEARTBEAT_TIMEOUT_SECS);
        let dead = SupervisorPulse::judge(&registration("c", 100), false, 100);
        assert!(!fresh.stale && hung.stale && dead.stale);
        assert_eq!(supervisor_attention(std::slice::from_ref(&fresh)), vec![]);
        let stale = supervisor_attention(&[fresh, hung, dead]);
        let summary: Vec<_> = stale
            .iter()
            .map(|a| (a.kind.as_str(), a.status.as_str(), a.pid))
            .collect();
        assert_eq!(
            summary,
            [
                ("supervisor_stale", "stale", Some(7)),
                ("supervisor_stale", "dead", Some(7))
            ]
        );
        assert!(
            stale
                .iter()
                .all(|a| a.next == AttentionNext::RestartSupervisor && a.run_id.is_none())
        );
        let stopped = supervisor_attention(&[]);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].kind, "supervisor_stopped");
        assert_eq!(stopped[0].pid, None);
        assert_eq!(
            serde_json::to_value(&stopped[0]).unwrap(),
            json!({
                "run_id": null, "task_id": null, "status": "stopped", "kind": "supervisor_stopped",
                "last_error": null, "next": "restart supervisor",
            })
        );
    }
}
