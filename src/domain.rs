use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $($value => Ok(Self::$variant)),+,
                    _ => bail!("unknown {}: {value}", stringify!($name)),
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

string_enum!(ReceiptResult {
    Succeeded => "succeeded",
    Failed => "failed",
});

string_enum!(CheckStatus {
    Passed => "passed",
    Failed => "failed",
    NotApplicable => "not_applicable",
});

/// User operations cannot mark a task in progress or completed.
#[derive(Debug, Clone, Copy)]
pub enum TaskAction {
    Ready,
    Draft,
    Cancel,
}

impl TaskStatus {
    /// `unfinished_run` is whether the task still owns a run that is executing,
    /// awaiting or undergoing integration, or waiting for a session. An in-progress task whose runs have all failed or
    /// been interrupted may be retried or canceled by hand; a retry is a new run.
    pub fn transition(self, action: TaskAction, unfinished_run: bool) -> Result<Self> {
        match (self, action) {
            (Self::Draft, TaskAction::Ready) => Ok(Self::Ready),
            (Self::Ready, TaskAction::Draft) => Ok(Self::Draft),
            (Self::Draft | Self::Ready, TaskAction::Cancel) => Ok(Self::Canceled),
            (Self::InProgress, _) if unfinished_run => bail!(
                "task has an unfinished run; recover or integrate it before applying {action:?}"
            ),
            (Self::InProgress, TaskAction::Ready) => Ok(Self::Ready),
            (Self::InProgress, TaskAction::Draft) => Ok(Self::Draft),
            (Self::InProgress, TaskAction::Cancel) => Ok(Self::Canceled),
            _ => bail!("cannot apply {action:?} to task in {} state", self.as_str()),
        }
    }

    pub fn dependencies_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Ready)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub dependencies: Vec<i64>,
}

impl NewTask {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.title.trim().is_empty(),
            "task title must not be blank"
        );
        ensure!(
            self.verification_commands
                .iter()
                .all(|s| !s.trim().is_empty()),
            "verification commands must not be blank"
        );
        ensure!(
            self.dependencies.iter().all(|id| *id > 0),
            "dependency IDs must be positive"
        );
        Ok(())
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
    pub created_at: String,
    pub updated_at: String,
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

impl TaskRun {
    /// Written by the provider's stop hook each time the agent finishes a
    /// response; newer than the receipt means the session is idle after submitting.
    pub fn idle_marker_path(&self) -> Result<std::path::PathBuf> {
        Ok(
            std::path::Path::new(self.run_dir.as_ref().context("missing run directory")?)
                .join("idle.json"),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: i64,
    pub task_id: i64,
    pub run_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLease {
    pub run_id: String,
    pub pid: u32,
    pub heartbeat_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    Claimed { run: Box<TaskRun> },
    NoReadyTask,
}

/// Result of one `integrate` invocation. `Integrated` landed the run on
/// `main` (`run.result_commit` is the landed commit). `NeedsSession` parked
/// the run for a session to resolve; `Failed` ended it because its rewritten
/// receipt reported `failed`. `NoRunAwaiting` is `--next` on an empty queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum IntegrationOutcome {
    Integrated {
        task: Task,
        run: Box<TaskRun>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptCheck {
    pub status: CheckStatus,
    #[serde(default)]
    pub evidence_or_reason: String,
}

impl Receipt {
    pub fn parse(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("receipt is not a valid completion receipt")
    }

    /// Structural consistency only; Git state and verification commands are checked by the supervisor.
    pub fn check(&self, run_id: &str) -> Result<()> {
        ensure!(
            self.run_id == run_id,
            "receipt run_id {} does not match run {run_id}",
            self.run_id
        );
        ensure!(
            self.result == ReceiptResult::Succeeded,
            "agent reported result {}: {}",
            self.result.as_str(),
            self.summary
        );
        for (name, check) in [
            ("tests", &self.tests),
            ("e2e", &self.e2e),
            ("subagent_review", &self.subagent_review),
        ] {
            ensure!(
                check.status != CheckStatus::Failed,
                "receipt reports {name} as failed: {}",
                check.evidence_or_reason
            );
            ensure!(
                !check.evidence_or_reason.trim().is_empty(),
                "receipt {name} is {} without evidence or reason",
                check.status.as_str()
            );
        }
        validate_commit(&self.commit).context("receipt commit")?;
        Ok(())
    }
}

pub fn validate_base_commit(commit: &str) -> Result<()> {
    validate_commit(commit).context("base commit")
}

fn validate_commit(commit: &str) -> Result<()> {
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "must be a full 40- or 64-character hexadecimal Git object ID"
    );
    Ok(())
}
