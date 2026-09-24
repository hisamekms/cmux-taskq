//! Application-facing storage contract. Provider/process adapters come next.

use anyhow::Result;
use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::domain::{
    ClaimOutcome, CommitSha, EvidenceCheck, Goal, GoalDetail, GoalEdit, GoalId, GoalStatus,
    GoalSummary, GoalVerdict, NewGoal, NewNote, NewTask, NotePage, NoteQuery, Predecessor,
    RunEvent, RunId, RunStatus, Task, TaskAction, TaskDetail, TaskId, TaskStatus,
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
    pub goal_id: Option<GoalId>,
    /// Page size; at least one.
    pub limit: usize,
    /// Start the page at this task ID: only tasks whose ID is at most this.
    /// The previous page's `next` is the first task of the following page.
    pub before: Option<TaskId>,
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
    pub next: Option<TaskId>,
    pub total: usize,
}

/// A task as `list` shows it: what the planner decides on, plus the
/// long fields only with `full`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListItem {
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// IDs of the direct predecessors, ascending.
    pub dependencies: Vec<TaskId>,
    /// The most recently created run, if any.
    pub latest_run: Option<LatestRun>,
    #[serde(flatten)]
    pub details: Option<TaskListDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LatestRun {
    pub id: RunId,
    pub status: RunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListDetails {
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub required_evidence: Vec<EvidenceCheck>,
    pub paths: Vec<String>,
    pub context: String,
    pub created_at: String,
    pub updated_at: String,
}

impl TaskListItem {
    pub fn new(
        task: Task,
        dependencies: Vec<TaskId>,
        latest_run: Option<LatestRun>,
        full: bool,
    ) -> Self {
        let details = full.then_some(TaskListDetails {
            description: task.description().to_owned(),
            acceptance: task.acceptance().to_owned(),
            verification_commands: task.verification_commands().to_vec(),
            required_evidence: task.required_evidence().to_vec(),
            paths: task.paths().to_vec(),
            context: task.context().to_owned(),
            created_at: task.created_at().to_owned(),
            updated_at: task.updated_at().to_owned(),
        });
        Self {
            id: task.id(),
            status: task.status(),
            title: task.title().to_owned(),
            goal_id: task.goal_id(),
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
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// Status of the task's goal; a draft goal's tasks are not candidates.
    pub goal_status: Option<GoalStatus>,
    /// IDs of the direct predecessors, ascending.
    pub depends_on: Vec<TaskId>,
}

/// One read of the queue for `graph`: the unfinished tasks in ID order and
/// the IDs of the claimable ones (`candidates`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphInput {
    pub tasks: Vec<GraphTask>,
    pub candidates: Vec<TaskId>,
}

/// An unfinished task as `graph` shows it (ADR-0023 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphNode {
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// Status of the task's goal, present only for a task in a goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_status: Option<GoalStatus>,
    /// Every direct predecessor, ascending.
    pub depends_on: Vec<TaskId>,
    /// Unfinished tasks that depend on this one directly, ascending.
    pub blocks: Vec<TaskId>,
    /// How many unfinished tasks depend on this one directly or transitively:
    /// the tasks its completion moves closer to running.
    pub unblocks: usize,
    /// The direct predecessors that are still unfinished, ascending.
    pub ready_after: Vec<TaskId>,
}

/// The dependency view of the unfinished tasks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DependencyGraph {
    /// Unfinished tasks in ID order.
    pub tasks: Vec<GraphNode>,
    /// Claimable tasks in the order the supervisor claims them: most
    /// `unblocks` first, then ascending ID.
    pub candidates: Vec<TaskId>,
    /// The chain from the task with the most `unblocks` (lowest ID on a tie),
    /// each step to the directly blocked task with the most `unblocks`
    /// (lowest ID on a tie), down to a task that blocks nothing. Empty when
    /// no task blocks another.
    pub critical: Vec<TaskId>,
}

