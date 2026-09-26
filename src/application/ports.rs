//! The traits the use cases reach the queue, Git, the agent, cmux, the
//! service manager, processes, time and IDs through. The infrastructure
//! implements them and the entry points inject the implementations.

use anyhow::Result;
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use super::{GraphInput, TaskPage, TaskQuery, timestamp, unix_seconds};
use crate::domain::{
    Ask, AskId, AskKind, AskOutcome, ClaimOutcome, CommitSha, DraftOrigin, DraftTarget, EventId,
    EvidenceCheck, Goal, GoalDetail, GoalEdit, GoalId, GoalPredecessor, GoalSummary, GoalVerdict,
    LintInput, NewAsk, NewGoal, NewNote, NewTask, NotePage, NoteQuery, PlanReviewCandidate,
    PlanReviewDecision, PlanReviewVerdict, PlannerId, PlannerOrigin, PlannerSession, Predecessor,
    Priority, Proposal, ProposalId, Reason, ReasonCode, RunEvent, RunId, RunLease, RunPlan,
    RunProcess, RunStatus, SessionRole, Submission, SupervisorMode, SupervisorRegistration, Task,
    TaskAction, TaskDetail, TaskEdit, TaskId, TaskKind, TaskRun, TaskStatus,
    related::RelatedPage,
    search::{SearchPage, SearchQuery},
};

pub trait TaskStore {
    fn add(&mut self, task: NewTask) -> Result<Task>;
    /// One page of tasks matching `query`, newest first.
    fn list(&self, query: &TaskQuery) -> Result<TaskPage>;
    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail>;
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task>;
    /// Cancel a task as a duplicate of another (ADR-0046 decision 5),
    /// recording which in its `task_status_changed`. The other task must
    /// exist, differ and not be canceled.
    fn cancel_duplicate(&mut self, task_id: TaskId, duplicate_of: TaskId) -> Result<Task>;
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    /// Make a draft or ready task wait until `goal_id` is closed as achieved
    /// (ADR-0038); never its own goal, never a cycle.
    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    /// Dependency-ready tasks in claim order (ADR-0040 decision 4); each
    /// task is limited to one unfinished run.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// The unfinished tasks with their direct predecessors and the IDs of
    /// `candidates`, read in one snapshot.
    fn graph_input(&self) -> Result<GraphInput>;
    /// Reserve one run atomically, without a lease. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome>;
    /// Direct predecessors of a task, each with the run that landed it, in ID order.
    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>>;
    /// Goals a task depends on, in ID order, each with its completed tasks
    /// and the runs that landed them.
    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>>;
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
    /// Replace the given fields of a draft task (ADR-0041 decision 9),
    /// recording `task_edited` with the fields that changed; running runs
    /// keep their prompt snapshot.
    fn edit_task(&mut self, task_id: TaskId, edit: TaskEdit) -> Result<Task>;
    /// Give a draft or ready task another priority (ADR-0040 decision 4);
    /// it takes effect at the next claim.
    fn set_priority(&mut self, task_id: TaskId, priority: Priority) -> Result<Task>;
    /// Bundle draft tasks, the draft tasks of the given goals and those
    /// goals into a proposal and submit it for plan review (ADR-0041
    /// decisions 7, 8): the tasks become `submitted`, which no claim takes.
    /// With a proposal ID, submit that proposal again after a revise,
    /// with the drafts it holds. A task or goal of another active proposal
    /// is refused.
    fn submit(&mut self, submission: Submission) -> Result<Proposal>;
    /// The plan-review path to `ready` (ADR-0041 decisions 8, 11): the
    /// submitted proposal is accepted, its submitted tasks become ready and
    /// its draft goals open.
    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Plan review sends the submitted proposal back to its planner: its
    /// submitted tasks return to draft.
    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Withdraw a submitted or revising proposal: it ends as canceled, its
    /// submitted tasks return to draft, and its tasks and goals are free to
    /// join another proposal.
    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal>;
    /// The submitted and revising proposals, oldest submission first; with
    /// `all`, every proposal.
    fn proposals(&self, all: bool) -> Result<Vec<Proposal>>;
    /// What `lint` checks `tasks` against (ADR-0041 decision 10), read in
    /// one snapshot: the tasks in the order given, every task's status and
    /// dependencies, and every goal's verdict. A missing task is an error.
    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput>;
    /// Open a draft goal so its tasks become candidates (ADR-0024 decision 5).
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal>;
    /// Record a note as an `observation` run event on its task, run or goal.
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent>;
    /// One page of notes, oldest first.
    fn notes(&self, query: &NoteQuery) -> Result<NotePage>;
}

/// A process to start: its program, arguments, environment changes and
/// working directory, built like a command and started by a [`Spawner`],
/// which also decides where its standard streams go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    /// In the order given; `None` removes the variable.
    envs: Vec<(OsString, Option<OsString>)>,
    current_dir: Option<PathBuf>,
    /// Start it in a session and process group of its own, so neither a
    /// signal to the starter's group nor the close of its terminal reaches
    /// it (the automatic update's job, ADR-0045 decision 13).
    new_session: bool,
}

impl CommandSpec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            ..Self::default()
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Add options `args` before the `--` that ends the options given so
    /// far, or after them all when there is none.
    pub fn option_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let at = self
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(self.args.len());
        let rest = self.args.split_off(at);
        self.args(args);
        self.args.extend(rest);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in vars {
            self.env(key, value);
        }
        self
    }

    /// The process does not inherit `key`.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// See [`Self::get_new_session`].
    pub fn new_session(&mut self) -> &mut Self {
        self.new_session = true;
        self
    }

    /// Whether the process starts in a session of its own.
    pub fn get_new_session(&self) -> bool {
        self.new_session
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    /// Every change to the environment in the order given; `None` removes.
    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, Option<&OsStr>)> {
        self.envs
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_deref()))
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }
}

/// Where the standard streams of a started process go.
#[derive(Debug, Clone, Copy)]
pub enum Streams<'a> {
    /// The starting process's own: the session wrapper's terminal.
    Inherit,
    /// Nowhere.
    Null,
    /// No input; stdout and stderr to these files, created or truncated.
    Files { stdout: &'a Path, stderr: &'a Path },
}

