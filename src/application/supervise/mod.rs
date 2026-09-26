//! The supervisor (ADR-0024 decision 1): execute claimed tasks in parallel,
//! validate their receipts, review accepted runs headless, land them on
//! main one at a time, triage failed ones and resume parked ones. One run's
//! state machine is unchanged from the single-run supervisor; the loop
//! multiplexes independent slots and isolates failures. A run whose
//! supervisor died while its session lives on is adopted by a supervisor
//! with a free slot instead of being rerun (ADR-0012).
//!
//! Everything outside the process reaches the loop through ports: the
//! queue ([`Queue`], a connection per thread from [`QueueOpener`]), Git
//! ([`Repository`], [`MainRemote`]), the verification commands
//! ([`Verifier`]), cmux ([`WorkspaceBackend`]), the agent
//! ([`AgentProvider`]) and what it shows and writes ([`AgentSignals`]),
//! the processes it starts ([`Spawner`]) and checks ([`ProcessControl`]),
//! the run files ([`RunFiles`]) and the time and IDs ([`Generators`]).
//! Progress and diagnostics are `tracing` events with `run_id` /
//! `task_id` / `ask_id` / `error` fields; the entry point picks their
//! subscriber (ADR-0033).
//!
//! This module holds the loop and the state machine of a slot (`Phase`,
//! `step`); each phase's watch and the supervisor's methods for it are in
//! the submodules: `session` (the worker's session), `exit` (its `/exit`),
//! `jobs` (the headless review and triage), `landing` (the review verdict
//! and the landing), `revise`, `resume`, `triage`, `adopt` and `idle` (the
//! idle marker). The prompts and requests are in [`super::prompt`].

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};

use super::{
    AgentProvider, AgentSignals, CommandSpec, Exhaustion, Generators, IdleHook, LeasedRun,
    MainRemote, ProcessControl, Queue, QueueOpener, Repository, ResumeCandidate, RunFiles, Spawned,
    Spawner, Streams, TRIAGE_ASKER, TriageAction, Validation, Verifier, WorkspaceBackend,
    WorkspaceTags, ask, dependency_graph,
    health::{lease_health, run_health},
    integrate::{self as integration, Integration, check_receipt, resume_attempts},
    naming::{
        resume_workspace_description, shell_join, workspace_description, workspace_group_name,
    },
    or_none, path_text,
    prompt::{
        GoalPredecessorSummary, Inheritance, PredecessorSummary, RecoveryMaterial, ResumeKind,
        ResumeRequest, TRIAGE_TOOLS, prompt, recovery_prompt, resume_request, review_prompt,
        revise_mismatch_request, revise_request, siblings_in_progress, stale_receipt_nudge,
        stall_nudge, triage_prompt,
    },
    recording::{
        RecordingBackend, exit_unsent, reason_of_error, text_on_screen, timed_out_maybe_sent,
    },
    tail, unix_seconds,
};
use crate::domain::{
    AskId, AskKind, AskReason, ClaimOutcome, CommitSha, EventId, EvidenceCheck,
    HEARTBEAT_TIMEOUT_SECS, HOLD_OPTIONS, IntegrationOutcome, LANDING_OPTIONS, MAX_RESUME_ATTEMPTS,
    MAX_REVISE_ATTEMPTS, NewAsk, NewHold, Predecessor, Reason, ReasonCode, Receipt, ReceiptResult,
    ReviewDecision, ReviewVerdict, RunId, RunLease, RunPaths, RunPlan, RunProcess, RunStatus,
    SessionRole, TRIAGE_OPTIONS, TRIAGE_RETRY_FAILURES, TaskAction, TaskId, TaskRun, TaskStatus,
    TriageDecision, TriageState, TriageVerdict, heartbeat_stale,
    marks::{RUN_ENV_CHANGED, SUPERVISOR_STARTED, SUPERVISOR_STOPPED, run_env_digest},
    measure::{ClaimAttributes, HostVersions, LoadSummary, LoadWindow},
    recovery::RecoveryDecision,
    resume::{ResumeCount, inherits_on_exhaustion},
    run_env::RUN_ENV_PROGRAM_KINDS,
    stall::{BackgroundTask, STALL_CONFIG_LOADED, StallConfig},
    triage_state,
};

mod adopt;
mod deliver;
mod dialog;
mod draft_planner;
mod exit;
mod handoff;
mod idle;
mod jobs;
mod landing;
mod plan_review;
mod recheck;
mod recovery;
mod resume;
mod revise;
mod session;
mod stale;
mod stall;
mod sweep;
mod triage;
mod update;
mod waiting;

pub use self::handoff::SUPERVISOR_HANDED_OFF;
pub use self::update::{UPDATE_INTERVAL, UpdateSettings};
use self::{
    deliver::*, dialog::*, exit::*, idle::*, jobs::*, recovery::*, resume::*, revise::*,
    session::*, stale::*, stall::*, sweep::*, waiting::*,
};

/// How often the supervisor records the finished transcript turns of the
/// session spans still open (ADR-0048 decision 8).
pub const SESSION_TURNS_INTERVAL: Duration = Duration::from_secs(600);

/// How far back the daily observation reads.
pub const DAILY_WINDOW_SECS: i64 = 24 * 60 * 60;

/// The two observations: the hourly one reads what finished since the last
/// one (its cursor), the daily one the last 24 hours for trends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveMode {
    Hourly,
    Daily,
}

impl ObserveMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
        }
    }
}

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct LoopSettings {
    /// Upper bound on runs executing at once.
    pub parallel: usize,
    /// Upper bound on the runs waiting for a person outside the slots
    /// (ADR-0062 decision 7); zero keeps every run in its slot.
    pub max_waiting: usize,
    /// Exit when no run is active and no task can be claimed, instead of
    /// polling for new work.
    pub once: bool,
    pub stop: Arc<AtomicBool>,
    /// Start the observer job when this long passed since the last one
    /// started or finished (ADR-0024 decision 4); zero disables the
    /// observer, the daily one included.
    pub observe_interval: Duration,
    /// Also run the daily observation once every 24 hours.
    pub observe_daily: bool,
    /// Pause between two passes over the active runs; tests shorten it.
    pub tick: Duration,
    /// Pause between two looks for claimable work while no run is active.
    pub idle_poll: Duration,
    /// Least time between two sweeps of the workspaces of ended runs; the
    /// first pass sweeps at once.
    pub sweep_interval: Duration,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4),
    /// recorded as `stall_config_loaded` when the loop starts.
    pub stall: StallConfig,
    /// The thresholds of the `conflict_hotspot` alert, for the files the
    /// plan review is told conflict often.
    pub conflicts: crate::domain::stats::ConflictConfigReport,
    /// Upper bound on the planners the runtime has open at once (ADR-0041
    /// decision 12), apart from the run slots; planners a person opened
    /// do not count.
    pub runtime_planners: usize,
    /// How long a planner a revise went to may take to submit its proposal
    /// again before the inbox is told (ADR-0041 decision 13).
    pub planner_timeout: Duration,
    /// The token of the supervisor this process continues after an exec
    /// (ADR-0045 decision 10); `None` registers a new one.
    pub handoff_token: Option<String>,
    /// How `up` started this process (`supervise --mode`): what its start
    /// mark records, since `up` writes the registration's mode only after
    /// it sees the registration (ADR-0051 decision 10).
    pub mode: Option<crate::domain::SupervisorMode>,
    /// The automatic update of this supervisor's binary (ADR-0045
    /// decision 17).
    pub update: UpdateSettings,
}

