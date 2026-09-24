//! A run of a task. It moved here unchanged; its encapsulation is a later
//! step of the layering (ADR-0013).

use serde::{Deserialize, Serialize};

use super::{CommitSha, DomainError, Provider, RunId, RunStatus, TaskId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
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