/// Compute the dependency view. Counts always span every unfinished task;
/// `goal_id` only narrows `tasks`, `candidates` and where `critical` starts
/// (the chain may then leave the goal). The queue rejects cycles, so the
/// dependencies form a DAG; a predecessor that is not in `input.tasks` is
/// finished.
pub fn dependency_graph(input: GraphInput, goal_id: Option<GoalId>) -> DependencyGraph {
    use std::collections::{BTreeMap, BTreeSet};
    let open: BTreeSet<TaskId> = input.tasks.iter().map(|task| task.id).collect();
    let mut blocks: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
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
    let direct = |id: TaskId| blocks.get(&id).map(Vec::as_slice).unwrap_or_default();
    let unblocks: BTreeMap<TaskId, usize> = open
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
    let count = |id: TaskId| unblocks.get(&id).copied().unwrap_or(0);
    // Most unblocks first, lowest ID on a tie.
    let rank = |id: &TaskId| (std::cmp::Reverse(count(*id)), *id);
    let in_goal = |task: &GraphTask| goal_id.is_none() || task.goal_id == goal_id;
    let goal_ids: BTreeSet<TaskId> = input
        .tasks
        .iter()
        .filter(|task| in_goal(task))
        .map(|task| task.id)
        .collect();
    let mut candidates: Vec<TaskId> = input
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
            goal_status: task.goal_status,
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
    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail>;
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task>;
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    /// Dependency-ready tasks; each task is limited to one unfinished run.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// The unfinished tasks with their direct predecessors and the IDs of
    /// `candidates`, read in one snapshot.
    fn graph_input(&self) -> Result<GraphInput>;
    /// Reserve one run atomically, without a lease. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome>;
    /// Direct predecessors of a task, each with the run that landed it, in ID order.
    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>>;
    /// Tasks that are `in_progress` right now, in ID order.
    fn tasks_in_progress(&self) -> Result<Vec<Task>>;
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal>;
    /// Every goal in ID order with its task counts by status.
    fn list_goals(&self) -> Result<Vec<GoalSummary>>;
    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail>;
    /// Replace the given fields; running runs keep their prompt snapshot.
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal>;
    /// Record the verdict once. `achieved` is refused while a task is not
    /// completed or canceled; `abandoned` while a task is in progress.
    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal>;
    /// Move a draft or ready task to an open goal, or to none.
    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task>;
    /// Replace the globs of the paths a draft or ready task may change
    /// (ADR-0029); an empty list removes the limit.
    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task>;
    /// Open a draft goal so its tasks become candidates (ADR-0024 decision 5).
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal>;
    /// Record a note as an `observation` run event on its task, run or goal.
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent>;
    /// One page of notes, oldest first.
    fn notes(&self, query: &NoteQuery) -> Result<NotePage>;
}

/// Provider-specific CLI construction is kept outside supervisor orchestration.
pub trait AgentProvider {
    fn preflight(&self) -> Result<()>;
    fn command(&self, run: &crate::domain::TaskRun, prompt: &str) -> Result<std::process::Command>;
    /// The same session reopened for a `needs_session` run (ADR-0019): the
    /// run's own settings and idle marker, without a prompt; the supervisor
    /// sends the resolution request to the terminal once it is up.
    fn resume_command(&self, run: &crate::domain::TaskRun) -> Result<std::process::Command>;
    /// A headless run of the agent for a job without a workspace (ADR-0024
    /// decision 2): `prompt` in `cwd`, allowed only `allowed_tools` beyond
    /// what needs no permission. The caller sets the environment and where
    /// the output goes. A provider without one refuses.
    fn headless_command(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        allowed_tools: &[&str],
    ) -> Result<std::process::Command> {
        let _ = (cwd, prompt, allowed_tools);
        anyhow::bail!("this provider has no headless execution")
    }
    /// How often the session wrapper checks the agent for its exit and
    /// heartbeats; tests shorten it.
    fn wait_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
    /// The headless review of an accepted run (ADR-0023 decision 2,
    /// ADR-0027). Kept apart from [`AgentProvider::headless_command`]: a
    /// review belongs to a run, and needs that run's directory (settings
    /// without the worker's `Stop` hook, so the live session's idle marker
    /// is not written, a debug file, `--add-dir`) and tools denied as well
    /// as allowed, since the live worker session owns the worktree; the
    /// observer's job has no run.
    ///
    /// A non-interactive agent in the run's worktree with settings of the
    /// run directory and `prompt` as its only input, whose
    /// stdout is the verdict JSON. It must not touch the worker session's
    /// idle marker. The runtime wires stdin, stdout and stderr, waits at
    /// most [`AgentProvider::review_timeout`] and reads stdout.
    fn review_command(
        &self,
        run: &crate::domain::TaskRun,
        prompt: &str,
    ) -> Result<std::process::Command>;
    /// How long the headless review may take before it counts as failed.
    fn review_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }
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
    /// Open the workspace a resumed session of a `needs_session` run works
    /// in, in the run's worktree; the backend names it like the run's worker
    /// workspace (ADR-0028; display only, ADR-0026) and gives it `tags`.
    fn create_resume(
        &self,
        task: &crate::domain::Task,
        run: &crate::domain::TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// Type one line at the session's prompt and submit it: the resolution
    /// request to a resumed session, the only text besides `/exit` the
    /// supervisor sends (ADR-0019).
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()>;
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way a person would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// Whether the workspace with this stable ID is still open. Workspaces
    /// are found by the ID the queue recorded, never by their title, which
    /// people may rename (ADR-0026).
    fn exists(&self, workspace_id: &str) -> Result<bool>;
    /// Open a workspace that is not tied to a run (the inbox and planner
    /// sessions, the in-cmux supervisor) and return its stable ID.
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
    /// to; `None` sends it without one. Only `ask` sends one, for a new
    /// ask, aimed at the inbox (ADR-0022 decision 5); the supervisor sends
    /// none.
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()>;
    /// How long one call may run before the backend gives it up as failed;
    /// recorded with every `backend_call_failed`.
    fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
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
    /// How long a session may run with neither a receipt nor an idle marker
    /// before the supervisor starts reading its screen for a dialog.
    fn prompt_wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(90)
    }
    /// How long a resumed session's agent may take, after it registered, to
    /// be ready for the resolution request.
    fn resume_prompt_delay(&self) -> std::time::Duration {
        std::time::Duration::from_secs(5)
    }
    /// How long a resumed session may work on the resolution request
    /// without going idle before the supervisor asks it to exit.
    fn resume_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
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

    fn ids(ids: &[i64]) -> Vec<TaskId> {
        ids.iter().copied().map(TaskId::new).collect()
    }

    fn task(id: i64, goal_id: Option<i64>, depends_on: &[i64]) -> GraphTask {
        GraphTask {
            id: TaskId::new(id),
            status: if depends_on.is_empty() {
                TaskStatus::Ready
            } else {
                TaskStatus::Draft
            },
            title: format!("task {id}"),
            goal_id: goal_id.map(GoalId::new),
            goal_status: goal_id.map(|_| GoalStatus::Open),
            depends_on: ids(depends_on),
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
            candidates: ids(&[1, 2, 7]),
        }
    }

    #[test]
    fn graph_counts_transitive_releases_and_follows_the_critical_chain() {
        let graph = dependency_graph(input(), None);
        let unblocks: Vec<(i64, usize)> = graph
            .tasks
            .iter()
            .map(|t| (t.id.as_i64(), t.unblocks))
            .collect();
        assert_eq!(
            unblocks,
            [(1, 1), (2, 3), (3, 1), (4, 1), (5, 0), (6, 0), (7, 0)]
        );
        assert_eq!(graph.tasks[1].blocks, ids(&[3, 4]));
        assert_eq!(graph.tasks[4].depends_on, ids(&[3, 4]));
        assert_eq!(graph.tasks[4].ready_after, ids(&[3, 4]));
        assert_eq!(graph.tasks[6].depends_on, ids(&[9]));
        assert!(graph.tasks[6].ready_after.is_empty());
        assert_eq!(graph.candidates, ids(&[2, 1, 7]));
        assert_eq!(graph.critical, ids(&[2, 3, 5]));
    }

    #[test]
    fn goal_narrows_tasks_candidates_and_the_critical_start() {
        let graph = dependency_graph(input(), Some(GoalId::new(1)));
        let tasks: Vec<TaskId> = graph.tasks.iter().map(|t| t.id).collect();
        assert_eq!(tasks, ids(&[1, 6]));
        assert_eq!(graph.candidates, ids(&[1]));
        assert_eq!(graph.critical, ids(&[1, 6]));
    }

    #[test]
    fn no_blocking_task_leaves_the_critical_chain_empty() {
        let graph = dependency_graph(
            GraphInput {
                tasks: vec![task(2, None, &[]), task(1, None, &[])],
                candidates: ids(&[2, 1]),
            },
            None,
        );
        assert_eq!(graph.candidates, ids(&[1, 2]));
        assert!(graph.critical.is_empty());
        assert!(
            dependency_graph(GraphInput::default(), None)
                .tasks
                .is_empty()
        );
    }
}