/// Where the supervisor works and what it starts: the queue database and
/// its run directory, the repository, the binaries a session and the
/// observer run, this process, and the environment of the processes it
/// starts. Paths only; nothing here is read or written by the use case.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The queue database, canonical.
    pub db: PathBuf,
    /// Where the run directories are made (`<queue dir>/runs`).
    pub runs_dir: PathBuf,
    /// The queue hash: the external ID of the queue's workspace group and
    /// part of every run workspace's description (ADR-0026).
    pub queue_hash: String,
    /// The checkout `supervise` was given, and its Git common directory.
    pub repo_root: PathBuf,
    pub common_dir: PathBuf,
    /// The `claude` the run sessions start.
    pub claude: PathBuf,
    /// The runtime binary the sessions and the observer run (snapshotted
    /// into each run directory).
    pub runner: PathBuf,
    /// This process and its binary's version, recorded on the registration.
    pub pid: u32,
    pub version: String,
    /// `DAGQ_ROLE` / `DAGQ_QUEUE` of a worker's workspace.
    pub worker_env: Vec<(String, String)>,
    /// The role and queue a headless review or triage runs under: the CLI
    /// knows the job by its role and allows it only reads of this queue.
    pub job_env: Vec<(String, String)>,
    /// Variables the observer's process does not inherit (its role).
    pub observer_env_remove: Vec<String>,
    /// `planners/` of the queue, where the planners the runtime opens keep
    /// their files, and the plugin directory they load.
    pub planners_dir: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    /// `plan-reviews/` of the queue: one directory per plan review job.
    pub plan_reviews_dir: PathBuf,
}

/// The ports the supervisor works through, and where it works.
pub struct Ports<'a> {
    pub queues: Arc<dyn QueueOpener>,
    pub repository: Arc<dyn Repository + Send + Sync>,
    pub remote: Arc<dyn MainRemote + Send + Sync>,
    pub verifier: Arc<dyn Verifier + Send + Sync>,
    pub cmux: &'a dyn WorkspaceBackend,
    /// The agent of the run sessions, checked before anything is claimed.
    pub agent: &'a dyn AgentProvider,
    /// Reads the screen and the idle marker of that agent's sessions.
    pub signals: &'a dyn AgentSignals,
    /// Starts the headless review and triage (ADR-0027, ADR-0024).
    pub reviewer: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: Arc<dyn RunFiles>,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub generators: Generators,
    /// Writes a task's review material (`review`) and reports its path.
    pub review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    /// The log of this start, given the registration's `started_at`.
    /// The 1-minute load average recorded with a failed cmux call, at a
    /// claim and over each interval of a run.
    pub load_average: fn() -> Option<f64>,
    /// The versions of Claude Code (given `--claude`) and of the host's
    /// `rustc` (run in the given checkout) a claim records (task 197).
    pub host_versions: fn(&Path, &Path) -> HostVersions,
    pub layout: Layout,
}

/// Spawn a thread that reports its `tracing` events to the subscriber of
/// the spawning thread, so a supervisor run under a scoped subscriber (the
/// tests) keeps the events of its heartbeat, validations and landings.
pub fn spawn_traced<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || tracing::dispatcher::with_default(&dispatch, work))
}

