//! Application-facing storage contract. Provider/process adapters come next.

use anyhow::Result;

use crate::domain::{
    ClaimOutcome, Goal, GoalDetail, GoalEdit, GoalSummary, GoalVerdict, NewGoal, NewTask,
    Predecessor, Task, TaskAction, TaskDetail,
};

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
    /// Direct predecessors of a task, each with the run that landed it, in ID order.
    fn predecessors(&self, task_id: i64) -> Result<Vec<Predecessor>>;
    /// Tasks that are `in_progress` right now, in ID order.
    fn tasks_in_progress(&self) -> Result<Vec<Task>>;
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal>;
    /// Every goal in ID order with its task counts by status.
    fn list_goals(&self) -> Result<Vec<GoalSummary>>;
    fn show_goal(&mut self, goal_id: i64) -> Result<GoalDetail>;
    /// Replace the given fields; running runs keep their prompt snapshot.
    fn edit_goal(&mut self, goal_id: i64, edit: GoalEdit) -> Result<Goal>;
    /// Record the verdict once. `achieved` is refused while a task is not
    /// completed or canceled; `abandoned` while a task is in progress.
    fn close_goal(&mut self, goal_id: i64, verdict: GoalVerdict) -> Result<Goal>;
    /// Move a draft or ready task to an open goal, or to none.
    fn set_goal(&mut self, task_id: i64, goal_id: Option<i64>) -> Result<Task>;
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