/// How a process ended: `description` as the operating system words it
/// (`exit status: 1`), `code` absent when a signal ended it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub success: bool,
    pub code: Option<i32>,
    pub description: String,
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

/// A process a [`Spawner`] started.
pub trait Spawned: Send {
    fn id(&self) -> u32;
    /// `None` while it runs.
    fn try_wait(&mut self) -> Result<Option<Exit>>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<Exit>;
}

/// Starts processes: the agent under the session wrapper, the headless
/// review and triage jobs, and the observer.
pub trait Spawner: Send + Sync {
    fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>>;
}

/// The files of the runs (the run directory, its prompt, the receipt, the
/// idle marker and `review.md`) and of the queue directory (`rebind`'s log
/// and `repository` file) as the use cases read and write them. Errors are
/// the operating system's, unchanged.
pub trait RunFiles: Send + Sync {
    /// Create `dir` and every missing parent.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Create `dir`, which must not exist yet.
    fn create_new_dir(&self, dir: &Path) -> io::Result<()>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// When the file was last written.
    fn modified(&self, path: &Path) -> io::Result<SystemTime>;
    /// The modification time and the bytes of one open file, so both
    /// belong to the same write; `None` when there is no file.
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn exists(&self, path: &Path) -> bool;
    /// The paths of the entries of `dir`, in no particular order.
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// The bytes the directory `dir` takes on disk, each file counted once
    /// however many links it has, without following links; `None` when
    /// `dir` is not a directory (missing, a file or a link).
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>>;
    /// Remove the directory `dir` and everything under it.
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Append `line` and a newline to `path`, creating it if missing.
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()>;
    /// The absolute path with every link resolved; an error when it does
    /// not exist.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Write `text`, then the bytes of the file `body` as a block fenced
    /// with one backtick more than its longest backtick run (three at
    /// least) and labelled `info`, to a new file at `path`, and sync it.
    /// `body` is read in chunks, never whole.
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()>;
    /// The wall clock that stamps the files: a time compared with a
    /// file's modification time is read here, not from the [`Clock`].
    fn now(&self) -> SystemTime;
}

/// Opens connections to the queue: the supervisor's own, and one for each
/// thread that works beside its loop (the heartbeat, validations,
/// landings).
pub trait QueueOpener: Send + Sync {
    fn open(&self) -> Result<Box<dyn Queue + Send>>;
}

/// Provider-specific CLI construction is kept outside supervisor orchestration.
pub trait AgentProvider {
    fn preflight(&self) -> Result<()>;
    fn command(&self, run: &crate::domain::TaskRun, prompt: &str) -> Result<CommandSpec>;
    /// The same session reopened for a `needs_session` run (ADR-0019): the
    /// run's own settings and idle marker, without a prompt; the supervisor
    /// sends the resolution request to the terminal once it is up.
    fn resume_command(&self, run: &crate::domain::TaskRun) -> Result<CommandSpec>;
    /// A headless run of the agent for a job without a workspace (ADR-0024
    /// decision 2): `prompt` in `cwd`, allowed only `allowed_tools` beyond
    /// what needs no permission. The caller sets the environment and where
    /// the output goes. A provider without one refuses.
    fn headless_command(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        allowed_tools: &[&str],
    ) -> Result<CommandSpec> {
        let _ = (cwd, prompt, allowed_tools);
        anyhow::bail!("this provider has no headless execution")
    }
    /// The agent of a planner session (ADR-0041 decisions 1, 6): an
    /// interactive agent in `planner.cwd` with `planner.prompt` as its first
    /// message, whose `Stop` hook writes [`PlannerCommand::idle_marker`] the
    /// way a worker's does, and that loads `planner.plugin_dir`. A provider
    /// without one refuses.
    fn planner_command(&self, planner: &PlannerCommand<'_>) -> Result<CommandSpec> {
        let _ = planner;
        anyhow::bail!("this provider has no planner session")
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
    fn review_command(&self, run: &crate::domain::TaskRun, prompt: &str) -> Result<CommandSpec>;
    /// Start the session of a headless job (`command`, from
    /// [`AgentProvider::review_command`] or
    /// [`AgentProvider::headless_command`]) with `session_id`, so that its
    /// span and transcript are known before it ends (ADR-0048 decision 4).
    /// A provider that cannot name its session leaves it as it is.
    fn assign_session_id(&self, command: &mut CommandSpec, session_id: &str) {
        let _ = (command, session_id);
    }
    /// How long the headless review may take before it counts as failed.
    fn review_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }
}

/// Where the transcript of a Claude session span is: the span's session id,
/// the directory the session ran in and the path recorded for it, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptSource {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
}

/// The transcripts of the agent's sessions (ADR-0048 decision 9): the one
/// place that knows where they are and what they hold. The runtime reads a
/// span's active time from it, and task 199 its token counts. A transcript
/// that cannot be read (missing, of a format this build does not know, of
/// another session) is `Err`, and nothing is recorded from it.
pub trait Transcripts {
    fn read(
        &self,
        source: &TranscriptSource,
    ) -> std::result::Result<
        crate::domain::transcript::Transcript,
        crate::domain::transcript::Unreadable,
    >;
}

/// What the agent of a planner session is started with: the planner's
/// directory (its prompt, settings, log and idle marker), the directory it
/// works in (the repository's checkout), its first message and the plugin
/// directory it loads.
#[derive(Debug, Clone, Copy)]
pub struct PlannerCommand<'a> {
    pub dir: &'a std::path::Path,
    pub cwd: &'a std::path::Path,
    pub prompt: &'a str,
    pub plugin_dir: Option<&'a std::path::Path>,
}

impl PlannerCommand<'_> {
    /// Where the agent's `Stop` hook writes its input when the agent stops,
    /// in the planner's directory like a run's `idle.json`.
    pub fn idle_marker(&self) -> std::path::PathBuf {
        planner_idle_marker(self.dir)
    }
}

/// The idle marker of the planner whose directory is `dir`.
pub fn planner_idle_marker(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("idle.json")
}

