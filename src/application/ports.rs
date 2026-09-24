//! The traits the use cases reach the queue, Git, the agent, cmux, the
//! service manager, processes, time and IDs through. The infrastructure
//! implements them and the entry points inject the implementations.

use anyhow::Result;
use std::{fmt, path::Path, process::ExitStatus, sync::Arc, time::SystemTime};

use super::{GraphInput, TaskPage, TaskQuery, timestamp, unix_seconds};
use crate::domain::{
    ClaimOutcome, CommitSha, Goal, GoalDetail, GoalEdit, GoalId, GoalSummary, GoalVerdict, NewGoal,
    NewNote, NewTask, NotePage, NoteQuery, Predecessor, RunEvent, RunId, RunLease, RunProcess,
    RunStatus, SupervisorRegistration, Task, TaskAction, TaskDetail, TaskId, TaskRun,
};

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

/// A run some other process leases, with its wrapper registration: what a
/// supervisor with a free slot judges for adoption (ADR-0012).
#[derive(Debug, Clone)]
pub struct LeasedRun {
    pub run: TaskRun,
    pub lease: RunLease,
    pub wrapper: Option<RunProcess>,
}

/// What `integrate` put on `main`: the squash `commit` whose tree is that of
/// `source_commit` (the rebased run head kept under `history_ref`), on top of
/// `main_before`. `verification_skipped` is always false: every landing
/// runs the verification commands (ADR-0023 decision 1); the field keeps the
/// `run_integrated` payload's shape.
#[derive(Debug, serde::Serialize)]
pub struct Landing {
    pub commit: CommitSha,
    pub source_commit: CommitSha,
    pub main_before: CommitSha,
    pub history_ref: String,
    pub message: String,
    pub verification_skipped: bool,
}

/// The durable state of runs: their leases, the supervisors that hold
/// them, their events and the integration slot. Every transition that
/// takes a `token` is refused unless that token holds the run's lease, so
/// two processes never move one run at once; the refusal is an error.
pub trait RunStore {
    /// Reserve the next dependency-ready task for the supervisor `token`:
    /// the run and its lease are created together.
    fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
    ) -> Result<ClaimOutcome>;
    /// Refresh every lease `token` holds; how many there were.
    fn heartbeat_leases(&self, token: &str) -> Result<usize>;
    fn register_supervisor(
        &mut self,
        token: &str,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration>;
    /// Whether a registration under `token` was removed.
    fn deregister_supervisor(&self, token: &str) -> Result<bool>;
    /// Every registered supervisor, oldest first, alive or not.
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>>;
    fn release_lease(&mut self, id: &RunId, token: &str) -> Result<()>;
    /// Adoptable runs whose lease carries a token other than `token`.
    fn runs_leased_by_others(&self, token: &str) -> Result<Vec<LeasedRun>>;
    /// Take over the stale lease `previous_token` holds on `id`; `None`
    /// when another process got there first or the lease is fresh again.
    fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &str,
        token: &str,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>>;
    fn holds_lease(&self, id: &RunId, token: &str) -> Result<bool>;
    /// Record a runtime error and give the lease up, leaving the status.
    fn abandon_run(&mut self, id: &RunId, token: &str, message: &str) -> Result<TaskRun>;
    fn run_leases(&self) -> Result<Vec<RunLease>>;
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>>;
    fn active_runs(&self) -> Result<Vec<TaskRun>>;
    /// Recover an orphaned run whose `checked_processes` registered
    /// processes the caller found dead.
    fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        report: serde_json::Value,
    ) -> Result<TaskRun>;
    fn run(&self, id: &RunId) -> Result<TaskRun>;
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>>;
    /// The run awaiting integration longest, by validation time.
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>>;
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>>;
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool>;
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Take the single integration slot for `id` under `token`.
    fn begin_integration(&mut self, id: &RunId, token: &str, main: &CommitSha) -> Result<TaskRun>;
    /// Leave the integrating run to a session (`needs_session`).
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun>;
    /// End the integrating run as failed, as its rewritten receipt says.
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Give the slot back before `main` moved; the run returns to `revert_to`.
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &str,
        revert_to: &str,
        message: &str,
    ) -> Result<TaskRun>;
    /// Record the landing: the run is integrated and its task completed.
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &str,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)>;
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str) -> Result<()>;
    fn workspace_closed(&mut self, id: &RunId, token: &str) -> Result<TaskRun>;
    fn cleanup_failed(&mut self, id: &RunId, token: &str, message: &str) -> Result<TaskRun>;
    /// Git common directory the queue is bound to, if any.
    fn repository_binding(&self) -> Result<Option<String>>;
    fn bind_repository(&mut self, common_dir: &str) -> Result<()>;
    fn assert_repository(&self, common_dir: &str) -> Result<()>;
}

/// The queue a use case works on: its tasks and goals and its runs.
pub trait Queue: TaskStore + RunStore {}

impl<T: TaskStore + RunStore + ?Sized> Queue for T {}

/// The Git operations `integrate` and the supervisor use on the repository
/// the queue is bound to and on its run worktrees. Commits are named by
/// their full SHA; a failed Git command is an error with Git's message.
pub trait Repository {
    /// Current `refs/heads/main`, read again on every call.
    fn main_head(&self) -> Result<CommitSha>;
    /// Symbolic HEAD of a worktree (`refs/heads/...`), `None` when detached.
    fn current_branch(&self, worktree: &Path) -> Result<Option<String>>;
    fn head(&self, worktree: &Path) -> Result<CommitSha>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool>;
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>>;
    /// `git status --porcelain` of the worktree; blank when clean.
    fn status(&self, worktree: &Path) -> Result<String>;
    fn rebase_in_progress(&self, worktree: &Path) -> Result<bool>;
    fn rebase_abort(&self, worktree: &Path) -> Result<()>;
    /// Rebase the worktree's branch onto `onto`; `Ok(Err(output))` is a
    /// rebase that stopped (a conflict), left in progress.
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>>;
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>>;
    /// Paths that differ between two commits.
    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>>;
    fn tree_of(&self, commit: &str) -> Result<String>;
    /// One commit with `tree` on top of `parent`, `paragraphs` its message.
    fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<CommitSha>;
    fn update_ref(&self, name: &str, value: &str) -> Result<()>;
    /// Fast-forward `refs/heads/main` from `from` to `to`.
    fn advance_main(&self, from: &str, to: &str) -> Result<()>;
    /// Point the repository's record of a moved worktree at it again.
    fn repair_worktree(&self, worktree: &Path) -> Result<()>;
    fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()>;
    /// The worktree that has `main` checked out, if any.
    fn main_checkout(&self) -> Result<Option<std::path::PathBuf>>;
}

/// Runs a task's verification commands for `integrate` (ADR-0023
/// decision 1) with the `[run.env]` of the repository (ADR-0023 decision 3).
pub trait Verifier {
    /// The environment of the run whose directory is `run_dir`.
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>>;
    /// Run `command` in a shell in `cwd` with `env`, its output in `log`.
    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<ExitStatus>;
}
