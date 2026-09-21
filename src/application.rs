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
    /// Dependency-ready tasks; an occupied execution slot is reported by claim.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// Reserve one run atomically. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &str) -> Result<ClaimOutcome>;
}