/// What the supervisor reads of the agent of a live session: its screen,
/// for a dialog that holds it (ADR-0019 decision 6), and the idle marker
/// its hook writes when it stops (ADR-0016). Both formats are the agent's
/// own (for Claude Code, its TUI and its `Stop` hook input), so the
/// provider's adapter implements this; the supervisor decides what a dialog
/// or an idle agent means for the run.
pub trait AgentSignals {
    /// The kind of dialog at the bottom of `screen` that holds the session
    /// (recorded as `prompt` of `prompt_waiting`), or `None` while it works.
    fn detect_prompt(&self, screen: &str) -> Option<&'static str>;
    /// Whether the bottom of `screen` shows the agent stopped at a login
    /// that ran out (ADR-0047 decision 42): only a person can log in again.
    fn auth_required(&self, _screen: &str) -> bool {
        false
    }
    /// The last lines of `screen` an ask and `prompt_waiting` carry.
    fn screen_excerpt(&self, screen: &str) -> String;
    /// What the idle marker's content says. A content the adapter cannot
    /// read still marks a stop.
    fn idle_hook(&self, content: &[u8]) -> IdleHook;
    /// Whether the agent's input box is drawn with no dialog over it: text
    /// typed now reaches the agent (a booting session drops it).
    fn input_ready(&self, screen: &str) -> bool;
    /// Whether the input box still holds `text` after it was submitted.
    fn input_pending(&self, screen: &str, text: &str) -> bool;
    /// Whether the screen shows the agent at work on a turn.
    fn working(&self, screen: &str) -> bool;
    /// The dialog of the fixed list the supervisor answers by rule
    /// (ADR-0047 decision 29) at the bottom of `screen`, with the keys that
    /// answer it, or `None` for any other screen, however much a dialog.
    fn known_dialog(&self, _screen: &str) -> Option<DialogAnswer> {
        None
    }
}

/// A dialog of the agent's TUI the supervisor answers by a fixed rule once
/// its safety conditions hold (ADR-0047 decision 29). Any other dialog gets
/// no key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownDialog {
    /// The confirmation a `/exit` gets while background work runs
    /// ("Background work is running"): answered with "Exit and stop tasks"
    /// only after the supervisor's `/exit`, with the worktree clean and the
    /// receipt's commit at HEAD.
    BackgroundWork,
    /// The Settings panel (`/status`, `/usage`, ...) left open over the
    /// input box: closed with Esc at any stage.
    SettingsPanel,
}

impl KnownDialog {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackgroundWork => "background_work",
            Self::SettingsPanel => "settings_panel",
        }
    }
}

/// A known dialog found on a screen and the keys that answer it, in the
/// backend's key names ([`WorkspaceBackend::send_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAnswer {
    pub dialog: KnownDialog,
    pub keys: Vec<&'static str>,
}

/// The content of an idle marker, as [`AgentSignals::idle_hook`] read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleHook {
    /// Background work the agent left running when it stopped: a `/exit`
    /// sent now stops at the agent's own dialog.
    pub background_running: bool,
    /// The background tasks still `running` when it stopped.
    pub background_tasks: Vec<crate::domain::stall::BackgroundTask>,
    /// The fields of the hook input recorded with the evidence of a stop,
    /// by name.
    pub evidence: Vec<(&'static str, serde_json::Value)>,
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

/// The one `CMUX_*` variable a detached process may carry: cmux's CLI
/// reads its socket password from it.
pub const SOCKET_PASSWORD_ENV: &str = "CMUX_SOCKET_PASSWORD";

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
    /// supervisor sends (ADR-0019). A long text is given time to be pasted
    /// before the Enter. Whether it was submitted is the caller's to read
    /// from the screen (task 285).
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()>;
    /// Press Enter alone: a text or `/exit` left in the input box after its
    /// submit is submitted again without being typed twice (task 285).
    fn send_enter(&self, workspace_id: &str) -> Result<()>;
    /// Press one key (`down`, `up`, `enter`, `escape`): the answer to a
    /// known dialog (ADR-0047 decision 29), the only keys the supervisor
    /// sends besides Enter. A backend without keys refuses.
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        let _ = (workspace_id, key);
        anyhow::bail!("this workspace backend sends no keys")
    }
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched. A
    /// pinned workspace is unpinned first, since cmux refuses to close one
    /// (ADR-0031); every close dagq makes goes through here.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Give the workspace a sidebar color: a cmux color name or `#RRGGBB`.
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()>;
    /// Show the status pill `key` with `value` and `icon` on the
    /// workspace's sidebar entry, replacing the pill under the same key.
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()>;
    /// Pin the workspace in the sidebar; pinning a pinned one is a no-op.
    fn pin(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way a person would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// [`send_exit`](Self::send_exit), made again after a timeout while
    /// `unsent` says from a screen read that the `/exit` did not get there
    /// (task 354). A backend that retries nothing sends it once.
    fn send_exit_when(&self, workspace_id: &str, unsent: &dyn Fn(&str) -> bool) -> Result<()> {
        let _ = unsent;
        self.send_exit(workspace_id)
    }
    /// Whether the workspace with this stable ID is still open. Workspaces
    /// are found by the ID the queue recorded, never by their title, which
    /// people may rename (ADR-0026).
    fn exists(&self, workspace_id: &str) -> Result<bool>;
    /// The stable IDs of every workspace cmux lists, in all its windows:
    /// one listing for many checks, as the supervisor's sweep of ended
    /// runs' workspaces makes.
    fn listed_workspace_ids(&self) -> Result<Vec<String>>;
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
    /// How many times in all a call that timed out is made when making it
    /// again is safe (a read, or a text that did not reach the screen;
    /// task 326).
    fn call_attempts(&self) -> u32 {
        3
    }
    /// The backoff before the first retry of a call that timed out,
    /// doubled before each next one.
    fn retry_backoff(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
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
    /// How long after a submit (and between the Enters sent again) the
    /// screen is read for the text left in the input box.
    fn submit_check_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
    /// How long a session sent a request or an answer may show no sign of
    /// work before the supervisor sends it again or asks the inbox.
    fn start_wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
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
    /// Collect the exit of `pid` if it is an ended child of this process,
    /// so it no longer counts as alive: a child the process started before
    /// it exec'd another binary (the automatic update's job, ADR-0045
    /// decision 17) has no other reaper. Nothing for any other process.
    fn reap(&self, pid: u32) {
        let _ = pid;
    }
    /// This user's processes with their parents, ages, commands and
    /// working directories, for the recovery job (ADR-0047 decision 39).
    fn list(&self) -> Result<Vec<crate::domain::recovery::ProcessInfo>> {
        anyhow::bail!("this process control cannot list processes")
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

/// Outcome of supervisor-side receipt validation. `result_commit` is kept on
/// rejection too when the commit itself was verified, so inspection can start there.
/// A rejection for nothing but `evidence_missing` (the task's required checks
/// the receipt does not back, ADR-0019 decision 5) parks the run as
/// `needs_session` instead of failing it, and so does one for a diff that
/// changes `scope_violation`, paths none of the task's `allowed_paths`
/// match (ADR-0029).
#[derive(Debug, Serialize)]
pub struct Validation {
    pub accepted: bool,
    pub result_commit: Option<CommitSha>,
    pub reason: Option<String>,
    /// The code of `reason` (ADR-0034); `None` for an accepted run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ReasonCode>,
    pub receipt: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_missing: Vec<EvidenceCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scope_violation: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// The load average from `receipt_observed` to this validation (task
    /// 197): the supervisor's samples, filled in when it records the result.
    #[serde(flatten)]
    pub load: crate::domain::measure::LoadSummary,
}

/// A workspace an ended run opened (the worker's or a resume's), which the
/// supervisor's sweep closes while cmux still lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedRunWorkspace {
    pub run_id: RunId,
    pub status: RunStatus,
    pub workspace_id: String,
}

/// The worktree of a run that nobody leases and that either ended or
/// belongs to a task that is over, for the supervisor's clean-up of the
/// disk (task 376).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedRunWorktree {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub task_status: TaskStatus,
    pub worktree: String,
    pub branch: Option<String>,
}

/// A `needs_session` run as the supervisor judges it for a resume.
#[derive(Debug, Clone)]
pub struct ResumeCandidate {
    pub run: TaskRun,
    pub lease: Option<RunLease>,
    /// The latest session's wrapper registration.
    pub wrapper: Option<RunProcess>,
    /// Its `resume_started` events so far, counted as ADR-0047 decision
    /// 24 says.
    pub resumes: crate::domain::resume::ResumeCount,
}

/// How a run whose resumes are used up ends (ADR-0047 decision 24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exhaustion {
    /// The run becomes `failed` and its `resume_exhausted` alert goes to
    /// the recovery job (ADR-0047 decision 39).
    Recover,
    /// The task is ready again, and its next run carries this run's
    /// `branch` over from `head` (its review passed; only conflicts kept
    /// it from landing).
    Inherit {
        branch: Option<String>,
        head: CommitSha,
    },
}

