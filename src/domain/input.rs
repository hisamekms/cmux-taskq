//! Input types: what a caller asks to create or change, and the stored
//! state a store hands back to restore an aggregate. They are plain data
//! with public fields; the aggregates in [`super::task`], [`super::goal`]
//! and [`super::run`] apply the rules when built from them.

use serde::{Deserialize, Serialize};

use super::{
    CommitSha, DomainError, EvidenceCheck, GoalId, GoalStatus, GoalVerdict, Priority, Provider,
    RunId, RunStatus, TaskId, TaskStatus, require, scope,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub dependencies: Vec<TaskId>,
    /// Goals the task waits for until each is closed as achieved
    /// (ADR-0038); never its own goal.
    #[serde(default)]
    pub goal_dependencies: Vec<GoalId>,
    /// Goal the task belongs to; must be open at registration.
    pub goal_id: Option<GoalId>,
    /// Why the task exists and what to read first; carried into the prompt.
    pub context: String,
    /// Receipt checks validation requires to be `passed` with evidence.
    #[serde(default)]
    pub required_evidence: Vec<EvidenceCheck>,
    /// Globs of the paths the task may change (ADR-0029); empty: no limit.
    #[serde(default)]
    pub paths: Vec<String>,
    /// How urgently the task should be claimed (ADR-0040 decision 4).
    #[serde(default)]
    pub priority: Priority,
}

impl NewTask {
    /// The required checks in the order given, each once.
    pub fn required_evidence(&self) -> Vec<EvidenceCheck> {
        let mut checks = Vec::new();
        for check in &self.required_evidence {
            if !checks.contains(check) {
                checks.push(*check);
            }
        }
        checks
    }

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
        require(self.dependencies.iter().all(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId {
                field: "dependency IDs",
            }
        })?;
        require(self.goal_id.is_none_or(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "goal ID" }
        })?;
        require(
            self.goal_dependencies.iter().all(|id| id.as_i64() > 0),
            || DomainError::NonPositiveId {
                field: "goal dependency IDs",
            },
        )?;
        if let Some(goal_id) = self.goal_id
            && self.goal_dependencies.contains(&goal_id)
        {
            return Err(DomainError::OwnGoalDependency { goal_id });
        }
        scope::validate_path_globs(&self.paths)
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
}

/// Fields of a draft task to replace (`dagq edit`, ADR-0041 decision 9);
/// `None` keeps the current value. The lists replace the whole list: an
/// empty one removes every command, check or glob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub verification_commands: Option<Vec<String>>,
    pub required_evidence: Option<Vec<EvidenceCheck>>,
    pub paths: Option<Vec<String>>,
    pub context: Option<String>,
}

impl TaskEdit {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.acceptance.is_none()
            && self.verification_commands.is_none()
            && self.required_evidence.is_none()
            && self.paths.is_none()
            && self.context.is_none()
    }

    /// The rules of [`NewTask::validate`] for the fields it replaces.
    pub fn validate(&self) -> Result<(), DomainError> {
        if let Some(title) = &self.title {
            require(!title.trim().is_empty(), || DomainError::Blank {
                field: "task title",
            })?;
        }
        if let Some(commands) = &self.verification_commands {
            require(commands.iter().all(|s| !s.trim().is_empty()), || {
                DomainError::Blank {
                    field: "verification commands",
                }
            })?;
        }
        match &self.paths {
            Some(paths) => scope::validate_path_globs(paths),
            None => Ok(()),
        }
    }
}

/// A task as the store saved it, for [`super::Task::restore`].
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub required_evidence: Vec<EvidenceCheck>,
    pub paths: Vec<String>,
    pub priority: Priority,
    pub status: TaskStatus,
    pub goal_id: Option<GoalId>,
    pub context: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A goal as the store saved it, for [`super::Goal::restore`].
#[derive(Debug, Clone)]
pub struct GoalRecord {
    pub id: GoalId,
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

/// A run as the store saved it, for [`super::TaskRun::restore`].
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub requested_provider: Provider,
    pub actual_provider: Provider,
    pub base_commit: CommitSha,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub workspace_id: Option<String>,
    pub receipt_path: Option<String>,
    pub log_path: Option<String>,
    pub result_commit: Option<CommitSha>,
    pub repo_path: Option<String>,
    pub run_dir: Option<String>,
    pub last_error: Option<String>,
    pub workspace_closed_at: Option<i64>,
    pub created_at: String,
}

/// Where a claimed run is provisioned: the repository, its run directory,
/// branch and worktree, and the files its session writes. `run_planned`
/// records it as is.
#[derive(Debug, Clone, Serialize)]
pub struct RunPlan {
    pub repo_path: String,
    pub run_dir: String,
    pub branch: String,
    pub worktree_path: String,
    pub receipt_path: String,
    pub log_path: String,
}