/// One process heartbeats its registration (a resident supervisor) and every
/// lease it holds with a single token.
pub struct Heartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Heartbeat {
    pub fn start(queues: Arc<dyn QueueOpener>, token: String) -> Self {
        let (stop, recv) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let worker = spawn_traced(move || {
            let result = (|| -> Result<()> {
                let mut queue = queues.open()?;
                loop {
                    queue.heartbeat(&token)?;
                    match recv.recv_timeout(Duration::from_secs(2)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        _ => break,
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                error!(error = %format_args!("{error:#}"), "supervisor heartbeat failed: {error:#}");
                flag.store(true, Ordering::SeqCst);
            }
        });
        Self {
            stop,
            worker: Some(worker),
            failed,
        }
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.failed.load(Ordering::SeqCst),
            "supervisor heartbeat failed; preserving runs for inspection"
        );
        Ok(())
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A run this supervisor gave up on; it keeps its status, lease-less, with
/// the message in `last_error`.
#[derive(Debug, Clone, Serialize)]
pub struct RunError {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub message: String,
}

/// Run and monitor tasks until the loop ends: with `once`, when nothing is
/// active or claimable; otherwise on `stop`, or after a provisioning failure
/// has drained the active runs (an error). Every task unblocked by `integrate`
/// is picked up on a later pass with the then-current `main` as its base.
/// Accepted runs are reviewed headless by the reviewer (ADR-0027).
pub fn supervise(ports: &Ports<'_>, settings: &LoopSettings) -> Result<Value> {
    ensure!(settings.parallel >= 1, "parallel must be at least 1");
    let layout = &ports.layout;
    ensure!(
        !layout.db.starts_with(&layout.repo_root) || layout.db.starts_with(&layout.common_dir),
        "keep the queue outside the worktree or under its Git common directory"
    );
    ports.cmux.preflight()?;
    ports.agent.preflight()?;
    let mut queue = ports.queues.open()?;
    queue.bind_repository(&path_text(&layout.common_dir)?)?;
    let parallel =
        u32::try_from(settings.parallel).context("parallel does not fit a registration")?;
    let pid = layout.pid;
    let mut previous_version = None;
    let token = match &settings.handoff_token {
        // This process exec'd this binary under the registration it had
        // (ADR-0045 decision 10): the same token, pid, mode and leases.
        Some(token) => {
            previous_version = queue
                .supervisors()?
                .into_iter()
                .find(|registration| &registration.token == token)
                .and_then(|registration| registration.binary_version);
            queue.resume_registration(token, pid, &layout.version)?;
            let previous = &previous_version;
            info!(
                "supervisor {token} handed off: version {} (was {}), pid {pid}, parallel {parallel}, db {}, repository {}",
                layout.version,
                previous.as_deref().unwrap_or("unrecorded"),
                layout.db.display(),
                layout.repo_root.display()
            );
            token.clone()
        }
        None => {
            let token = ports.generators.ids.uuid();
            // Registered before the first heartbeat so the loop is visible
            // to `status` from its first second, runs or not.
            queue.register_supervisor(&token, pid, parallel, &layout.version)?;
            queue.accept_handoff(&token)?;
            if settings.update.register {
                queue.set_auto_update(&token, true)?;
            }
            info!(
                "supervisor {token} started: version {}, pid {pid}, parallel {parallel}, db {}, repository {}",
                layout.version,
                layout.db.display(),
                layout.repo_root.display()
            );
            token
        }
    };
    // Next to `parallel` on the registration, for `status` (ADR-0062
    // decision 7); written again by the process an exec continues.
    queue.set_max_waiting(
        &token,
        u32::try_from(settings.max_waiting).context("max-waiting does not fit a registration")?,
    )?;
    let mut config = serde_json::to_value(settings.stall)?;
    config["supervisor"] = json!(token);
    queue.record_queue_event(STALL_CONFIG_LOADED, config)?;
    // The mark of this start or handoff (ADR-0051 decision 10): the mode
    // `up` passed, else the registration's (a handoff keeps it).
    let registration = queue
        .supervisors()?
        .into_iter()
        .find(|registration| registration.token == token);
    queue.record_queue_event(
        SUPERVISOR_STARTED,
        json!({
            "supervisor": token,
            "dagq_version": layout.version,
            "parallel": parallel,
            "mode": settings
                .mode
                .or(registration.as_ref().and_then(|r| r.mode))
                .map(|mode| mode.as_str()),
            "auto_update": registration.as_ref().is_some_and(|r| r.auto_update),
            "handoff": settings.handoff_token.is_some(),
            "previous_version": previous_version,
        }),
    )?;
    let heartbeat = Heartbeat::start(ports.queues.clone(), token.clone());
    let cmux = RecordingBackend::over(
        ports.cmux,
        ports.queues.clone(),
        Some(token.clone()),
        ports.load_average,
    );
    let mut supervisor = Supervisor {
        queue,
        queues: ports.queues.clone(),
        layout,
        repository: ports.repository.clone(),
        remote: ports.remote.clone(),
        verifier: ports.verifier.clone(),
        cmux: &cmux,
        reviewer: ports.reviewer,
        signals: ports.signals,
        spawner: ports.spawner,
        files: ports.files.clone(),
        processes: ports.processes.clone(),
        review_material: ports.review_material,
        token,
        heartbeat,
        slots: Vec::new(),
        parallel: settings.parallel,
        max_waiting: settings.max_waiting,
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
        observer: None,
        observers_launched: Vec::new(),
        last_sweep: None,
        last_turns: None,
        sweep_failures: Vec::new(),
        triaged: Vec::new(),
        generators: ports.generators.clone(),
        stall: settings.stall,
        conflicts: settings.conflicts,
        plan_review: None,
        planner_exits: Vec::new(),
        handoff: None,
        exec: None,
        run_env_missing: false,
        draining: false,
        update: update::UpdateWatch::default(),
        rechecks: recheck::Rechecks::default(),
        load_average: ports.load_average,
        host_versions: ports.host_versions,
        loads: HashMap::new(),
    };
    if settings.handoff_token.is_some() {
        supervisor.rebuild_own_runs(previous_version.as_deref())?;
    }
    let result = supervisor.run_loop(settings);
    match &result {
        Ok(value) => info!("supervisor {} exiting: {value}", supervisor.token),
        Err(error) => {
            error!(error = %format_args!("{error:#}"), "supervisor {} failed: {error:#}", supervisor.token)
        }
    }
    result
}

struct Supervisor<'a> {
    queue: Box<dyn Queue + Send>,
    /// A connection for each thread beside the loop.
    queues: Arc<dyn QueueOpener>,
    layout: &'a Layout,
    repository: Arc<dyn Repository + Send + Sync>,
    remote: Arc<dyn MainRemote + Send + Sync>,
    verifier: Arc<dyn Verifier + Send + Sync>,
    cmux: &'a dyn WorkspaceBackend,
    /// Starts the headless review of accepted runs (ADR-0027).
    reviewer: &'a dyn AgentProvider,
    /// Reads the screen and the idle marker of the run sessions.
    signals: &'a dyn AgentSignals,
    spawner: &'a dyn Spawner,
    files: Arc<dyn RunFiles>,
    processes: Arc<dyn ProcessControl + Send + Sync>,
    review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    token: String,
    heartbeat: Heartbeat,
    slots: Vec<Slot>,
    /// `--parallel`: the slots in use (the runs not waiting) are held under
    /// it, apart from a run a person moved (ADR-0062 decision 10).
    parallel: usize,
    /// `--max-waiting` (ADR-0062 decision 7).
    max_waiting: usize,
    finished: Vec<TaskRun>,
    errors: Vec<RunError>,
    /// Cleared after a provisioning failure so an unavailable cmux or Git
    /// does not burn through every candidate.
    claiming: bool,
    provisioning_error: Option<String>,
    /// The observer job running now: one at a time, outside the run slots.
    observer: Option<(ObserveMode, Box<dyn Spawned>)>,
    /// When this process last launched each observation, so one that dies
    /// before it records anything is not relaunched on every pass.
    observers_launched: Vec<(ObserveMode, Instant)>,
    /// When this process last swept the workspaces of ended runs
    /// (`LoopSettings::sweep_interval`); `None` until the first pass sweeps.
    last_sweep: Option<Instant>,
    /// When this process last recorded the transcript turns of the open
    /// session spans (ADR-0048 decision 8); `None` until the first pass.
    last_turns: Option<Instant>,
    /// The workspaces the sweep could not close: retried on every sweep,
    /// their `cleanup_failed` recorded once per process.
    sweep_failures: Vec<String>,
    /// The runs this process triaged, with where each one went.
    triaged: Vec<Value>,
    /// The clock and IDs `queue` also uses.
    generators: Generators,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4).
    stall: StallConfig,
    /// The `[conflicts]` thresholds the plan review's hotspots are judged by.
    conflicts: crate::domain::stats::ConflictConfigReport,
    /// The plan review job running now (ADR-0041 decision 11): one at a
    /// time, queue-wide, outside the run slots.
    plan_review: Option<plan_review::PlanReviewWatch>,
    /// The runtime's planners this process asked to `/exit`, and when.
    planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
    /// The binary a handoff asked this process to exec (ADR-0045 decision
    /// 10): no new work starts, and the loop ends once every slot rests at
    /// a point the next process rebuilds it from.
    handoff: Option<String>,
    /// Set when the loop ended for that exec: the registration stays.
    exec: Option<String>,
    /// A program `[run.env]` names did not resolve on this process's PATH
    /// at the last claim pass (ADR-0049 decision 9): nothing is claimed and
    /// no passed run lands until it does.
    run_env_missing: bool,
    /// This pass drains (a stop, a handoff, or claiming stopped after a
    /// provisioning failure): nothing may wait for the program to appear.
    draining: bool,
    /// The automatic update's look at main (ADR-0045 decision 17).
    update: update::UpdateWatch,
    /// The landing recheck running and the one due (ADR-0068).
    rechecks: recheck::Rechecks,
    /// The 1-minute load average, and the host's versions a claim records.
    load_average: fn() -> Option<f64>,
    host_versions: fn(&Path, &Path) -> HostVersions,
    /// The load samples of each held run's current interval (task 197).
    loads: HashMap<RunId, LoadWindow>,
}

/// One executing run between provisioning and rest.
struct Slot {
    run: TaskRun,
    phase: Phase,
    /// The run waits for a person outside the slots, or waits to go back
    /// to one (ADR-0062).
    waiting: Option<Waiting>,
    /// The asks of the run's waits that ended: none starts another.
    consumed: Vec<AskId>,
    /// The asks `run_waiting_deferred` was recorded for.
    deferred: Vec<AskId>,
}

enum Phase {
    Session(SessionWatch),
    /// Receipt validation runs off the loop; the loop only joins the result.
    /// The run's session (if it still has a workspace) stays open through
    /// validation and review (ADR-0027 decision 1).
    Validating(
        Option<thread::JoinHandle<Result<Validation>>>,
        Option<SessionRef>,
    ),
    /// The headless review of an accepted run (ADR-0023 decision 2).
    Review(ReviewWatch),
    /// The live session fixes what a `revise` verdict named (ADR-0027
    /// decision 2).
    Revise(ReviseWatch),
    /// The session is asked to `/exit` and its workspace closed before the
    /// run moves on (a landing, an ask, a failed review, or rest).
    Exiting(ExitWatch),
    /// A resumed session of a `needs_session` run (ADR-0019).
    Resume(ResumeWatch),
    /// A run to land (a passed review, or an approved resolved resume)
    /// waits for the single integration slot, keeping its lease.
    AwaitingSlot,
    /// The run lands off the loop, like validation; the landing releases
    /// the lease itself.
    Landing(Option<thread::JoinHandle<Result<IntegrationOutcome>>>),
    /// The headless triage of a `failed` or `interrupted` run (ADR-0024
    /// decision 3), under a lease of its own.
    Triage(TriageWatch),
}

/// The session of a run the supervisor keeps open through validation,
/// review and revise (ADR-0027): the worker's own workspace, or the one of
/// the resume that reopened the session (ADR-0019).
#[derive(Debug, Clone)]
struct SessionRef {
    workspace: String,
    /// The resume attempt that opened the workspace; `None` for the
    /// worker's workspace (`task_runs.workspace_id`).
    resume: Option<usize>,
}

/// What the supervisor does once the session exited and its workspace
/// closed.
enum AfterExit {
    /// Wait for the integration slot and land (a passed review, or an
    /// approved resolved resume).
    Land,
    /// Open the `approve_landing` ask (a `concern`, or a `revise` past its
    /// limit) and give the lease back.
    Ask {
        decision: ReviewDecision,
        reasons: Vec<String>,
        summary: String,
        /// Why a `revise` verdict became a question for a person.
        why: Option<String>,
    },
    /// Record `review_failed` and give the lease back.
    ReviewFailed {
        attempt: usize,
        error: String,
        duration_secs: u64,
    },
    /// Give the lease back: a run parked for evidence (its workspace is
    /// closed) or a failed one (its workspace is kept for inspection).
    Rest { close: bool },
}

enum Step {
    Continue,
    Done(Box<TaskRun>),
    /// The triage of a run ended (its verdict acted on, or it failed).
    Triaged(Box<TaskRun>),
    /// The lease now carries another token (an adopter took the run, or
    /// `recover` released it): this process must not touch the run again.
    Disowned,
}
impl Supervisor<'_> {
    /// Drive the loop, then remove this process's registration: it is about
    /// to exit, whether it drained its runs, ran out of work, or failed on
    /// a claim or provisioning. Only a heartbeat failure keeps the row (the
    /// database may be unreachable), and it goes stale with the leases.
    fn run_loop(&mut self, options: &LoopSettings) -> Result<Value> {
        let result = self.drive(options);
        if self.exec.is_none() && self.heartbeat.check().is_ok() {
            // The mark of the stop (ADR-0051 decision 10); an exec leaves it
            // to the next process's handoff mark.
            let stopped = json!({
                "supervisor": self.token,
                "dagq_version": self.layout.version,
                "outcome": if result.is_ok() { "stopped" } else { "failed" },
            });
            if let Err(error) = self.queue.record_queue_event(SUPERVISOR_STOPPED, stopped) {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop could not be recorded: {error:#}");
            }
            if let Err(error) = self.queue.deregister_supervisor(&self.token) {
                warn!(error = %format_args!("{error:#}"), "supervisor registration could not be removed: {error:#}");
            }
        }
        result
    }
    fn drive(&mut self, options: &LoopSettings) -> Result<Value> {
        loop {
            if let Err(error) = self.heartbeat.check() {
                // Supervisor-level failure: note it on every run and keep the
                // leases and the registration; they go stale once this
                // process is gone.
                for slot in &self.slots {
                    let _ = self.queue.record_runtime_error(
                        slot.run.id(),
                        &format!("{error:#}"),
                        &ReasonCode::LeaseLost.into(),
                    );
                }
                return Err(error);
            }
            let stopping = options.stop.load(Ordering::SeqCst);
            // Every pass, draining or not, so a hold on landings ends as soon
            // as the program is found (ADR-0049 decision 9).
            self.check_run_env_programs()?;
            self.mark_run_env_change()?;
            self.draining = stopping || !self.claiming || self.handoff.is_some();
            // Before any new work, draining or not: a drain waits for them
            // (ADR-0062 decision 8).
            self.return_waiting_runs();
            // A stop wins over a handoff: the drain goes on as before.
            if !stopping {
                if self.handoff.is_none() {
                    self.handoff = self.queue.handoff_request(&self.token)?;
                    if let Some(binary) = &self.handoff {
                        info!(
                            "supervisor {} asked to hand off to {binary}: no new work starts; it execs once the validations and landings in progress are done",
                            self.token
                        );
                    }
                }
                if let Some(binary) = self.handoff.clone() {
                    // A landing recheck in progress is waited for: its
                    // command would go on in the scratch worktree the next
                    // process uses (ADR-0068). None starts meanwhile (this
                    // pass drains).
                    self.recheck_pass();
                    if !self.rechecks.running()
                        && self.slots.iter().all(|slot| slot.phase.rebuildable())
                    {
                        let runs = self.prepare_handoff();
                        info!(
                            "supervisor {} execs {binary}, handing over {runs} run(s)",
                            self.token
                        );
                        self.exec = Some(binary.clone());
                        return Ok(json!({
                            "outcome": "handoff",
                            "binary": binary,
                            "token": self.token,
                            "runs": self.finished,
                            "handed_over": runs,
                            "errors": self.errors,
                            "triaged": self.triaged,
                        }));
                    }
                    self.draining = true;
                    self.poll_observer();
                    self.tick(true);
                    thread::sleep(options.tick);
                    continue;
                }
            }
            if self.claiming && !stopping {
                self.fill_slots(options.parallel, options.sweep_interval)?;
            }
            self.poll_observer();
            self.record_session_turns(false);
            let rechecked = self.recheck_pass();
            // A supervisor that stopped claiming is draining, not observing
            // nor starting plan reviews, nor updating itself.
            if !stopping && self.claiming {
                self.start_observer_when_due(options);
                self.auto_update_pass(options);
            }
            // A plan review that just readied tasks is followed by one more
            // pass, which claims them.
            let progressed = self.plan_review_pass(options, !stopping && self.claiming);
            if self.slots.is_empty() {
                // A running observer, plan review or landing recheck is
                // waited for like a run: it is bounded by its own timeout
                // or its command. A recheck just applied is followed by one
                // more pass, which resumes the runs it parked.
                let job = self.observer.is_some()
                    || self.plan_review.is_some()
                    || self.rechecks.running()
                    || rechecked;
                if !job && !progressed && (options.once || stopping || !self.claiming) {
                    break;
                }
                thread::sleep(if job { options.tick } else { options.idle_poll });
                continue;
            }
            self.tick(false);
            thread::sleep(options.tick);
        }
        if let Some(message) = &self.provisioning_error {
            bail!(
                "{message}; claiming stopped and {} active run(s) were drained; inspect doctor before recovery",
                self.finished.len()
            );
        }
        let outcome = if options.stop.load(Ordering::SeqCst) {
            "stopped"
        } else {
            "finished"
        };
        Ok(json!({
            "outcome": outcome,
            "runs": self.finished,
            "errors": self.errors,
            "triaged": self.triaged,
        }))
    }
    /// Adopt the runs other supervisors left behind, then claim and
    /// provision candidates until every slot is taken or nothing is
    /// claimable. `main` is reread per claim so a task released by
    /// `integrate` starts from the main that contains its predecessor.
    fn fill_slots(&mut self, parallel: usize, sweep_interval: Duration) -> Result<()> {
        // A run that waits for a person is adopted without a free slot
        // (ADR-0062 decision 11): the check is per run.
        self.adopt_stale_runs(parallel)?;
        // Takes no slot: a dead run goes to the triage below.
        self.recover_dead_runs()?;
        if self.used_slots() < parallel {
            self.apply_landing_answers(parallel)?;
        }
        self.apply_triage_answers()?;
        if self.used_slots() < parallel {
            self.resume_parked_runs(parallel)?;
        }
        if self.used_slots() < parallel {
            self.triage_runs(parallel)?;
        }
        // Takes no slot: only closes and frees what ended runs left.
        if let Err(error) = self.sweep_ended_runs(sweep_interval) {
            warn!(error = %format_args!("{error:#}"), "the workspaces and worktrees of ended runs could not all be swept: {error:#}");
        }
        // A run claimed now would fail every cargo command (ADR-0049
        // decision 9; checked at the top of the pass); the runs in flight,
        // their reviews and resumes go on.
        if self.run_env_missing {
            return Ok(());
        }
        let mut host: Option<HostVersions> = None;
        while self.used_slots() < parallel {
            // Highest effective priority, then most-releasing, then lowest
            // ID (ADR-0040 decision 4); `candidates` and `graph` show the
            // same order, so it is not recorded.
            let order = dependency_graph(self.queue.graph_input()?, None).candidates;
            if order.is_empty() {
                break;
            }
            let base = self.repository.main_head()?;
            // Read once per pass: `rustc -vV` takes a moment on a loaded host.
            let host = host.get_or_insert_with(|| {
                (self.host_versions)(&self.layout.claude, &self.layout.repo_root)
            });
            let attributes = self.claim_attributes(parallel, host.clone());
            let run = match self.queue.claim_for_supervisor_in_order(
                &base,
                &self.token,
                &order,
                Some(&serde_json::to_value(&attributes)?),
            )? {
                ClaimOutcome::Claimed { run } => *run,
                ClaimOutcome::NoReadyTask => break,
            };
            // The work interval starts at the claim, with its sample.
            let mut window = LoadWindow::default();
            window.add(attributes.load_avg);
            self.loads.insert(run.id().clone(), window);
            match self.provision(&run) {
                Ok(watch) => {
                    let run = self.queue.run(run.id())?;
                    self.slots.push(Slot::new(run, Phase::Session(watch)));
                }
                Err(error) => {
                    let message = format!("run {} provisioning failed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{message}; no further tasks will be claimed");
                    self.abandon(
                        &run,
                        message.clone(),
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                    self.claiming = false;
                    self.provisioning_error = Some(message);
                    break;
                }
            }
        }
        Ok(())
    }
    /// Check the programs `[run.env]` names on this process's PATH
    /// (ADR-0049 decision 9) and record `run_env_program_missing` or
    /// `run_env_program_found` when the answer differs from the latest one
    /// on the queue, so a restarted supervisor does not repeat it. A
    /// `dagq.toml` that cannot be read leaves the last answer: provisioning
    /// and `integrate` report that file themselves.
    fn check_run_env_programs(&mut self) -> Result<()> {
        let check = match self.verifier.run_env_programs(None) {
            Ok(check) => check,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the programs of [run.env] could not be checked: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?;
        if let Some((kind, mut payload)) = check.transition(last.as_ref().map(|e| e.kind.as_str()))
        {
            payload["supervisor"] = json!(self.token);
            self.queue.record_queue_event(kind, payload)?;
            match check.missing_message() {
                Some(message) => {
                    warn!("{message}; no task is claimed and no run lands until it is found")
                }
                None => {
                    info!("the programs of [run.env] are found again; claiming and landing resume")
                }
            }
        }
        self.run_env_missing = !check.missing().is_empty();
        Ok(())
    }
    /// Record `run_env_changed` when the normalized `[run.env]` of the main
    /// checkout hashes differently from the latest one on the queue
    /// (ADR-0051 decision 11), so a restarted supervisor does not repeat
    /// it. A `dagq.toml` that cannot be read records nothing: provisioning
    /// and `integrate` report that file themselves. Neither does a missing
    /// one, which may only be a checkout rewriting it; an empty `[run.env]`
    /// is the change that removes the table.
    fn mark_run_env_change(&mut self) -> Result<()> {
        let table = match self.verifier.run_env_table() {
            Ok(Some(table)) => table,
            Ok(None) => return Ok(()),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "[run.env] could not be read for its change mark: {error:#}");
                return Ok(());
            }
        };
        let salt = match self.verifier.run_env_salt() {
            Ok(salt) => salt,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the salt of the [run.env] hashes could not be read: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&[RUN_ENV_CHANGED])?;
        if let Some(mut payload) =
            run_env_digest(&table, &salt).transition(last.as_ref().map(|event| &event.payload))
        {
            info!(
                "[run.env] changed ({}): recorded as a change mark",
                payload["changed"]
            );
            payload["supervisor"] = json!(self.token);
            self.queue.record_queue_event(RUN_ENV_CHANGED, payload)?;
        }
        Ok(())
    }
    /// What `run_claimed` records of a claim made now (task 197): this
    /// binary's build identifier, the host's versions, `parallel`, the
    /// slots held before the claim (runs waiting for a person hold none,
    /// ADR-0062) and the load average.
    fn claim_attributes(&self, parallel: usize, host: HostVersions) -> ClaimAttributes {
        ClaimAttributes {
            dagq_version: self.layout.version.clone(),
            host,
            parallel,
            slots: self.used_slots(),
            load_avg: (self.load_average)(),
        }
    }
    /// Sample the load average once for every slot's current interval
    /// (task 197); the windows of runs no slot holds any more are dropped.
    fn sample_load(&mut self) {
        let load = (self.load_average)();
        let held: HashSet<&RunId> = self.slots.iter().map(|slot| slot.run.id()).collect();
        self.loads.retain(|id, _| held.contains(id));
        for id in held {
            self.loads.entry(id.clone()).or_default().add(load);
        }
    }
    /// The load over the run's interval that ends now, and start the next.
    pub(super) fn take_load(&mut self, id: &RunId) -> LoadSummary {
        self.loads.entry(id.clone()).or_default().take()
    }
    /// One pass over the slots; with `unsettled_only`, over the slots a
    /// handoff waits for (their validation or landing in progress) only.
    fn tick(&mut self, unsettled_only: bool) {
        self.sample_load();
        if !unsettled_only {
            self.start_waits();
        }
        let mut index = 0;
        while index < self.slots.len() {
            if unsettled_only && self.slots[index].phase.rebuildable() {
                index += 1;
                continue;
            }
            let mut slot = self.slots.remove(index);
            let stepped = if slot.out_of_slot() {
                self.watch_waiting(&mut slot)
            } else {
                self.step(&mut slot)
            };
            match stepped {
                Ok(Step::Continue) => {
                    self.slots.insert(index, slot);
                    index += 1;
                }
                Ok(Step::Done(run)) => {
                    info!(run_id = %run.id(), "run {} is {}", run.id(), run.status().as_str());
                    // Main moved: the runs that wait to land are checked
                    // against it (ADR-0068 decision 1).
                    if run.status() == RunStatus::Integrated {
                        self.note_landed(&run);
                    }
                    // A landed run needs none of its workspaces; a failed
                    // one keeps them for its triage.
                    if matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded)
                        && let Err(error) =
                            self.close_open_workspaces(&run, WorkspaceCloser::Supervisor)
                    {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
                    }
                    self.clean_task_worktrees(run.task_id());
                    self.finished.push(*run);
                }
                Ok(Step::Triaged(run)) => self.note_triaged(&run),
                Ok(Step::Disowned) => {
                    stop_job(&mut slot);
                    self.disown(&slot)
                }
                // A lease-guarded write that failed because the lease
                // changed hands mid-step (this process was stalled and
                // adopted from) is the other owner's run to describe.
                Err(_)
                    if !self
                        .queue
                        .holds_lease(slot.run.id(), &self.token)
                        .unwrap_or(true) =>
                {
                    self.disown(&slot)
                }
                Err(error) if matches!(slot.phase, Phase::Triage(_)) => {
                    stop_job(&mut slot);
                    let attempt = match &slot.phase {
                        Phase::Triage(watch) => watch.attempt,
                        _ => unreachable!("matched a triage"),
                    };
                    self.fail_triage(&slot.run, attempt, format!("{error:#}"), 0);
                    let run = self.queue.run(slot.run.id()).unwrap_or(slot.run);
                    self.note_triaged(&run);
                }
                Err(error) if matches!(slot.phase, Phase::AwaitingSlot) => {
                    // The resume already recorded its `resume_finished`;
                    // only the lease it kept for the landing goes.
                    let message = format!("landing could not start: {error:#}");
                    warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                    if let Err(error) = self.queue.release_lease(slot.run.id(), &self.token) {
                        warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", slot.run.id());
                    }
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
                Err(error) if matches!(slot.phase, Phase::Resume(_)) => {
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} resume stopped: {message}; its workspace is kept for inspection", slot.run.id());
                    let (attempt, workspace) = match &slot.phase {
                        Phase::Resume(watch) => (watch.attempt, Some(watch.workspace.clone())),
                        _ => unreachable!("matched a resume"),
                    };
                    self.give_up_resume(
                        &slot.run,
                        attempt,
                        workspace.as_deref(),
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                }
                Err(error) => {
                    // Creation/communication failures can be ambiguous: the
                    // session may be alive. Disown the run, delete nothing,
                    // and keep serving the other slots. A headless review in
                    // progress is stopped: nobody would read its verdict.
                    stop_job(&mut slot);
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} retained for inspection: {message}; see show {} and doctor", slot.run.id(), slot.run.task_id());
                    self.abandon(
                        &slot.run,
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                }
            }
        }
    }
    /// The observation due now, if any: the daily one when it has not run
    /// for 24 hours, else the hourly one when the interval passed since the
    /// last one started or finished (from the queue, whichever supervisor
    /// ran it) and since this process last launched it.
    fn due_observation(&self, options: &LoopSettings) -> Result<Option<ObserveMode>> {
        if options.observe_interval.is_zero() {
            return Ok(None);
        }
        let now = self.generators.clock.now();
        let mut modes = vec![(
            ObserveMode::Hourly,
            i64::try_from(options.observe_interval.as_secs())?,
        )];
        if options.observe_daily {
            modes.insert(0, (ObserveMode::Daily, DAILY_WINDOW_SECS));
        }
        for (mode, every) in modes {
            let recorded = self
                .queue
                .last_observe(mode.as_str())?
                .is_some_and(|last| now - last < every);
            let launched = self.observers_launched.iter().any(|(launched, at)| {
                *launched == mode && at.elapsed().as_secs() < every.unsigned_abs()
            });
            if !recorded && !launched {
                return Ok(Some(mode));
            }
        }
        Ok(None)
    }
    /// Launch `dagq observe` as a child process when an observation is due
    /// and none is running. It takes no run slot. A failure to launch is
    /// logged and retried after the interval.
    fn start_observer_when_due(&mut self, options: &LoopSettings) {
        if self.observer.is_some() {
            return;
        }
        let mode = match self.due_observation(options) {
            Ok(Some(mode)) => mode,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        // The observer reads the active time of the spans still open too.
        self.record_session_turns(true);
        self.observers_launched
            .retain(|(launched, _)| *launched != mode);
        self.observers_launched.push((mode, Instant::now()));
        let mut command = CommandSpec::new(&self.layout.runner);
        command
            .arg("--db")
            .arg(&self.layout.db)
            .arg("observe")
            .arg("--claude")
            .arg(&self.layout.claude)
            .current_dir(&self.layout.repo_root);
        for name in &self.layout.observer_env_remove {
            command.env_remove(name);
        }
        if mode == ObserveMode::Daily {
            command.arg("--daily");
        }
        match self.spawner.spawn(&command, Streams::Null) {
            Ok(child) => {
                info!("observer ({}) started: pid {}", mode.as_str(), child.id());
                self.observer = Some((mode, child));
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not start: {error:#}", mode.as_str())
            }
        }
    }
    /// Record the finished transcript turns of the open session spans when
    /// [`SESSION_TURNS_INTERVAL`] passed since the last time (at once with
    /// `now`). A failure is logged only: it changes no run (ADR-0048
    /// decision 10), and the next time reads the transcripts again.
    fn record_session_turns(&mut self, now: bool) {
        if !now
            && self
                .last_turns
                .is_some_and(|last| last.elapsed() < SESSION_TURNS_INTERVAL)
        {
            return;
        }
        self.last_turns = Some(Instant::now());
        match self.queue.record_session_turns() {
            Ok(0) => {}
            Ok(spans) => info!("recorded the transcript turns of {spans} open session span(s)"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "transcript turns could not be recorded: {error:#}")
            }
        }
    }
    /// Reap the observer once it exited; its own `observe_finished` is the
    /// record.
    fn poll_observer(&mut self) {
        let Some((mode, child)) = self.observer.as_mut() else {
            return;
        };
        let mode = *mode;
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                info!("observer ({}) exited: {status}", mode.as_str());
                self.observer = None;
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not be waited for: {error:#}", mode.as_str());
                self.observer = None;
            }
        }
    }
    /// Drop a slot whose lease another process holds now, writing nothing
    /// about the run: the new owner's record is the record.
    fn disown(&mut self, slot: &Slot) {
        let mut message = format!(
            "lease of run {} is held by another process; this supervisor stopped watching it",
            slot.run.id()
        );
        if matches!(slot.phase, Phase::Validating(Some(_), _)) {
            // Its checks finish on their own; the new owner runs its own.
            message.push_str("; a validation already in progress runs to completion unrecorded");
        }
        warn!(run_id = %slot.run.id(), "{}", message);
        self.errors.push(RunError {
            run_id: slot.run.id().clone(),
            task_id: slot.run.task_id(),
            message,
        });
    }
    fn abandon(&mut self, run: &TaskRun, message: String, reason: &Reason) {
        if let Err(error) = self
            .queue
            .abandon_run(run.id(), &self.token, &message, reason)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the error: {error:#}", run.id());
        }
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    /// A resume that failed in itself (not the session's verdict): record
    /// `resume_finished` with outcome `error` and give the lease back; the
    /// run stays `needs_session` with its reason, and the attempt counts.
    /// The resume workspace is closed only when no session of this resume
    /// can be alive: its wrapper never registered (and, the lease gone, no
    /// longer can) or already exited. `workspace` is `None` when opening it
    /// failed before cmux returned its ID; workspaces are never looked up
    /// by title (ADR-0026).
    fn give_up_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        workspace: Option<&str>,
        message: String,
        reason: &Reason,
    ) {
        let workspace = workspace.map(str::to_owned);
        // The workspace is recorded so a later pass knows it for this run's
        // own once its session has ended (`close_left_resume_workspaces`).
        let payload = reason.on(json!({
            "attempt": attempt,
            "outcome": "error",
            "error": message,
            "workspace_id": workspace,
            "exhausted": resumes_exhausted(&*self.queue, run.id()),
        }));
        if let Err(error) =
            self.queue
                .finish_resume(run.id(), &self.token, None, None, false, payload)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the resume error: {error:#}", run.id());
        }
        let session_may_live = self.queue.processes(run.id()).map_or(true, |processes| {
            processes
                .iter()
                .any(|p| p.role == "wrapper" && p.exited_at.is_none())
        });
        if let Some(workspace) = workspace {
            if session_may_live {
                info!(run_id = %run.id(), "run {}: resume workspace {workspace} is kept; its session may still run", run.id());
            } else if let Err(error) = self.cmux.close(&workspace) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: resume workspace {workspace} could not be closed: {error:#}", run.id());
            }
        }
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    fn step(&mut self, slot: &mut Slot) -> Result<Step> {
        // A landing releases the lease itself when it ends, so it is joined
        // before the lease is checked.
        if let Phase::Landing(handle) = &mut slot.phase {
            if !handle.as_ref().is_some_and(|h| h.is_finished()) {
                return Ok(Step::Continue);
            }
            let landed = handle
                .take()
                .context("landing already joined")?
                .join()
                .map_err(|_| anyhow!("landing thread panicked"));
            match landed.and_then(|result| result) {
                Ok(outcome) => {
                    let outcome = serde_json::to_value(&outcome)?;
                    info!(run_id = %slot.run.id(), "run {} landing: {}", slot.run.id(), outcome["outcome"]);
                }
                Err(error) => {
                    let message = format!("landing failed: {error:#}");
                    warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
            }
            return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
        }
        if !self.queue.holds_lease(slot.run.id(), &self.token)? {
            return Ok(Step::Disowned);
        }
        match &mut slot.phase {
            Phase::Resume(watch) => {
                let Some(verdict) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let workspace = watch.workspace.clone();
                self.finish_resumed_session(slot, attempt, &workspace, verdict)
            }
            Phase::AwaitingSlot => {
                // Its verification would fail on the missing program: the
                // run stays awaiting integration, leased, and the
                // integration slot stays free (ADR-0049 decision 9). A
                // supervisor that drains or hands off cannot wait for it:
                // it gives the lease back and leaves the run awaiting
                // integration for a person (`review and integrate`).
                if self.run_env_missing {
                    if !self.draining {
                        return Ok(Step::Continue);
                    }
                    warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: a program [run.env] names is missing and this supervisor stops", slot.run.id());
                    self.queue.release_lease(slot.run.id(), &self.token)?;
                    return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
                }
                if !self
                    .queue
                    .runs_with_status(RunStatus::Integrating)?
                    .is_empty()
                {
                    return Ok(Step::Continue);
                }
                let current = self.queue.run(slot.run.id())?;
                let previous = current.status();
                let main = self.repository.main_head()?;
                // A head the landing recheck found not landing on this main
                // is parked for a resume instead (ADR-0068 decision 3).
                if let Some(parked) = self.park_held_by_recheck(&current, &main)? {
                    return Ok(Step::Done(Box::new(parked)));
                }
                let run = match self
                    .queue
                    .begin_integration(slot.run.id(), &self.token, &main)
                {
                    Ok(run) => run,
                    // An `integrate` took the slot since the check: try again later.
                    Err(_)
                        if !self
                            .queue
                            .runs_with_status(RunStatus::Integrating)?
                            .is_empty() =>
                    {
                        return Ok(Step::Continue);
                    }
                    Err(error) => return Err(error),
                };
                info!(run_id = %run.id(), "run {} lands onto main {main} ({})", run.id(), previous.as_str());
                slot.phase =
                    Phase::Landing(Some(self.spawn_landing(run.clone(), previous, main)?));
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::Landing(_) => unreachable!("joined above"),
            Phase::Triage(watch) => {
                let Some(outcome) = watch.poll(&*self.files)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let duration_secs = watch.job.started.elapsed().as_secs();
                let run = self.queue.run(slot.run.id())?;
                let acted = outcome
                    .map_err(|error| anyhow!(error))
                    .and_then(|verdict| self.act_on_triage(&run, attempt, duration_secs, verdict));
                if let Err(error) = acted {
                    // Another process took the run's lease meanwhile: its
                    // triage is the record.
                    if !self.queue.holds_lease(run.id(), &self.token)? {
                        return Ok(Step::Disowned);
                    }
                    self.fail_triage(&run, attempt, format!("{error:#}"), duration_secs);
                }
                Ok(Step::Triaged(Box::new(self.queue.run(run.id())?)))
            }
            Phase::Session(watch) => {
                let Some(run) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                if run.status() != RunStatus::Validating {
                    self.queue.release_lease(run.id(), &self.token)?;
                    return Ok(Step::Done(Box::new(run)));
                }
                let session = SessionRef {
                    workspace: watch.workspace.clone(),
                    resume: None,
                };
                let handle = self.validate(run.clone());
                slot.run = run;
                slot.phase = Phase::Validating(Some(handle), Some(session));
                Ok(Step::Continue)
            }
            Phase::Validating(handle, session) => {
                if !handle.as_ref().is_some_and(|h| h.is_finished()) {
                    return Ok(Step::Continue);
                }
                let mut validation = handle
                    .take()
                    .context("validation already joined")?
                    .join()
                    .map_err(|_| anyhow!("validation thread panicked"))??;
                validation.load = self.take_load(slot.run.id());
                let run = self
                    .queue
                    .finish_validation(slot.run.id(), &self.token, &validation)?;
                let session = session.take();
                slot.phase = match run.status() {
                    // An approved run (its integrate was called) lands without
                    // a review, as before (ADR-0027 decision 3).
                    RunStatus::AwaitingIntegration
                        if self.queue.has_run_event(run.id(), "integration_approved")? =>
                    {
                        Phase::Exiting(ExitWatch::new(session, AfterExit::Land))
                    }
                    // A landing recheck resumed the run without waiting for
                    // the answer to its approve_landing ask (ADR-0068
                    // decision 4): the rebased run waits for it instead of
                    // a new review.
                    RunStatus::AwaitingIntegration
                        if crate::domain::resume::parked_by_recheck(
                            &self.queue.run_events(run.id())?,
                        ) && self
                            .queue
                            .has_unclosed_ask(run.id(), AskKind::ApproveLanding)? =>
                    {
                        Phase::Exiting(ExitWatch::new(session, AfterExit::Rest { close: true }))
                    }
                    RunStatus::AwaitingIntegration => self.start_review(&run, session)?,
                    // A run parked for evidence gives up its workspace, since a
                    // resume opens one of its own; a failed one keeps it for
                    // inspection.
                    RunStatus::NeedsSession => {
                        Phase::Exiting(ExitWatch::new(session, AfterExit::Rest { close: true }))
                    }
                    _ => Phase::Exiting(ExitWatch::new(session, AfterExit::Rest { close: false })),
                };
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::Review(watch) => {
                let Some(outcome) = watch.poll(&*self.files)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let duration_secs = watch.job.started.elapsed().as_secs();
                let session = watch.session.take();
                let run = self.queue.run(slot.run.id())?;
                slot.phase = match outcome {
                    ReviewEnd::Verdict(verdict) => {
                        self.queue.record_runtime_event(
                            run.id(),
                            "review_finished",
                            json!({
                                "verdict": verdict.verdict,
                                "reasons": verdict.reasons,
                                "summary": verdict.summary,
                                "duration_secs": duration_secs,
                                "attempt": attempt,
                            }),
                        )?;
                        info!(run_id = %run.id(), "run {} review {attempt}: {} ({})", run.id(), verdict.verdict.as_str(), verdict.summary);
                        self.act_on_verdict(&run, session, verdict)?
                    }
                    ReviewEnd::Unreadable(error) if !watch.retried => {
                        self.retry_review(&run, session, attempt, &error)?
                    }
                    ReviewEnd::Unreadable(error) | ReviewEnd::Failed(error) => {
                        warn!(run_id = %run.id(), error = %error, "run {} review {attempt} failed: {error}; a person is asked", run.id());
                        Phase::Exiting(ExitWatch::new(
                            session,
                            AfterExit::ReviewFailed {
                                attempt,
                                error,
                                duration_secs,
                            },
                        ))
                    }
                };
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::Revise(watch) => {
                let Some(outcome) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                let session = watch.session.clone();
                let label = watch.fix.label(watch.attempt);
                match outcome {
                    ReviseOutcome::Rewritten(head) => {
                        let kind = match watch.fix {
                            Fix::Revise(_) => "revise_finished",
                            Fix::Conflict(_) => "conflict_resolved",
                        };
                        self.queue.record_runtime_event(
                            slot.run.id(),
                            kind,
                            json!({"attempt": watch.attempt, "head": head}),
                        )?;
                        info!(run_id = %slot.run.id(), "run {} rewrote its receipt for {label} (head {head}); validating again", slot.run.id());
                        let run = self.queue.restart_validation(slot.run.id(), &self.token)?;
                        let handle = self.validate(run.clone());
                        slot.run = run;
                        slot.phase = Phase::Validating(Some(handle), Some(session));
                    }
                    ReviseOutcome::Mismatch(code, why) => {
                        let message = revise_mismatch_request(&slot.run, &label, &why)?;
                        // Only what the session writes after this counts.
                        let sent_at = self.files.now();
                        let run = slot.run.clone();
                        match submit(
                            self,
                            &run,
                            &session.workspace,
                            Input::Text(&message),
                            "receipt fix request",
                        ) {
                            Ok(submission) => {
                                watch.sent_at = sent_at;
                                watch.start = Some(StartCheck::new(
                                    "receipt fix request",
                                    &message,
                                    sent_at,
                                    &submission,
                                ));
                                let kind = match watch.fix {
                                    Fix::Revise(_) => "revise_receipt_rejected",
                                    Fix::Conflict(_) => "conflict_receipt_rejected",
                                };
                                self.queue.record_runtime_event(
                                    slot.run.id(),
                                    kind,
                                    json!({"code": code, "attempt": watch.attempt, "reason": why}),
                                )?;
                                info!(run_id = %slot.run.id(), "run {}: {why}; asked the session to fix it ({label})", slot.run.id());
                            }
                            Err(error) => {
                                let why = format!(
                                    "{why}, and the request to fix it could not be sent: {error:#}"
                                );
                                warn!(run_id = %slot.run.id(), "run {}: {why}", slot.run.id());
                                let then = watch.fix.ask(why.clone(), why);
                                slot.phase = Phase::Exiting(ExitWatch::new(Some(session), then));
                            }
                        }
                    }
                    ReviseOutcome::Ended(why) => {
                        info!(run_id = %slot.run.id(), "run {}: the session {why} after {label}; asking a person", slot.run.id());
                        let then = watch.fix.ask(
                            format!("the session {why}"),
                            format!("the session {why} after {label}"),
                        );
                        slot.phase = Phase::Exiting(ExitWatch::new(Some(session), then));
                    }
                }
                Ok(Step::Continue)
            }
            Phase::Exiting(watch) => {
                if !watch.poll(self, &slot.run)? {
                    return Ok(Step::Continue);
                }
                let session = watch.session.take();
                let then = std::mem::replace(&mut watch.then, AfterExit::Rest { close: false });
                let mut run = self.queue.run(slot.run.id())?;
                let close = !matches!(then, AfterExit::Rest { close: false });
                if close && let Some(session) = &session {
                    run = self.close_session(&run, session)?;
                }
                match then {
                    AfterExit::Land => {
                        self.queue_landing(&run, "exit");
                        slot.run = run;
                        slot.phase = Phase::AwaitingSlot;
                        Ok(Step::Continue)
                    }
                    AfterExit::Ask {
                        decision,
                        reasons,
                        summary,
                        why,
                    } => {
                        let ask = self.open_landing_ask(
                            &run,
                            decision,
                            &reasons,
                            &summary,
                            why.as_deref(),
                        )?;
                        info!(run_id = %run.id(), "run {} waits for a person in ask {ask}", run.id());
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs,
                    } => {
                        // The ask goes with the failure (task 328): the
                        // person answers it rather than finding the run in
                        // the attention. When it cannot be opened, the
                        // failure is the attention (review by hand).
                        let ask = match self.open_failed_review_ask(&run, attempt, &error) {
                            Ok(ask) => {
                                info!(run_id = %run.id(), "run {} waits for a person in ask {ask} after its failed review", run.id());
                                Some(ask)
                            }
                            Err(ask_error) => {
                                warn!(run_id = %run.id(), error = %format_args!("{ask_error:#}"), "run {}: the approve_landing ask of its failed review could not be opened: {ask_error:#}; it waits for a review by hand", run.id());
                                None
                            }
                        };
                        let mut payload = json!({
                            "code": ReasonCode::JobFailed,
                            "attempt": attempt,
                            "error": error,
                            "duration_secs": duration_secs,
                            "status": run.status().as_str(),
                        });
                        if let Some(ask) = ask {
                            payload["ask_id"] = json!(ask);
                        }
                        self.queue
                            .record_runtime_event(run.id(), "review_failed", payload)?;
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::Rest { .. } => {
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(run)))
                    }
                }
            }
        }
    }
}

/// Kill the headless job (a review or a triage) of a slot the supervisor
/// stops watching.
fn stop_job(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Review(watch) => watch.job.stop(),
        Phase::Triage(watch) => watch.job.stop(),
        Phase::Session(watch) => watch.recovery.stop_job(),
        _ => {}
    }
}

/// Whether the run's resumes are used up (ADR-0047 decision 24: its
/// counted resumes or its conflict-only ones); unreadable counts as used up.
fn resumes_exhausted(queue: &dyn Queue, run_id: &RunId) -> bool {
    queue
        .run_events(run_id)
        .map_or(true, |events| ResumeCount::of(&events).exhausted())
}

/// Whether the run's session wrapper is registered, has not exited and has
/// not died without recording its exit (its heartbeat expired and its
/// process is gone): a session that died is one that ended (task 236).
fn session_alive(sv: &Supervisor<'_>, run_id: &RunId) -> Result<bool> {
    let now = sv.generators.clock.now();
    Ok(sv
        .queue
        .processes(run_id)?
        .iter()
        .any(|p| p.role == "wrapper" && p.exited_at.is_none() && !wrapper_dead(sv, p, now)))
}

/// Whether a wrapper that has not recorded its exit died: its heartbeat is
/// older than `HEARTBEAT_TIMEOUT_SECS` and its process is gone.
fn wrapper_dead(sv: &Supervisor<'_>, wrapper: &RunProcess, now: i64) -> bool {
    now - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS && !sv.processes.alive(wrapper.pid)
}

/// The `reasons` of the run's latest review: those of a `review_finished`,
/// or the error of a `review_failed` after it.
fn latest_review_reasons(queue: &dyn Queue, run_id: &RunId) -> Result<Vec<String>> {
    Ok(queue
        .run_events(run_id)?
        .into_iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "review_finished" | "review_failed"))
        .and_then(|e| match e.kind.as_str() {
            "review_failed" => e.payload["error"]
                .as_str()
                .map(|error| vec![format!("the headless review failed: {error}")]),
            _ => serde_json::from_value(e.payload["reasons"].clone()).ok(),
        })
        .unwrap_or_default())
}

/// Cross-check the agent's receipt against Git on a thread with its own
/// connection. The task's verification commands do not run here: `integrate`
/// runs them once, after its rebase (ADR-0023 decision 1). Rejections become a
/// `Validation` that is not accepted; only errors in the checks themselves
/// propagate, leaving the run in `validating`.
fn spawn_validation(
    queues: Arc<dyn QueueOpener>,
    repository: Arc<dyn Repository + Send + Sync>,
    files: Arc<dyn RunFiles>,
    run: TaskRun,
) -> thread::JoinHandle<Result<Validation>> {
    spawn_traced(move || {
        let mut queue = queues.open()?;
        let task = queue.show(run.task_id())?.task;
        let checked = check_receipt(&*repository, &*files, &task, &run)?;
        Ok(match checked {
            Ok((receipt, commit)) => Validation {
                accepted: true,
                result_commit: Some(commit),
                reason: None,
                code: None,
                receipt: serde_json::to_value(receipt)?,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                load: LoadSummary::default(),
            },
            Err(rejection) => {
                warn!(run_id = %run.id(), "run {} rejected: {}", run.id(), rejection.reason);
                Validation {
                    accepted: false,
                    result_commit: rejection.commit,
                    reason: Some(rejection.reason),
                    code: Some(rejection.code),
                    receipt: rejection
                        .receipt
                        .map(serde_json::to_value)
                        .transpose()?
                        .unwrap_or(Value::Null),
                    evidence_missing: rejection.evidence_missing,
                    allowed_paths: if rejection.scope_violation.is_empty() {
                        Vec::new()
                    } else {
                        task.paths().to_vec()
                    },
                    scope_violation: rejection.scope_violation,
                    load: LoadSummary::default(),
                }
            }
        })
    })
}

/// Close the cmux workspace of an accepted run. The worktree and branch stay
/// until integration. A close failure is recorded but does not change the run
/// status; `workspace_closed_at` stays null so nothing treats it as cleaned.
fn close_workspace(
    queue: &mut dyn Queue,
    cmux: &dyn WorkspaceBackend,
    token: &str,
    run: &TaskRun,
) -> Result<TaskRun> {
    let workspace = run.workspace_id().context("missing workspace")?;
    match cmux.close(workspace) {
        Ok(()) => queue.workspace_closed(run.id(), token),
        Err(error) => {
            let message = format!("workspace {workspace} could not be closed: {error:#}");
            warn!(run_id = %run.id(), "run {}: {message}", run.id());
            queue.cleanup_failed(
                run.id(),
                token,
                &message,
                &reason_of_error(&error, ReasonCode::BackendFailed),
            )
        }
    }
}