/// What a recovery round of a `failed` or `interrupted` run does to it
/// (ADR-0047 decision 40), recorded as `triage_finished`'s `action`: the
/// job's action once its preconditions held, or the ask it escalated to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageAction {
    /// The task goes back to `ready`; the next claim makes a new run.
    Retry,
    /// The task goes back to `ready`, and its next run carries this run's
    /// `branch` over from `head` (ADR-0047 decision 24).
    RetryInherit {
        branch: Option<String>,
        head: CommitSha,
    },
    /// The run becomes `needs_session` with `instruction` as `last_error`,
    /// and the supervisor resumes it (ADR-0019 decision 1).
    Resume { instruction: String },
    /// Nothing moves; the job runs again from `recheck_at` (unix seconds)
    /// if the run is still where it was.
    Wait { recheck_at: i64 },
    /// The run stays; the `decide` ask `ask_id` waits for a person.
    Ask { ask_id: AskId },
}

impl TriageAction {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::RetryInherit { .. } => crate::domain::resume::RETRY_INHERIT,
            Self::Resume { .. } => "resume",
            Self::Wait { .. } => "wait",
            Self::Ask { .. } => "ask",
        }
    }
}

/// `asked_by` of the triage's `decide` asks: the supervisor that triaged.
pub const TRIAGE_ASKER: &str = "supervisor";

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
    /// Mark `token`'s registration as one that takes a handoff.
    fn accept_handoff(&self, token: &str) -> Result<()>;
    /// Ask the supervisor `token` to exec `binary`; `false` when it is not
    /// registered or does not take a handoff (ADR-0045 decision 10).
    fn request_handoff(&self, token: &str, binary: &str) -> Result<bool>;
    /// The binary the supervisor `token` was asked to exec, if any.
    fn handoff_request(&self, token: &str) -> Result<Option<String>>;
    /// Withdraw a request to exec `binary` not taken yet.
    fn cancel_handoff(&self, token: &str, binary: &str) -> Result<bool>;
    /// Take `token`'s registration back under `binary_version` after an
    /// exec, clearing the request.
    fn resume_registration(
        &mut self,
        token: &str,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration>;
    /// Every run whose lease carries `token`, oldest first.
    fn runs_leased_by(&self, token: &str) -> Result<Vec<TaskRun>>;
    /// Turn the automatic update of the supervisor `token` on or off
    /// (ADR-0045 decision 17).
    fn set_auto_update(&self, token: &str, enabled: bool) -> Result<()>;
    /// Record the supervisor `token`'s `--max-waiting` (ADR-0062 decision 7).
    fn set_max_waiting(&self, token: &str, max_waiting: u32) -> Result<()>;
    /// The unclosed asks of the run, answered or not, oldest first.
    fn unclosed_run_asks(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>>;
    /// The latest `limit` steps of the automatic update (its `update_*`
    /// queue events), newest first.
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>>;
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
    fn abandon_run(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun>;
    fn run_leases(&self) -> Result<Vec<RunLease>>;
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>>;
    fn active_runs(&self) -> Result<Vec<TaskRun>>;
    /// Every run of the queue, oldest first.
    fn all_runs(&self) -> Result<Vec<TaskRun>>;
    /// Every run event, oldest first, for `stats`.
    fn all_events(&self) -> Result<Vec<RunEvent>>;
    /// Per task, its newest event of one of `kinds` (ADR-0069).
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>>;
    /// The commits that landed the `limit` completed tasks most related to
    /// `task` (`dagq related`, ADR-0046), for the files it is expected to
    /// touch when it declares no paths (ADR-0069).
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>>;
    /// The `limit` tasks most related to `task` in any status, best first,
    /// with their clues (`dagq related`, ADR-0046 decision 4).
    fn related_tasks(&self, task: TaskId, limit: usize) -> Result<RelatedPage>;
    /// The documents matching `query`, best first (`dagq search`, ADR-0046).
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage>;
    /// The goal of every task, for `stats`.
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>>;
    /// The title of every task, for `stats`.
    fn task_titles(&self) -> Result<HashMap<TaskId, String>>;
    /// The kind of every task (none for a task without one), for `stats`.
    fn task_kinds(&self) -> Result<HashMap<TaskId, Option<TaskKind>>>;
    /// Point the queue at `common_dir` whatever it was bound to, and return
    /// the previous binding (`rebind`, ADR-0020).
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>>;
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
        reason: &Reason,
    ) -> Result<TaskRun>;
    /// Record the landing: the run is integrated and its task completed.
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &str,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)>;
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()>;
    fn workspace_closed(&mut self, id: &RunId, token: &str) -> Result<TaskRun>;
    fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun>;
    /// Git common directory the queue is bound to, if any.
    fn repository_binding(&self) -> Result<Option<String>>;
    fn bind_repository(&mut self, common_dir: &str) -> Result<()>;
    fn assert_repository(&self, common_dir: &str) -> Result<()>;
    /// [`RunStore::claim_for_supervisor`], taking the first task of `order`
    /// that is still claimable, with `attributes` (an object) in its
    /// `run_claimed`.
    fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
        order: &[TaskId],
        attributes: Option<&serde_json::Value>,
    ) -> Result<ClaimOutcome>;
    /// One heartbeat of the process `token`: its registration and every
    /// lease it holds; how many leases there were.
    fn heartbeat(&mut self, token: &str) -> Result<usize>;
    /// The processes registered for the run (its wrapper and agent).
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>>;
    /// Record a runtime error on the run without changing its status.
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()>;
    /// Save the paths a claimed run is provisioned at.
    fn plan_run(&mut self, id: &RunId, token: &str, plan: &RunPlan) -> Result<()>;
    fn workspace_created(&mut self, id: &RunId, token: &str, workspace: &str) -> Result<()>;
    /// The session's wrapper exited: the run moves on by its exit code.
    fn finish_supervision(&mut self, id: &RunId, token: &str) -> Result<TaskRun>;
    /// The session went idle after its receipt and stays open: validating.
    fn finish_supervision_live(&mut self, id: &RunId, token: &str) -> Result<TaskRun>;
    fn finish_validation(
        &mut self,
        id: &RunId,
        token: &str,
        validation: &Validation,
    ) -> Result<TaskRun>;
    /// Validate a rewritten receipt again.
    fn restart_validation(&mut self, id: &RunId, token: &str) -> Result<TaskRun>;
    /// Apply a person's answer to an `approve_landing` ask.
    fn decide_landing(
        &mut self,
        id: &RunId,
        status: RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Park a run awaiting integration that the landing recheck found no
    /// longer landing on main (ADR-0068 decision 3), leased to `token` or
    /// to nobody; `None` when it is not so any more.
    fn park_rechecked(
        &mut self,
        id: &RunId,
        token: Option<&str>,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>>;
    /// The latest `failed` / `interrupted` run of every task in progress.
    fn runs_to_triage(&self) -> Result<Vec<TaskRun>>;
    /// Take the run's lease for a recovery round, recording `request` as
    /// its `recovery_requested` first when there is one; the round, or
    /// `None` when another process has it.
    fn begin_triage(
        &mut self,
        id: &RunId,
        token: &str,
        request: Option<serde_json::Value>,
    ) -> Result<Option<(TaskRun, usize)>>;
    /// Act on the round's outcome and record `triage_finished`, then each
    /// of `also` (kind, payload), in one transaction.
    fn finish_triage(
        &mut self,
        id: &RunId,
        token: &str,
        action: &TriageAction,
        payload: serde_json::Value,
        also: Vec<(&'static str, serde_json::Value)>,
    ) -> Result<TaskRun>;
    /// Record `workspace_closed` (`payload` and the `workspace_id`) of a
    /// workspace the triage or the supervisor's sweep closed.
    fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// The workspaces of the ended runs the triage does not take, for the
    /// supervisor's sweep.
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>>;
    /// The worktrees of the runs nobody leases that ended or whose task is
    /// `completed` / `canceled`, for the supervisor's clean-up of the disk.
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>>;
    /// Apply a person's answer to the triage's `decide` ask.
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun>;
    fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>>;
    /// Take the lease of a `needs_session` run for a resume; the attempt.
    fn begin_resume(
        &mut self,
        id: &RunId,
        token: &str,
        main: &CommitSha,
        reason: Option<&str>,
    ) -> Result<Option<(TaskRun, usize)>>;
    fn finish_resume(
        &mut self,
        id: &RunId,
        token: &str,
        status: Option<RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Move a run an earlier resume resolved on without a session.
    fn skip_resume(
        &mut self,
        id: &RunId,
        token: &str,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>>;
    /// Fail a run whose resumes are used up, naming the ask for a person
    /// or retrying its task with its branch carried over.
    fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
    ) -> Result<Option<TaskRun>>;
    /// When an observation of `mode` last started or finished.
    fn last_observe(&self, mode: &str) -> Result<Option<i64>>;
    /// The workspace `up` recorded for `role`.
    fn session_workspace(&self, role: SessionRole) -> Result<Option<String>>;
    /// Record the cmux workspace `up` opened for `role`, replacing any
    /// earlier one (ADR-0026).
    fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()>;
    /// Forget the workspace of `role`; `false` when none was recorded.
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool>;
    /// Forget the workspaces recorded for a role `up` no longer opens.
    fn forget_retired_session_workspaces(&self) -> Result<usize>;
    /// Record a new planner session (ADR-0041 decisions 1, 6) before its
    /// workspace opens.
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession>;
    /// The UUID of the workspace cmux opened for the planner.
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()>;
    /// Give the planner up; the first close and its error are kept.
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession>;
    fn planner(&self, id: PlannerId) -> Result<PlannerSession>;
    /// The planners not closed, oldest first; with `all`, every planner.
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>>;
    /// The planner's session wrapper registers itself, once.
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()>;
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()>;
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()>;
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()>;
    /// Record how `up` started the supervisor `token`.
    fn set_supervisor_mode(
        &self,
        token: &str,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()>;
    /// The newest `run_events` id, 0 for an empty queue.
    fn latest_event_id(&self) -> Result<EventId>;
    /// The latest run of every `in_progress` task, oldest first.
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>>;
    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push, oldest first.
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>>;
    fn register_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()>;
    fn register_resume_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()>;
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()>;
    fn register_resume_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32)
    -> Result<()>;
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()>;
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()>;
    /// The run whose workspace is `workspace_id`, the latest one first.
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>>;
    /// The leases `token` holds (every lease with `None`) and the
    /// `parallel` it registered (null without a supervisor).
    fn backend_slots(&self, token: Option<&str>) -> Result<(i64, Option<i64>)>;
    /// Record `backend_call_failed`, on `run` when the call was for one.
    fn record_backend_failure(&self, run: Option<&RunId>, payload: serde_json::Value)
    -> Result<()>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_queue_event(&self, kind: &str, payload: serde_json::Value) -> Result<EventId>;
    /// Record the finished transcript turns of the Claude session spans
    /// still open (ADR-0048 decision 8); returns how many spans got turns.
    fn record_session_turns(&self) -> Result<usize>;
    /// The newest event of `kind`, on whatever task, goal or run.
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>>;
    /// The newest event of the queue itself (on no run) of one of `kinds`.
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>>;
}

/// The questions the runtime and its sessions put to a person (ADR-0022).
pub trait AskStore {
    /// Open the ask of the automatic update of `kind` (`update_failed` or
    /// `approve_update`), closing an older open one of it (ADR-0073
    /// decision 17).
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
    ) -> Result<crate::domain::Ask>;
    /// The answered, unclosed asks of the automatic update of `kind`,
    /// oldest first.
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>>;
    /// Asks matching `query`, oldest first.
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>>;
    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool>;
    /// Register an ask, or return the open one it repeats.
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome>;
    /// Open the authentication or cost ask of the hold with its run, or add
    /// the run to the open one (ADR-0047 decision 42).
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome>;
    /// The open `queue_hold` ask that holds the run, if any.
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>>;
    fn read_ask(&self, id: AskId) -> Result<Ask>;
    /// Write the answer of an open ask, given by `answered_by` (a role,
    /// `person` or `runtime`).
    fn answer_as(&mut self, id: AskId, text: &str, answered_by: &str) -> Result<Ask>;
    fn close_ask(&mut self, id: AskId) -> Result<Ask>;
    /// Answered `approve_landing` asks nobody closed.
    fn landing_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `decide` asks of the triage nobody closed.
    fn triage_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `worker_question` asks of the run not yet delivered.
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>>;
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask>;
    fn has_stuck_exit_ask(&self, run_id: &RunId) -> Result<bool>;
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool>;
    /// When the run's `worker_question` closed last (unix seconds).
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>>;
    /// Close the run's `stuck_exit` asks nobody closed, with `answer`.
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the run's `answer_prompt` asks nobody closed, with `answer`.
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// The run's `stalled` ask nobody closed, answered or not.
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>>;
    /// Close the run's `stalled` asks nobody closed, with `answer`.
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Add `note` as a paragraph to the question of every ask of the run
    /// nobody closed, recording `ask_updated` with `why`; the asks noted.
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>>;
    /// Close the run's `approve_landing` asks nobody closed, with `answer`.
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
}

/// Which asks [`AskStore::asks`] lists. By default the ones nobody closed;
/// `all` adds the closed ones, `open` keeps only the unanswered ones, and
/// `role` keeps those that wait for that role ([`Ask::waits_for`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AskQuery {
    pub all: bool,
    pub open: bool,
    pub role: Option<SessionRole>,
}

/// How [`DraftPlannerStore::open_draft_planner`] ended.
#[derive(Debug, Clone)]
pub enum DraftPlannerStart {
    /// A planner of the runtime's is recorded for the draft, its
    /// `attempt`-th; the caller opens its workspace.
    Opened {
        planner: PlannerSession,
        target: Box<DraftTarget>,
        attempt: usize,
    },
    /// [`crate::domain::MAX_DRAFT_PLANNERS`] planners ended without
    /// deciding the draft: `draft_planner_exhausted` is recorded and a
    /// person decides.
    Exhausted { attempts: usize },
    /// Not now: the draft moved on, or another planner took it.
    Skipped,
}

/// Where the answer of a `planner_question` goes (ADR-0041 decision 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerAnswerRoute {
    /// Typed into the workspace of this planner of the runtime's, which
    /// works on the ask's task (its draft, or its proposal).
    Planner(PlannerSession),
    /// The draft's planner is gone: a new one is opened with the answer.
    NewPlanner,
    /// The draft moved on (submitted, canceled) with no planner left: the
    /// supervisor closes the ask.
    Close,
    /// None of these: a person delivers it through the inbox.
    Person,
}

