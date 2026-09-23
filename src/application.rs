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

/// An unfinished task (draft, ready or in progress) with every direct
/// predecessor, finished or not: the input of [`dependency_graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphTask {
    pub id: i64,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<i64>,
    /// IDs of the direct predecessors, ascending.
    pub depends_on: Vec<i64>,
}

/// One read of the queue for `graph`: the unfinished tasks in ID order and
/// the IDs of the claimable ones (`candidates`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphInput {
    pub tasks: Vec<GraphTask>,
    pub candidates: Vec<i64>,
}

/// An unfinished task as `graph` shows it (ADR-0023 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphNode {
    pub id: i64,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<i64>,
    /// Every direct predecessor, ascending.
    pub depends_on: Vec<i64>,
    /// Unfinished tasks that depend on this one directly, ascending.
    pub blocks: Vec<i64>,
    /// How many unfinished tasks depend on this one directly or transitively:
    /// the tasks its completion moves closer to running.
    pub unblocks: usize,
    /// The direct predecessors that are still unfinished, ascending.
    pub ready_after: Vec<i64>,
}

/// The dependency view of the unfinished tasks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DependencyGraph {
    /// Unfinished tasks in ID order.
    pub tasks: Vec<GraphNode>,
    /// Claimable tasks in the order the supervisor claims them: most
    /// `unblocks` first, then ascending ID.
    pub candidates: Vec<i64>,
    /// The chain from the task with the most `unblocks` (lowest ID on a tie),
    /// each step to the directly blocked task with the most `unblocks`
    /// (lowest ID on a tie), down to a task that blocks nothing. Empty when
    /// no task blocks another.
    pub critical: Vec<i64>,
}

