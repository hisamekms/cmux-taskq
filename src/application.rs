//! Application-facing storage contract. Provider/process adapters come next.

use anyhow::Result;

use crate::domain::{
    ClaimOutcome, Goal, GoalDetail, GoalEdit, GoalSummary, GoalVerdict, NewGoal, NewTask,
    Predecessor, RunStatus, Task, TaskAction, TaskDetail, TaskStatus,
};

/// Which task statuses `list` returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StatusFilter {
    /// Every status except the terminal ones (completed, canceled).
    #[default]
    Open,
    /// Every status, terminal ones included.
    Any,
    /// Only these statuses (any of them).
    Only(Vec<TaskStatus>),
}

/// Filter and page of a task listing. Filters combine with AND; tasks come
/// newest first (ID descending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskQuery {
    pub status: StatusFilter,
    pub goal_id: Option<i64>,
    /// Page size; at least one.
    pub limit: usize,
    /// Start the page at this task ID: only tasks whose ID is at most this.
    /// The previous page's `next` is the first task of the following page.
    pub before: Option<i64>,
    /// Include description, acceptance, context, verification commands and timestamps.
    pub full: bool,
}

impl TaskQuery {
    pub const DEFAULT_LIMIT: usize = 20;
}

impl Default for TaskQuery {
    fn default() -> Self {
        Self {
            status: StatusFilter::Open,
            goal_id: None,
            limit: Self::DEFAULT_LIMIT,
            before: None,
            full: false,
        }
    }
}

/// One page of tasks. `next` is the ID of the first task past this page,
/// to pass as `before` for the following page, and null on the last one;
/// `total` counts every task the filter matches, regardless of the page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskPage {
    pub tasks: Vec<TaskListItem>,
    pub next: Option<i64>,
    pub total: usize,
}

/// A task as `list` shows it: what the maintainer decides on, plus the
/// long fields only with `full`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListItem {
    pub id: i64,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<i64>,
    /// IDs of the direct predecessors, ascending.
    pub dependencies: Vec<i64>,
    /// The most recently created run, if any.
    pub latest_run: Option<LatestRun>,
    #[serde(flatten)]
    pub details: Option<TaskListDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LatestRun {
    pub id: String,
    pub status: RunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListDetails {
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub context: String,
    pub created_at: String,
    pub updated_at: String,
}

impl TaskListItem {
    pub fn new(
        task: Task,
        dependencies: Vec<i64>,
        latest_run: Option<LatestRun>,
        full: bool,
    ) -> Self {
        let details = full.then_some(TaskListDetails {
            description: task.description,
            acceptance: task.acceptance,
            verification_commands: task.verification_commands,
            context: task.context,
            created_at: task.created_at,
            updated_at: task.updated_at,
        });
        Self {
            id: task.id,
            status: task.status,
            title: task.title,
            goal_id: task.goal_id,
            dependencies,
            latest_run,
            details,
        }
    }
}

pub trait TaskStore {
    fn add(&mut self, task: NewTask) -> Result<Task>;
    /// One page of tasks matching `query`, newest first.
    fn list(&self, query: &TaskQuery) -> Result<TaskPage>;
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

/// The environment variables the LaunchAgent gives the supervisor, which
/// is all a launchd-started process keeps of the shell that ran `up`: its
/// PATH and, only when that shell exported it, the cmux socket password.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorEnvironment {
    pub path: String,
    /// `CMUX_SOCKET_PASSWORD` as exported by the invoking shell; a password
    /// saved in cmux's Settings is never read or stored here.
    pub socket_password: Option<String>,
}

/// cmux answered the detached ping and did not admit it (its message is
/// `reason`), as opposed to not answering at all: only this failure has
/// the socket password as its remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedRefusal {
    pub reason: String,
}

impl std::fmt::Display for DetachedRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for DetachedRefusal {}

pub trait WorkspaceBackend {
    fn preflight(&self) -> Result<()>;
    /// Check that cmux accepts a connection from a process that is not a
    /// child of one of its terminals, the way the launchd-run supervisor
    /// connects: `ping` run outside cmux's process tree with `environment`
    /// and none of the `CMUX_*` variables a cmux session inherits (cmux
    /// admits such a process only by socket password). A refusal is a
    /// [`DetachedRefusal`]; any other error means cmux could not be asked.
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()>;
    fn create(&self, run: &crate::domain::TaskRun, command: &str) -> Result<String>;
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way the maintainer would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// The workspace whose title is exactly `name`, if one is open.
    fn find_named(&self, name: &str) -> Result<Option<String>>;
    /// Open a workspace that is not tied to a run (the maintainer session)
    /// and return its stable ID.
    fn create_named(&self, name: &str, cwd: &std::path::Path, command: &str) -> Result<String>;
    /// How long the session may take to exit after the request before the
    /// supervisor stops waiting and leaves the run to a human.
    fn exit_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
}

/// What the service manager had under a label when `uninstall` ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentState {
    pub loaded: bool,
    /// The agent's process, when it was loaded and running.
    pub pid: Option<u32>,
}

/// The service manager that keeps the supervisor resident (launchd on
/// macOS). `up` writes the agent's definition and loads it; `down` unloads
/// it, which is what makes the supervisor stop without being restarted.
pub trait LaunchAgent {
    /// Write `contents` to `path` and (re)load the agent under `label`: an
    /// agent already loaded is unloaded first, and its process waited for,
    /// so the new definition takes.
    fn install(&self, label: &str, path: &std::path::Path, contents: &str) -> Result<()>;
    /// Unload the agent without waiting for its process (which drains) and
    /// remove its definition so it does not come back at the next login.
    fn uninstall(&self, label: &str, path: &std::path::Path) -> Result<AgentState>;
}

/// Liveness and signals for the supervisor's PID, replaceable in tests.
pub trait ProcessControl {
    fn alive(&self, pid: u32) -> bool;
    /// Ask the process to drain (SIGTERM); used when no agent is loaded for it.
    fn terminate(&self, pid: u32) -> Result<()>;
    /// Ask the process to drain the way Ctrl-C in its terminal would
    /// (SIGINT); used for the supervisor of an in-cmux workspace, which no
    /// service manager can signal for us.
    fn interrupt(&self, pid: u32) -> Result<()>;
    /// End the process immediately (SIGKILL).
    fn kill(&self, pid: u32) -> Result<()>;
}