/// The drafts the runtime or a job registered (ADR-0041 decision 16):
/// where each came from, the planner of the runtime's opened for each, and
/// the `planner_question` answers the supervisor types into a planner.
pub trait DraftPlannerStore {
    /// Record where a draft the runtime or a job registered came from, and
    /// what its planner is shown about it. A draft has one origin: a second
    /// call for it is refused.
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &serde_json::Value,
    ) -> Result<()>;
    /// Where the draft came from, if the runtime or a job registered it.
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, serde_json::Value)>>;
    /// The drafts waiting for a planner of the runtime's, by ID: `draft`,
    /// in no proposal, with an origin, no planner open for it, no
    /// `planner_question` about it nobody closed, not kept as a draft by
    /// an answer and not exhausted.
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>>;
    /// Record a planner of the runtime's for `draft` (`draft_planner_opened`)
    /// after re-checking it in the same write transaction. With `answer`,
    /// the planner carries that answered `planner_question` about the draft,
    /// whose planner is gone, instead of the draft being a target.
    fn open_draft_planner(
        &mut self,
        draft: TaskId,
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart>;
    /// Answered `planner_question` asks nobody closed, oldest first.
    fn planner_answers(&self) -> Result<Vec<Ask>>;
    /// Where the answer of an answered `planner_question` goes.
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute>;
    /// Close an answered `planner_question` nobody needs any more (its
    /// draft moved on), recording `planner_answer_closed` with `why`.
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()>;
    /// Record an event of a task that has no run (a draft's).
    fn record_task_event(
        &mut self,
        task: TaskId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Drafts still `draft` whose planners were used up
    /// (`draft_planner_exhausted`), by ID.
    fn exhausted_drafts(&self) -> Result<Vec<Task>>;
    /// The `follow_up_depth` of a task (ADR-0037 decision 6).
    fn follow_up_depth(&self, task: TaskId) -> Result<i64>;
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()>;
}

/// A plan review job the queue recorded (ADR-0041 decision 11): the
/// proposal it reviews, its attempt at that proposal, the first task of the
/// proposal (where its events are recorded) and its directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReviewJob {
    pub id: i64,
    pub proposal_id: ProposalId,
    pub attempt: usize,
    pub anchor: TaskId,
    pub dir: PathBuf,
    /// The Claude session id the job is started with (ADR-0048 decision 4).
    pub session_id: String,
}

