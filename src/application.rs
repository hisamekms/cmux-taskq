//! Application-facing storage contract. Provider/process adapters come next.

use anyhow::Result;

use crate::domain::{ClaimOutcome, NewTask, Task, TaskAction, TaskDetail};

pub trait TaskQueue {
    fn add(&mut self, task: NewTask) -> Result<Task>;
    fn list(&self) -> Result<Vec<Task>>;
    fn show(&mut self, task_id: i64) -> Result<TaskDetail>;
    fn transition(&mut self, task_id: i64, action: TaskAction) -> Result<Task>;
    fn add_dependency(&mut self, task_id: i64, predecessor_id: i64) -> Result<()>;
    fn remove_dependency(&mut self, task_id: i64, predecessor_id: i64) -> Result<()>;
    /// Dependency-ready tasks; each task is limited to one unfinished run.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// Reserve one run atomically, without a lease. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &str) -> Result<ClaimOutcome>;
}

/// Provider-specific CLI construction is kept outside supervisor orchestration.
pub trait AgentProvider {
    fn preflight(&self) -> Result<()>;
    fn command(&self, run: &crate::domain::TaskRun, prompt: &str) -> Result<std::process::Command>;
}

pub trait WorkspaceBackend {
    fn preflight(&self) -> Result<()>;
    fn create(&self, run: &crate::domain::TaskRun, command: &str) -> Result<String>;
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way an operator would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// How long the session may take to exit after the request before the
    /// supervisor stops waiting and leaves the run to a human.
    fn exit_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
}