/// The current time, injected so a use case reads it through this port and
/// a test can fix it (ADR-0013 policy 7). One operation reads it once and
/// passes the value on, so its steps share one reference time.
pub trait Clock: Send + Sync {
    fn system_time(&self) -> SystemTime;

    /// Unix seconds, the form of `heartbeat_at`, `closed_at` and the other
    /// INTEGER times.
    fn now(&self) -> i64 {
        unix_seconds(self.system_time())
    }

    /// `%Y-%m-%dT%H:%M:%fZ` in UTC (RFC 3339 with milliseconds), the form
    /// SQLite's `strftime` gives `created_at` / `updated_at`.
    fn timestamp(&self) -> String {
        timestamp(self.system_time())
    }
}

/// New identifiers: run IDs, supervisor tokens and integrate tokens, each
/// a UUID string.
pub trait IdGenerator: Send + Sync {
    fn uuid(&self) -> String;
}

/// The clock and the ID generator a use case is given.
#[derive(Clone)]
pub struct Generators {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

impl fmt::Debug for Generators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Generators").finish_non_exhaustive()
    }
}

/// Seconds since the Unix epoch; zero before it.
pub fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// `time` as `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC; the epoch for a time before it.
pub fn timestamp(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = elapsed.as_secs() as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        elapsed.subsec_millis()
    )
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Duration;

    struct At(SystemTime);

    impl Clock for At {
        fn system_time(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn a_clock_gives_unix_seconds_and_the_timestamp_column_form() {
        let clock = At(UNIX_EPOCH + Duration::from_millis(1_000_000_000_123));
        assert_eq!(clock.now(), 1_000_000_000);
        assert_eq!(clock.timestamp(), "2001-09-09T01:46:40.123Z");
        let before_epoch = At(UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(before_epoch.now(), 0);
        assert_eq!(before_epoch.timestamp(), "1970-01-01T00:00:00.000Z");
        // Leap days and the last millisecond of a year.
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_millis(1_735_689_599_999)),
            "2024-12-31T23:59:59.999Z"
        );
    }
}