/// What the runtime makes of a plan review's verdict before it is applied:
/// the decision it acts on (a `revise` past [`crate::domain::MAX_PLAN_REVISES`]
/// is a `concern`, `overridden` saying why), the reasons a revise carries to
/// the planner (the verdict's, then the precedents it named), and the
/// `approve_plan` ask a concern opens.
#[derive(Debug, Clone)]
pub struct PlanReviewApply {
    pub verdict: PlanReviewVerdict,
    pub decision: PlanReviewDecision,
    pub overridden: Option<String>,
    pub revise_reasons: Vec<String>,
    pub ask: Option<NewAsk>,
    pub duration_secs: u64,
}

/// A ready task a verdict took back to submitted, with the proposal of its
/// own a planner fixes it in (ADR-0041 decision 14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReopenedTask {
    pub task_id: TaskId,
    pub proposal_id: ProposalId,
}

/// What applying a verdict did. `stale`: nothing, because the job's row or
/// its proposal moved on meanwhile (the job is finished as `interrupted`).
#[derive(Debug, Clone, Default)]
pub struct PlanReviewApplied {
    pub stale: bool,
    pub reopened: Vec<ReopenedTask>,
    /// The `approve_plan` ask a concern opened; `created: false` when an
    /// open one already stood.
    pub ask: Option<AskOutcome>,
}