/// Compute the dependency view. Counts always span every unfinished task;
/// `goal_id` only narrows `tasks`, `candidates` and where `critical` starts
/// (the chain may then leave the goal). The queue rejects cycles, so the
/// dependencies form a DAG; a predecessor that is not in `input.tasks` is
/// finished.
pub fn dependency_graph(input: GraphInput, goal_id: Option<i64>) -> DependencyGraph {
    use std::collections::{BTreeMap, BTreeSet};
    let open: BTreeSet<i64> = input.tasks.iter().map(|task| task.id).collect();
    let mut blocks: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for task in &input.tasks {
        for predecessor in &task.depends_on {
            if open.contains(predecessor) {
                blocks.entry(*predecessor).or_default().push(task.id);
            }
        }
    }
    for dependents in blocks.values_mut() {
        dependents.sort_unstable();
        dependents.dedup();
    }
    let direct = |id: i64| blocks.get(&id).map(Vec::as_slice).unwrap_or_default();
    let unblocks: BTreeMap<i64, usize> = open
        .iter()
        .map(|&id| {
            let mut reached = BTreeSet::new();
            let mut pending = direct(id).to_vec();
            while let Some(next) = pending.pop() {
                if reached.insert(next) {
                    pending.extend_from_slice(direct(next));
                }
            }
            (id, reached.len())
        })
        .collect();
    let count = |id: i64| unblocks.get(&id).copied().unwrap_or(0);
    // Most unblocks first, lowest ID on a tie.
    let rank = |id: &i64| (std::cmp::Reverse(count(*id)), *id);
    let in_goal = |task: &GraphTask| goal_id.is_none() || task.goal_id == goal_id;
    let goal_ids: BTreeSet<i64> = input
        .tasks
        .iter()
        .filter(|task| in_goal(task))
        .map(|task| task.id)
        .collect();
    let mut candidates: Vec<i64> = input
        .candidates
        .iter()
        .copied()
        .filter(|id| goal_id.is_none() || goal_ids.contains(id))
        .collect();
    candidates.sort_by_key(rank);
    let mut critical = Vec::new();
    let mut step = goal_ids.iter().copied().min_by_key(rank);
    if step.is_some_and(|id| count(id) == 0) {
        step = None;
    }
    while let Some(id) = step {
        critical.push(id);
        step = direct(id).iter().copied().min_by_key(rank);
    }
    let tasks = input
        .tasks
        .into_iter()
        .filter(|task| goal_ids.contains(&task.id))
        .map(|task| GraphNode {
            blocks: direct(task.id).to_vec(),
            unblocks: count(task.id),
            ready_after: task
                .depends_on
                .iter()
                .copied()
                .filter(|id| open.contains(id))
                .collect(),
            id: task.id,
            status: task.status,
            title: task.title,
            goal_id: task.goal_id,
            depends_on: task.depends_on,
        })
        .collect();
    DependencyGraph {
        tasks,
        candidates,
        critical,
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
    /// The unfinished tasks with their direct predecessors and the IDs of
    /// `candidates`, read in one snapshot.
    fn graph_input(&self) -> Result<GraphInput>;
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

/// The Git remote `integrate` pushes the landed `main` to (ADR-0019
/// decision 3), replaceable in tests.
pub trait MainRemote {
    /// Whether the repository has a remote named `remote`.
    fn has_remote(&self, remote: &str) -> Result<bool>;
    /// Push `refs/heads/main` to the same branch of `remote`. An error is
    /// the failed push, with Git's message.
    fn push_main(&self, remote: &str) -> Result<()>;
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

/// What a workspace carries besides its title and command (ADR-0026).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceTags {
    /// Environment every shell of the workspace inherits: `DAGQ_ROLE` and
    /// `DAGQ_QUEUE`, which the workspace keeps however its session is
    /// started again.
    pub env: Vec<(String, String)>,
    /// One machine-readable line for people; never read back.
    pub description: Option<String>,
    /// The queue's workspace group, when it could be made.
    pub group: Option<String>,
}

pub trait WorkspaceBackend {
    fn preflight(&self) -> Result<()>;
    /// Check that cmux accepts a connection from a process that is not a
    /// child of one of its terminals, the way the launchd-run supervisor
    /// connects: `ping` run outside cmux's process tree with `environment`
    /// and none of the `CMUX_*` variables a cmux session inherits (cmux
    /// admits such a process only by socket password). A refusal is a
    /// [`DetachedRefusal`]; any other error means cmux could not be asked.
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()>;
    /// Open the workspace a run's session works in. `task` is the run's
    /// task; the backend names the workspace after it (ADR-0018) and gives
    /// it `tags`.
    fn create(
        &self,
        task: &crate::domain::Task,
        run: &crate::domain::TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way the maintainer would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// Whether the workspace with this stable ID is still open. Workspaces
    /// are found by the ID the queue recorded, never by their title, which
    /// people may rename (ADR-0026).
    fn exists(&self, workspace_id: &str) -> Result<bool>;
    /// Open a workspace that is not tied to a run (the maintainer session,
    /// the in-cmux supervisor) and return its stable ID.
    fn create_named(
        &self,
        name: &str,
        cwd: &std::path::Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// The handle of the workspace group whose external ID is
    /// `external_id`, created under `name` when there is none yet; asking
    /// again returns the same group.
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String>;
    /// Tell a person that something waits for them: a notification, never
    /// keystrokes into a terminal. `workspace` is the workspace it belongs
    /// to; `None` sends it without one. The supervisor sends none yet:
    /// ADR-0022 limits notifications to `ask_opened`, aimed at the inbox.
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()>;
    /// How long the session may take to exit after the request before the
    /// supervisor stops waiting and leaves the run to a human.
    fn exit_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
    /// How long the session's wrapper may take to register after the
    /// workspace opens before the supervisor gives the run up.
    fn registration_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(45)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: i64, goal_id: Option<i64>, depends_on: &[i64]) -> GraphTask {
        GraphTask {
            id,
            status: if depends_on.is_empty() {
                TaskStatus::Ready
            } else {
                TaskStatus::Draft
            },
            title: format!("task {id}"),
            goal_id,
            depends_on: depends_on.to_vec(),
        }
    }

    /// 2 -> {3, 4} -> 5, 1 -> 6 (goal 1), and 7 waits only on the finished 9.
    fn input() -> GraphInput {
        GraphInput {
            tasks: vec![
                task(1, Some(1), &[]),
                task(2, None, &[]),
                task(3, None, &[2]),
                task(4, None, &[2]),
                task(5, None, &[3, 4]),
                task(6, Some(1), &[1]),
                task(7, None, &[9]),
            ],
            candidates: vec![1, 2, 7],
        }
    }

    #[test]
    fn graph_counts_transitive_releases_and_follows_the_critical_chain() {
        let graph = dependency_graph(input(), None);
        let unblocks: Vec<(i64, usize)> = graph.tasks.iter().map(|t| (t.id, t.unblocks)).collect();
        assert_eq!(
            unblocks,
            [(1, 1), (2, 3), (3, 1), (4, 1), (5, 0), (6, 0), (7, 0)]
        );
        assert_eq!(graph.tasks[1].blocks, [3, 4]);
        assert_eq!(graph.tasks[4].depends_on, [3, 4]);
        assert_eq!(graph.tasks[4].ready_after, [3, 4]);
        assert_eq!(graph.tasks[6].depends_on, [9]);
        assert!(graph.tasks[6].ready_after.is_empty());
        assert_eq!(graph.candidates, [2, 1, 7]);
        assert_eq!(graph.critical, [2, 3, 5]);
    }

    #[test]
    fn goal_narrows_tasks_candidates_and_the_critical_start() {
        let graph = dependency_graph(input(), Some(1));
        let ids: Vec<i64> = graph.tasks.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 6]);
        assert_eq!(graph.candidates, [1]);
        assert_eq!(graph.critical, [1, 6]);
    }

    #[test]
    fn no_blocking_task_leaves_the_critical_chain_empty() {
        let graph = dependency_graph(
            GraphInput {
                tasks: vec![task(2, None, &[]), task(1, None, &[])],
                candidates: vec![2, 1],
            },
            None,
        );
        assert_eq!(graph.candidates, [1, 2]);
        assert!(graph.critical.is_empty());
        assert!(
            dependency_graph(GraphInput::default(), None)
                .tasks
                .is_empty()
        );
    }
}