/// A proposal sent back to its planner (`revising`), with where its revise
/// stands: the reasons, when and to which planner they went (`None`: the
/// supervisor still has to deliver them), and when the inbox was told the
/// planner did not answer.
#[derive(Debug, Clone)]
pub struct RevisingProposal {
    pub proposal: Proposal,
    pub reasons: Vec<String>,
    /// Since when the revise waits (Unix seconds).
    pub revised_at: Option<i64>,
    pub sent_at: Option<i64>,
    pub planner_id: Option<PlannerId>,
    pub unresponsive_at: Option<i64>,
}

/// A proposal that waits for a person outside an ask: its plan review
/// failed (`plan_review_failed`), or its planner did not answer a revise
/// (`planner_unresponsive`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewHold {
    pub proposal_id: ProposalId,
    pub anchor: TaskId,
    pub kind: &'static str,
    pub error: Option<String>,
}

/// What a person's `approve_plan` answer did to the proposal.
#[derive(Debug, Clone)]
pub struct PlanDecided {
    pub proposal: Proposal,
    pub answer: String,
}

/// Plan review (ADR-0041 decisions 11-15, 17): the submitted proposals it
/// takes, its one job at a time, the verdicts and answers the runtime
/// applies (each in one transaction), and the revises it delivers.
pub trait PlanReviewStore {
    /// Submitted proposals plan review may take now: not held, with a
    /// submitted task, and whether one of those has the interrupt priority.
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>>;
    /// Record the start of a plan review of `proposal` by `token`, in the
    /// directory named by its row's ID under `plan_reviews_dir`
    /// (`plan_review_started`, with `cwd`, the checkout the job runs in, for
    /// its Claude session: ADR-0048). Rows of gone supervisors are finished
    /// as `interrupted` first. `None`: another supervisor's job runs, or the
    /// proposal is no longer a candidate.
    fn begin_plan_review(
        &mut self,
        proposal: ProposalId,
        token: &str,
        plan_reviews_dir: &Path,
        cwd: &Path,
    ) -> Result<Option<PlanReviewJob>>;
    /// Apply what the runtime made of the verdict and finish the job
    /// (`plan_review_finished`); an action the job may not take is an
    /// error, and nothing is applied.
    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &str,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied>;
    /// Record the job's failure (`plan_review_failed`, the inbox's) and
    /// hold the proposal as `failed`, unless the job moved on.
    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &str,
        error: &str,
        duration_secs: u64,
    ) -> Result<()>;
    /// Every proposal sent back to its planner, oldest first.
    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>>;
    /// Claim the delivery of the revise of `proposal` (it records when):
    /// `false` when another process claimed it, or it is no longer waiting.
    fn claim_revise(&mut self, proposal: ProposalId) -> Result<bool>;
    /// The claimed revise of `proposal` went to `planner`
    /// (`plan_revise_sent`); `opened` when the runtime opened that planner
    /// for it.
    fn revise_sent(
        &mut self,
        proposal: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: bool,
    ) -> Result<()>;
    /// Take the revise of `proposal` back for another delivery: the
    /// planner it went to is gone before it submitted again
    /// (`plan_revise_lost`), or the delivery failed (`planner` `None`).
    fn revise_lost(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()>;
    /// No planner submitted the proposal again within the timeout
    /// (`planner_unresponsive`, the inbox's), once per revise; `planner`
    /// is `None` when none took the revise yet.
    fn planner_unresponsive(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
    ) -> Result<()>;
    /// End the submitted proposals none of whose tasks waits for plan
    /// review any more (a person readied them with the bypass or canceled
    /// them): `accepted` when a task is left, `canceled` otherwise
    /// (`proposal_settled`).
    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, crate::domain::ProposalStatus)>>;
    /// Answered `approve_plan` asks nobody closed, oldest first.
    fn plan_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to an `approve_plan` ask and close it.
    /// `None` when the answer is not one the runtime applies (left to the
    /// inbox), or the proposal no longer waits for it (the ask is closed).
    fn decide_plan(&mut self, ask: AskId) -> Result<Option<PlanDecided>>;
    /// Whether the supervisor applies the answer the `approve_plan` ask has.
    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool>;
    /// The proposals held for a person outside an ask.
    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>>;
    /// Asks a person answered, newest first, at most `limit`: the
    /// precedents plan review may cite.
    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>>;
}

/// The queue a use case works on: its tasks and goals, its runs and its asks.
pub trait Queue: TaskStore + RunStore + AskStore + DraftPlannerStore + PlanReviewStore {}

impl<T: TaskStore + RunStore + AskStore + DraftPlannerStore + PlanReviewStore + ?Sized> Queue
    for T
{
}

/// The Git operations `integrate` and the supervisor use on the repository
/// the queue is bound to and on its run worktrees. Commits are named by
/// their full SHA; a failed Git command is an error with Git's message.
pub trait Repository {
    /// Current `refs/heads/main`, read again on every call.
    fn main_head(&self) -> Result<CommitSha>;
    /// Main's first-parent history since `since` (unix seconds) and the
    /// paths it has now, for `conflict_hotspots`; a repository that cannot
    /// tell has none.
    fn main_history(&self, since: i64) -> Result<crate::domain::stats::conflicts::MainHistory> {
        let _ = since;
        anyhow::bail!("this repository keeps no history")
    }
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
    /// Paths `to` adds over `from`.
    fn added_paths(&self, from: &str, to: &str) -> Result<Vec<String>>;
    /// Paths of the files directly in `directory` of `commit`'s tree; none
    /// when it has no such directory.
    fn paths_in(&self, commit: &str, directory: &str) -> Result<Vec<String>>;
    /// Which of `paths` contain the text `needle` in `commit`'s tree.
    fn paths_containing(&self, commit: &str, needle: &str, paths: &[String])
    -> Result<Vec<String>>;
    /// Move `from` to `to` in the clean `worktree` and commit the rename on
    /// its branch, `paragraphs` its message; the new head.
    fn rename_and_commit(
        &self,
        worktree: &Path,
        from: &str,
        to: &str,
        paragraphs: &[String],
    ) -> Result<CommitSha>;
    fn tree_of(&self, commit: &str) -> Result<String>;
    /// One commit with `tree` on top of `parent`, `paragraphs` its message.
    fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<CommitSha>;
    fn update_ref(&self, name: &str, value: &str) -> Result<()>;
    /// Fast-forward `refs/heads/main` from `from` to `to`.
    fn advance_main(&self, from: &str, to: &str) -> Result<()>;
    /// Point the repository's record of a moved worktree at it again.
    fn repair_worktree(&self, worktree: &Path) -> Result<()>;
    /// Remove the worktree and its branch; a branch already gone is not
    /// an error.
    fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()>;
    /// Whether Git tracks any file at or under `path` (relative to the
    /// worktree) in `worktree`.
    fn tracks(&self, worktree: &Path, path: &str) -> Result<bool>;
    /// The worktree that has `main` checked out, if any.
    fn main_checkout(&self) -> Result<Option<std::path::PathBuf>>;
    /// Add the run's worktree on its new branch from its base commit; Git's
    /// output.
    fn create_worktree(&self, run: &TaskRun) -> Result<String>;
    /// The paths `git merge-tree` finds conflicting between two commits,
    /// without touching a worktree; empty when they merge cleanly.
    fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>>;
    /// The tree of `head` merged with `main` without touching a worktree:
    /// `Ok(tree)` when they merge cleanly, `Err(paths)` when they conflict.
    fn merged_tree(
        &self,
        main: &str,
        head: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let _ = (main, head);
        anyhow::bail!("this repository cannot merge trees")
    }
    /// Check `commit` out detached in the scratch worktree at `path`,
    /// adding it when missing and clearing what its last use left.
    fn checkout_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        let _ = (path, commit);
        anyhow::bail!("this repository keeps no scratch worktree")
    }
    /// The tasks landed between two commits, oldest first, from their
    /// `Dagq-Task` trailers.
    fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>>;
    /// `git log --oneline <base>..<head>`.
    fn log_oneline(&self, base: &str, head: &str) -> Result<String>;
    /// `git diff --stat <base>...<head>`.
    fn diff_stat(&self, base: &str, head: &str) -> Result<String>;
    /// The size of the diff `<base>...<head>`.
    fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers>;
    /// Write the full diff `<base>...<head>` to a new file at `path`, as
    /// Git's raw bytes and never through memory.
    fn diff_to_file(&self, base: &str, head: &str, path: &Path) -> Result<()>;
}

/// The size of a diff, as `git diff --numstat` counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiffNumbers {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

/// Runs a task's verification commands for `integrate` (ADR-0023
/// decision 1) with the `[run.env]` of the repository (ADR-0023 decision 3).
pub trait Verifier {
    /// The environment of the run whose directory is `run_dir`.
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>>;
    /// Whether the programs that environment names resolve on this
    /// process's `PATH` (ADR-0049 decision 9); `None` is before any run.
    fn run_env_programs(
        &self,
        run_dir: Option<&Path>,
    ) -> Result<crate::domain::run_env::RunEnvCheck>;
    /// `[run.env]` of the main checkout's `dagq.toml` as written (values
    /// unexpanded), `None` without the file: what the supervisor hashes for
    /// the `run_env_changed` mark (ADR-0051 decision 11).
    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>>;
    /// The queue's secret salt of the `[run.env]` hashes, made on first use
    /// and kept outside the events (ADR-0051 decision 11).
    fn run_env_salt(&self) -> Result<String>;
    /// The command the landing recheck runs on main's tree with a waiting
    /// run merged in (`[recheck] command` of `dagq.toml`, ADR-0068 decision
    /// 2); `None` checks the merge only.
    fn recheck_command(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// Run `command` in a shell in `cwd` with `env`, its output in `log`.
    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Exit>;
}
