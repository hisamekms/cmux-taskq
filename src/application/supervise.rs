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
//! ([`AgentProvider`]), the processes it starts ([`Spawner`]) and checks
//! ([`ProcessControl`]), the run files ([`RunFiles`]), its log
//! ([`NoteLog`]) and the time and IDs ([`Generators`]).

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    AgentProvider, CommandSpec, Generators, LeasedRun, MainRemote, NoteLog, ProcessControl, Queue,
    QueueOpener, Repository, ResumeCandidate, RunFiles, Spawned, Spawner, Streams, TRIAGE_ASKER,
    TriageAction, Validation, Verifier, WorkspaceBackend, WorkspaceTags, ask, dependency_graph,
    fenced,
    health::run_health,
    integrate::{
        self as integration, Integration, check_receipt, integrate_logs, log_names, resume_attempts,
    },
    naming::{
        resume_workspace_description, shell_join, workspace_description, workspace_group_name,
    },
    or_none, path_text,
    prompt::{PredecessorSummary, STOP_BACKGROUND, prompt, siblings_in_progress},
    recording::RecordingBackend,
    tail, unix_seconds,
};
use crate::domain::{
    AskKind, ClaimOutcome, CommitSha, EvidenceCheck, HEARTBEAT_TIMEOUT_SECS, IntegrationOutcome,
    LANDING_OPTIONS, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, NewAsk, Predecessor, Receipt,
    ReceiptResult, ReviewDecision, ReviewVerdict, RunId, RunLease, RunPaths, RunPlan, RunProcess,
    RunStatus, SessionRole, TRIAGE_OPTIONS, TRIAGE_RETRY_FAILURES, Task, TaskAction, TaskDetail,
    TaskId, TaskRun, TaskStatus, TriageDecision, TriageState, TriageVerdict, heartbeat_stale,
    triage_state,
};

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
    /// Starts the headless review and triage (ADR-0027, ADR-0024).
    pub reviewer: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: Arc<dyn RunFiles>,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub generators: Generators,
    /// Writes a task's review material (`review`) and reports its path.
    pub review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    /// The log of this start, given the registration's `started_at`.
    pub open_log: &'a dyn Fn(i64) -> Result<Arc<dyn NoteLog>>,
    /// The 1-minute load average recorded with a failed cmux call.
    pub load_average: fn() -> Option<f64>,
    pub layout: Layout,
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
        let worker = thread::spawn(move || {
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
                eprintln!("supervisor heartbeat failed: {error:#}");
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
    let token = ports.generators.ids.uuid();
    // Registered before the first heartbeat so the loop is visible to
    // `status` from its first second, runs or not.
    let parallel =
        u32::try_from(settings.parallel).context("parallel does not fit a registration")?;
    let pid = layout.pid;
    let registration = queue.register_supervisor(&token, pid, parallel, &layout.version)?;
    let log = match (ports.open_log)(registration.started_at) {
        Ok(log) => log,
        Err(error) => {
            // Not a supervisor after all: leave no row for `status`.
            let _ = queue.deregister_supervisor(&token);
            return Err(error);
        }
    };
    log.note(&format!(
        "supervisor {token} started: version {}, pid {pid}, parallel {parallel}, db {}, repository {}",
        layout.version,
        layout.db.display(),
        layout.repo_root.display()
    ));
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
        spawner: ports.spawner,
        files: ports.files.clone(),
        processes: ports.processes.clone(),
        review_material: ports.review_material,
        token,
        heartbeat,
        log: log.clone(),
        slots: Vec::new(),
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
        observer: None,
        observers_launched: Vec::new(),
        triaged: Vec::new(),
        generators: ports.generators.clone(),
    };
    let result = supervisor.run_loop(settings);
    match &result {
        Ok(value) => log.note(&format!("supervisor {} exiting: {value}", supervisor.token)),
        Err(error) => log.note(&format!(
            "supervisor {} failed: {error:#}",
            supervisor.token
        )),
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
    spawner: &'a dyn Spawner,
    files: Arc<dyn RunFiles>,
    processes: Arc<dyn ProcessControl + Send + Sync>,
    review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    token: String,
    heartbeat: Heartbeat,
    log: Arc<dyn NoteLog>,
    slots: Vec<Slot>,
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
    /// The runs this process triaged, with where each one went.
    triaged: Vec<Value>,
    /// The clock and IDs `queue` also uses.
    generators: Generators,
}

/// One executing run between provisioning and rest.
struct Slot {
    run: TaskRun,
    phase: Phase,
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
        if self.heartbeat.check().is_ok()
            && let Err(error) = self.queue.deregister_supervisor(&self.token)
        {
            self.log.note(&format!(
                "supervisor registration could not be removed: {error:#}"
            ));
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
                    let _ = self
                        .queue
                        .record_runtime_error(slot.run.id(), &format!("{error:#}"));
                }
                return Err(error);
            }
            let stopping = options.stop.load(Ordering::SeqCst);
            if self.claiming && !stopping {
                self.fill_slots(options.parallel)?;
            }
            self.poll_observer();
            // A supervisor that stopped claiming is draining, not observing.
            if !stopping && self.claiming {
                self.start_observer_when_due(options);
            }
            if self.slots.is_empty() {
                // A running observer is waited for like a run: it is short
                // and bounded by its own timeout.
                if self.observer.is_none() && (options.once || stopping || !self.claiming) {
                    break;
                }
                thread::sleep(if self.observer.is_some() {
                    options.tick
                } else {
                    options.idle_poll
                });
                continue;
            }
            self.tick();
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
    fn fill_slots(&mut self, parallel: usize) -> Result<()> {
        if self.slots.len() < parallel {
            self.adopt_stale_runs(parallel)?;
        }
        // Takes no slot: a dead run goes to the triage below.
        self.recover_dead_runs()?;
        if self.slots.len() < parallel {
            self.apply_landing_answers(parallel)?;
        }
        self.apply_triage_answers()?;
        if self.slots.len() < parallel {
            self.resume_parked_runs(parallel)?;
        }
        if self.slots.len() < parallel {
            self.triage_runs(parallel)?;
        }
        while self.slots.len() < parallel {
            // Most-releasing candidate first, lowest ID on a tie (ADR-0023);
            // `graph` shows the same order, so it is not recorded.
            let order = dependency_graph(self.queue.graph_input()?, None).candidates;
            if order.is_empty() {
                break;
            }
            let base = self.repository.main_head()?;
            let run = match self
                .queue
                .claim_for_supervisor_in_order(&base, &self.token, &order)?
            {
                ClaimOutcome::Claimed { run } => *run,
                ClaimOutcome::NoReadyTask => break,
            };
            match self.provision(&run) {
                Ok(watch) => {
                    let run = self.queue.run(run.id())?;
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Session(watch),
                    });
                }
                Err(error) => {
                    let message = format!("run {} provisioning failed: {error:#}", run.id());
                    self.log
                        .note(&format!("{message}; no further tasks will be claimed"));
                    self.abandon(&run, message.clone());
                    self.claiming = false;
                    self.provisioning_error = Some(message);
                    break;
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        let mut index = 0;
        while index < self.slots.len() {
            let mut slot = self.slots.remove(index);
            match self.step(&mut slot) {
                Ok(Step::Continue) => {
                    self.slots.insert(index, slot);
                    index += 1;
                }
                Ok(Step::Done(run)) => {
                    self.log
                        .note(&format!("run {} is {}", run.id(), run.status().as_str()));
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
                    self.log.note(&format!("run {}: {message}", slot.run.id()));
                    if let Err(error) = self.queue.release_lease(slot.run.id(), &self.token) {
                        self.log.note(&format!(
                            "run {}: could not release the lease: {error:#}",
                            slot.run.id()
                        ));
                    }
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
                Err(error) if matches!(slot.phase, Phase::Resume(_)) => {
                    let message = format!("{error:#}");
                    self.log.note(&format!(
                        "run {} resume stopped: {message}; its workspace is kept for inspection",
                        slot.run.id()
                    ));
                    let (attempt, workspace) = match &slot.phase {
                        Phase::Resume(watch) => (watch.attempt, Some(watch.workspace.clone())),
                        _ => unreachable!("matched a resume"),
                    };
                    self.give_up_resume(&slot.run, attempt, workspace.as_deref(), message);
                }
                Err(error) => {
                    // Creation/communication failures can be ambiguous: the
                    // session may be alive. Disown the run, delete nothing,
                    // and keep serving the other slots. A headless review in
                    // progress is stopped: nobody would read its verdict.
                    stop_job(&mut slot);
                    let message = format!("{error:#}");
                    self.log.note(&format!(
                        "run {} retained for inspection: {message}; see show {} and doctor",
                        slot.run.id(),
                        slot.run.task_id()
                    ));
                    self.abandon(&slot.run, message);
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
                self.log
                    .note(&format!("observer schedule could not be read: {error:#}"));
                return;
            }
        };
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
                self.log.note(&format!(
                    "observer ({}) started: pid {}",
                    mode.as_str(),
                    child.id()
                ));
                self.observer = Some((mode, child));
            }
            Err(error) => self.log.note(&format!(
                "observer ({}) could not start: {error:#}",
                mode.as_str()
            )),
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
                self.log
                    .note(&format!("observer ({}) exited: {status}", mode.as_str()));
                self.observer = None;
            }
            Err(error) => {
                self.log.note(&format!(
                    "observer ({}) could not be waited for: {error:#}",
                    mode.as_str()
                ));
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
        self.log.note(&message);
        self.errors.push(RunError {
            run_id: slot.run.id().clone(),
            task_id: slot.run.task_id(),
            message,
        });
    }

    fn abandon(&mut self, run: &TaskRun, message: String) {
        if let Err(error) = self.queue.abandon_run(run.id(), &self.token, &message) {
            self.log.note(&format!(
                "run {}: could not record the error: {error:#}",
                run.id()
            ));
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
    ) {
        let workspace = workspace.map(str::to_owned);
        // The workspace is recorded so a later pass knows it for this run's
        // own once its session has ended (`close_left_resume_workspaces`).
        let payload = json!({
            "attempt": attempt,
            "outcome": "error",
            "error": message,
            "workspace_id": workspace,
            "exhausted": attempt >= MAX_RESUME_ATTEMPTS,
        });
        if let Err(error) =
            self.queue
                .finish_resume(run.id(), &self.token, None, None, false, payload)
        {
            self.log.note(&format!(
                "run {}: could not record the resume error: {error:#}",
                run.id()
            ));
        }
        let session_may_live = self.queue.processes(run.id()).map_or(true, |processes| {
            processes
                .iter()
                .any(|p| p.role == "wrapper" && p.exited_at.is_none())
        });
        if let Some(workspace) = workspace {
            if session_may_live {
                self.log.note(&format!(
                    "run {}: resume workspace {workspace} is kept; its session may still run",
                    run.id()
                ));
            } else if let Err(error) = self.cmux.close(&workspace) {
                self.log.note(&format!(
                    "run {}: resume workspace {workspace} could not be closed: {error:#}",
                    run.id()
                ));
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
                    self.log.note(&format!(
                        "run {} landing: {}",
                        slot.run.id(),
                        outcome["outcome"]
                    ));
                }
                Err(error) => {
                    let message = format!("landing failed: {error:#}");
                    self.log.note(&format!("run {}: {message}", slot.run.id()));
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
                if !self
                    .queue
                    .runs_with_status(RunStatus::Integrating)?
                    .is_empty()
                {
                    return Ok(Step::Continue);
                }
                let previous = self.queue.run(slot.run.id())?.status();
                let main = self.repository.main_head()?;
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
                self.log.note(&format!(
                    "run {} lands onto main {main} ({})",
                    run.id(),
                    previous.as_str()
                ));
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
                let validation = handle
                    .take()
                    .context("validation already joined")?
                    .join()
                    .map_err(|_| anyhow!("validation thread panicked"))??;
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
                    Ok(verdict) => {
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
                        self.log.note(&format!(
                            "run {} review {attempt}: {} ({})",
                            run.id(),
                            verdict.verdict.as_str(),
                            verdict.summary
                        ));
                        self.act_on_verdict(&run, session, verdict)?
                    }
                    Err(error) => {
                        self.log.note(&format!(
                            "run {} review {attempt} failed: {error}; the run waits for a review by hand",
                            run.id()
                        ));
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
                        self.log.note(&format!(
                            "run {} rewrote its receipt for {label} (head {head}); validating again",
                            slot.run.id()
                        ));
                        let run = self.queue.restart_validation(slot.run.id(), &self.token)?;
                        let handle = self.validate(run.clone());
                        slot.run = run;
                        slot.phase = Phase::Validating(Some(handle), Some(session));
                    }
                    ReviseOutcome::Mismatch(why) => {
                        let message = revise_mismatch_request(&slot.run, &label, &why)?;
                        // Only what the session writes after this counts.
                        let sent_at = self.files.now();
                        match self.cmux.send_text(&session.workspace, &message) {
                            Ok(()) => {
                                watch.sent_at = sent_at;
                                let kind = match watch.fix {
                                    Fix::Revise(_) => "revise_receipt_rejected",
                                    Fix::Conflict(_) => "conflict_receipt_rejected",
                                };
                                self.queue.record_runtime_event(
                                    slot.run.id(),
                                    kind,
                                    json!({"attempt": watch.attempt, "reason": why}),
                                )?;
                                self.log.note(&format!(
                                    "run {}: {why}; asked the session to fix it ({label})",
                                    slot.run.id()
                                ));
                            }
                            Err(error) => {
                                let why = format!(
                                    "{why}, and the request to fix it could not be sent: {error:#}"
                                );
                                self.log.note(&format!("run {}: {why}", slot.run.id()));
                                let then = watch.fix.ask(why.clone(), why);
                                slot.phase = Phase::Exiting(ExitWatch::new(Some(session), then));
                            }
                        }
                    }
                    ReviseOutcome::Ended(why) => {
                        self.log.note(&format!(
                            "run {}: the session {why} after {label}; asking a person",
                            slot.run.id()
                        ));
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
                        self.log
                            .note(&format!("run {} waits for a person in ask {ask}", run.id()));
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs,
                    } => {
                        self.queue.record_runtime_event(
                            run.id(),
                            "review_failed",
                            json!({
                                "attempt": attempt,
                                "error": error,
                                "duration_secs": duration_secs,
                                "status": run.status().as_str(),
                            }),
                        )?;
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

    /// Land `run`, which holds the integration slot under this token, on a
    /// thread (`previous` is where an error before `main` moved returns it).
    /// It pushes unless an approving `integrate --no-push` recorded
    /// `push: false`; a run landed on a passed review always pushes.
    fn spawn_landing(
        &self,
        run: TaskRun,
        previous: RunStatus,
        main: CommitSha,
    ) -> Result<thread::JoinHandle<Result<IntegrationOutcome>>> {
        let queues = self.queues.clone();
        let repository = self.repository.clone();
        let remote = self.remote.clone();
        let verifier = self.verifier.clone();
        let processes = self.processes.clone();
        let pid = self.layout.pid;
        let token = self.token.clone();
        let common_dir = path_text(&self.layout.common_dir)?;
        let push = self
            .queue
            .run_events(run.id())?
            .iter()
            .find(|e| e.kind == "integration_approved")
            .is_none_or(|e| e.payload.get("push") != Some(&json!(false)));
        let generators = self.generators.clone();
        Ok(thread::spawn(move || {
            let mut queue = queues.open()?;
            integration::land_integrating(
                &mut Integration {
                    queue: &mut *queue,
                    repository: &*repository,
                    verifier: &*verifier,
                    remote: push.then_some(&*remote as &dyn MainRemote),
                    common_dir: &common_dir,
                    clock: &*generators.clock,
                    ids: &*generators.ids,
                    processes: &*processes,
                    pid,
                },
                &run,
                previous,
                &main,
                &token,
            )
        }))
    }

    /// Record `review_started` and start the headless review of an accepted
    /// run whose session stays open (ADR-0027 decision 1): write
    /// `review.md`, then run the reviewer's command with the task's
    /// acceptance and the verdict schema. A review that cannot even start
    /// is a failed one.
    fn start_review(&mut self, run: &TaskRun, session: Option<SessionRef>) -> Result<Phase> {
        let attempt = self
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "review_started")
            .count()
            + 1;
        let live = match &session {
            Some(_) => session_alive(&*self.queue, run.id())?,
            None => false,
        };
        self.queue.record_runtime_event(
            run.id(),
            "review_started",
            json!({
                "attempt": attempt,
                "workspace_id": session.as_ref().map(|s| s.workspace.clone()),
                "session_live": live,
            }),
        )?;
        Ok(match self.spawn_review(run, attempt) {
            Ok((child, stdout, stderr)) => {
                self.log.note(&format!(
                    "run {} review {attempt} started (session {})",
                    run.id(),
                    if live { "kept open" } else { "ended" }
                ));
                Phase::Review(ReviewWatch {
                    session,
                    attempt,
                    job: HeadlessJob {
                        what: "review",
                        child,
                        started: Instant::now(),
                        timeout: self.reviewer.review_timeout(),
                        stdout,
                        stderr,
                    },
                })
            }
            Err(error) => {
                let error = format!("the headless review could not start: {error:#}");
                self.log.note(&format!("run {}: {error}", run.id()));
                Phase::Exiting(ExitWatch::new(
                    session,
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs: 0,
                    },
                ))
            }
        })
    }

    fn spawn_review(
        &mut self,
        run: &TaskRun,
        attempt: usize,
    ) -> Result<(Box<dyn Spawned>, PathBuf, PathBuf)> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let material = (self.review_material)(run.task_id())?;
        let path = material["path"]
            .as_str()
            .context("review wrote no path")?
            .to_owned();
        let task = self.queue.show(run.task_id())?.task;
        let prompt = review_prompt(&task, run, &path);
        self.files.write(
            &run_dir.join(format!("review-prompt-{attempt}.txt")),
            prompt.as_bytes(),
        )?;
        let stdout = run_dir.join(format!("review-{attempt}.out"));
        let stderr = run_dir.join(format!("review-{attempt}.err"));
        let mut command = self.reviewer.review_command(run, &prompt)?;
        // The repository's [run.env] reaches the review too (ADR-0023
        // decision 3).
        command
            .envs(self.verifier.run_env(&run_dir)?)
            // Like the observer's job: the CLI knows the review by its role
            // and allows it only reads of this queue.
            .envs(self.layout.job_env.iter().cloned());
        let child = self
            .spawner
            .spawn(
                &command,
                Streams::Files {
                    stdout: &stdout,
                    stderr: &stderr,
                },
            )
            .context("start the review")?;
        Ok((child, stdout, stderr))
    }

    /// Move on from a verdict: `pass` exits the session and lands; `revise`
    /// goes to the live session while revises are left (ADR-0027 decision
    /// 2); anything else exits the session and asks a person.
    fn act_on_verdict(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
    ) -> Result<Phase> {
        let ask = |why: Option<String>, verdict: ReviewVerdict, session| {
            Phase::Exiting(ExitWatch::new(
                session,
                AfterExit::Ask {
                    decision: verdict.verdict,
                    reasons: verdict.reasons,
                    summary: verdict.summary,
                    why,
                },
            ))
        };
        match verdict.verdict {
            ReviewDecision::Pass => self.precheck(run, session, verdict),
            ReviewDecision::Concern => Ok(ask(None, verdict, session)),
            ReviewDecision::Revise => {
                let revises = self
                    .queue
                    .run_events(run.id())?
                    .iter()
                    .filter(|e| e.kind == "revise_requested")
                    .count();
                if revises >= MAX_REVISE_ATTEMPTS {
                    let why = format!("the review still asks for changes after {revises} revises");
                    return Ok(ask(Some(why), verdict, session));
                }
                let Some(live) = session
                    .clone()
                    .filter(|_| session_alive(&*self.queue, run.id()).unwrap_or(false))
                else {
                    let why = "the session had ended, so nobody could revise the run".to_owned();
                    return Ok(ask(Some(why), verdict, session));
                };
                let attempt = revises + 1;
                let task = self.queue.show(run.task_id())?.task;
                let message = revise_request(&task, run, attempt, &verdict.reasons)?;
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                self.files.write(
                    &run_dir.join(format!("revise-{attempt}.txt")),
                    message.as_bytes(),
                )?;
                let sent_at = self.files.now();
                if let Err(error) = self.cmux.send_text(&live.workspace, &message) {
                    let why = format!("the revise request could not be sent: {error:#}");
                    self.log.note(&format!("run {}: {why}", run.id()));
                    return Ok(ask(Some(why), verdict, session));
                }
                self.queue.record_runtime_event(
                    run.id(),
                    "revise_requested",
                    json!({"attempt": attempt, "reasons": verdict.reasons, "sent_at": unix_seconds(sent_at)}),
                )?;
                self.log.note(&format!(
                    "revise {attempt} of {MAX_REVISE_ATTEMPTS} sent to run {} in workspace {}",
                    run.id(),
                    live.workspace
                ));
                Ok(Phase::Revise(ReviseWatch {
                    session: live,
                    attempt,
                    fix: Fix::Revise(verdict.reasons),
                    sent_at,
                    sent: Instant::now(),
                }))
            }
        }
    }

    /// Before a passed run's session is asked to exit, judge with `git
    /// merge-tree` whether its head conflicts with the current main,
    /// without touching the worktree (ADR-0027 decision 4). A clean merge
    /// exits the session and lands. A conflict records `conflict_precheck`
    /// and sends the live session the resolution request of a resume; the
    /// session's rewritten receipt is validated and reviewed again. The
    /// requests and the run's resumes share `MAX_RESUME_ATTEMPTS`: past it,
    /// the session exits and a person is asked. Without a live session to
    /// ask (or when Git cannot judge), the run lands as before, and a
    /// conflicting landing parks it for a resume.
    fn precheck(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
    ) -> Result<Phase> {
        let land = |session| Phase::Exiting(ExitWatch::new(session, AfterExit::Land));
        let head = run
            .result_commit()
            .cloned()
            .context("accepted run has no result commit")?;
        let main = self.repository.main_head()?;
        let conflicts = match self
            .repository
            .merge_conflicts(main.as_str(), head.as_str())
        {
            Ok(conflicts) => conflicts,
            Err(error) => {
                self.log.note(&format!(
                    "run {}: the conflict precheck against main {main} failed: {error:#}; landing",
                    run.id()
                ));
                return Ok(land(session));
            }
        };
        if conflicts.is_empty() {
            return Ok(land(session));
        }
        let events = self.queue.run_events(run.id())?;
        let requested = events
            .iter()
            .filter(|e| e.kind == "conflict_precheck" && e.payload["requested"] == true)
            .count();
        let resumes = events.iter().filter(|e| e.kind == "resume_started").count();
        let attempt = requested + 1;
        let mut payload = json!({
            "main": main,
            "head": head,
            // Recorded for the reader; a failure to find it does not stop the request.
            "merge_base": self.repository.merge_base(main.as_str(), head.as_str()).ok().flatten(),
            "conflicts": conflicts,
            "attempt": attempt,
            "requested": false,
        });
        let why = format!(
            "git merge-tree finds that main {main} conflicts with the run in {}",
            conflicts.join(", ")
        );
        if requested + resumes >= MAX_RESUME_ATTEMPTS {
            let why = format!("{why}, after {requested} conflict requests and {resumes} resumes");
            // What an adopter asks, if it takes the run over before the ask.
            payload["asked"] = json!(why);
            self.queue
                .record_runtime_event(run.id(), "conflict_precheck", payload)?;
            self.log
                .note(&format!("run {}: {why}; asking a person", run.id()));
            return Ok(Phase::Exiting(ExitWatch::new(
                session,
                Fix::Conflict(verdict).ask(String::new(), why),
            )));
        }
        let live = session
            .clone()
            .filter(|_| session_alive(&*self.queue, run.id()).unwrap_or(false));
        let sent = match &live {
            Some(live) => {
                let task = self.queue.show(run.task_id())?.task;
                let landed = landed_since(&mut *self.queue, &*self.repository, run, &main)?;
                let request = ResumeRequest {
                    main: main.clone(),
                    reason: why.clone(),
                    kind: ResumeKind::Precheck,
                };
                let message = resume_request(&task, run, &request, &landed)?;
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                self.files.write(
                    &run_dir.join(format!("conflict-{attempt}.txt")),
                    message.as_bytes(),
                )?;
                let sent_at = self.files.now();
                self.cmux
                    .send_text(&live.workspace, &message)
                    .map(|()| sent_at)
                    .map_err(|error| format!("the request could not be sent: {error:#}"))
            }
            None => Err("the session had ended".to_owned()),
        };
        let (Some(live), Ok(sent_at)) = (live, &sent) else {
            let error = sent.err().unwrap_or_default();
            payload["error"] = json!(error);
            self.queue
                .record_runtime_event(run.id(), "conflict_precheck", payload)?;
            self.log.note(&format!(
                "run {}: {why}, and {error}; landing, whose rebase parks it for a resume",
                run.id()
            ));
            return Ok(land(session));
        };
        payload["requested"] = json!(true);
        payload["sent_at"] = json!(unix_seconds(*sent_at));
        self.queue
            .record_runtime_event(run.id(), "conflict_precheck", payload)?;
        self.log.note(&format!(
            "run {}: {why}; asked its live session in workspace {} to rebase (request {attempt})",
            run.id(),
            live.workspace
        ));
        Ok(Phase::Revise(ReviseWatch {
            session: live,
            attempt,
            fix: Fix::Conflict(verdict),
            sent_at: *sent_at,
            sent: Instant::now(),
        }))
    }

    /// Close the session's workspace after it exited: the worker's own
    /// through [`close_workspace`], a resume's by recording
    /// `workspace_closed` with its attempt.
    fn close_session(&mut self, run: &TaskRun, session: &SessionRef) -> Result<TaskRun> {
        match session.resume {
            None if run.workspace_closed_at().is_none() && run.workspace_id().is_some() => {
                close_workspace(&mut *self.queue, self.cmux, &self.token, run, &*self.log)
            }
            None => Ok(run.clone()),
            Some(attempt) => {
                match self.cmux.close(&session.workspace) {
                    Ok(()) => self.queue.record_runtime_event(
                        run.id(),
                        "workspace_closed",
                        json!({"workspace_id": session.workspace, "resume_attempt": attempt}),
                    )?,
                    Err(error) => {
                        let message = format!(
                            "resume workspace {} could not be closed: {error:#}",
                            session.workspace
                        );
                        self.log.note(&format!("run {}: {message}", run.id()));
                        self.queue.record_cleanup_failure(run.id(), &message)?;
                    }
                }
                self.queue.run(run.id())
            }
        }
    }

    /// Open the `approve_landing` ask of a run whose review did not pass
    /// (ADR-0027, ADR-0022 decision 3) and notify the inbox; returns its ID.
    fn open_landing_ask(
        &mut self,
        run: &TaskRun,
        decision: ReviewDecision,
        reasons: &[String],
        summary: &str,
        why: Option<&str>,
    ) -> Result<i64> {
        let mut question = format!(
            "The supervisor's review of run {} (task {}) returned {}{}: {summary}",
            run.id(),
            run.task_id(),
            decision.as_str(),
            why.map(|why| format!(" ({why})")).unwrap_or_default()
        );
        for reason in reasons {
            question.push_str(&format!("\n- {reason}"));
        }
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!("\nReview material: {run_dir}/review.md"));
        }
        question.push_str(
            "\nland: land it as it is. send_back: resume the session with these reasons. cancel: fail the run and cancel the task.",
        );
        // Through `ask`, like the CLI: a new ask notifies the inbox.
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: LANDING_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".to_owned(),
            },
            self.cmux,
        )?;
        outcome["id"].as_i64().context("ask returned no id")
    }

    /// Apply the answered `approve_landing` asks of runs awaiting
    /// integration that nobody leases (ADR-0027): `land` lands the run in
    /// the single slot (as an approved one), `send_back` makes it
    /// `needs_session` for a resume that names the review's reasons, and
    /// `cancel` fails the run and cancels its task. The ask is closed once
    /// applied; any other answer is left to the inbox. An error is
    /// noted and the ask is tried again on a later pass.
    fn apply_landing_answers(&mut self, parallel: usize) -> Result<()> {
        for ask in self.queue.landing_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            if run.status() != RunStatus::AwaitingIntegration
                || !LANDING_OPTIONS.contains(&answer.as_str())
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            if answer == "land"
                && (self.slots.len() >= parallel
                    || !self
                        .queue
                        .runs_with_status(RunStatus::Integrating)?
                        .is_empty())
            {
                continue;
            }
            if let Err(error) = self.apply_landing_answer(&run, ask.id, &answer) {
                self.log.note(&format!(
                    "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}",
                    run.id(),
                    ask.id
                ));
            }
        }
        Ok(())
    }

    fn apply_landing_answer(&mut self, run: &TaskRun, ask_id: i64, answer: &str) -> Result<()> {
        let payload = json!({"ask_id": ask_id, "answer": answer});
        match answer {
            "land" => {
                if !self.queue.has_run_event(run.id(), "integration_approved")? {
                    self.queue.record_runtime_event(
                        run.id(),
                        "integration_approved",
                        json!({"status": run.status().as_str(), "pid": self.layout.pid, "push": true, "ask_id": ask_id}),
                    )?;
                }
                let main = self.repository.main_head()?;
                let landing = self.queue.begin_integration(run.id(), &self.token, &main)?;
                self.queue.close_ask(ask_id)?;
                self.log.note(&format!(
                    "run {} lands onto main {main} as ask {ask_id} answered",
                    run.id()
                ));
                let handle =
                    self.spawn_landing(landing.clone(), RunStatus::AwaitingIntegration, main)?;
                self.slots.push(Slot {
                    run: landing,
                    phase: Phase::Landing(Some(handle)),
                });
            }
            "send_back" => {
                let reasons = latest_review_reasons(&*self.queue, run.id())?;
                let reason = format!(
                    "the review's findings were sent back by ask {ask_id}: {}",
                    if reasons.is_empty() {
                        "(no reasons recorded)".to_owned()
                    } else {
                        reasons.join("; ")
                    }
                );
                self.queue
                    .decide_landing(run.id(), RunStatus::NeedsSession, &reason, payload)?;
                self.queue.close_ask(ask_id)?;
                self.log.note(&format!(
                    "run {} was sent back by ask {ask_id}; it waits for a resume",
                    run.id()
                ));
            }
            _ => {
                let reason = format!("canceled by ask {ask_id}");
                self.queue
                    .decide_landing(run.id(), RunStatus::Failed, &reason, payload)?;
                self.queue.transition(run.task_id(), TaskAction::Cancel)?;
                self.queue.close_ask(ask_id)?;
                self.log.note(&format!(
                    "run {} failed and task {} was canceled by ask {ask_id}",
                    run.id(),
                    run.task_id()
                ));
            }
        }
        Ok(())
    }

    /// Recover the unfinished runs nobody leases whose wrapper exited or
    /// died (ADR-0024 decision 3, amending ADR-0012): `recover`'s own check
    /// (no live process of the run; `doctor`'s blockers empty) on
    /// `claimed` / `starting` / `running` / `validating` runs without a
    /// lease row. They become `interrupted` with `run_recovered` (`by:
    /// supervisor`) and go to the triage, never straight to `ready`. A run
    /// that changed meanwhile is left for a later pass.
    fn recover_dead_runs(&mut self) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.active_runs()? {
            if run.status() == RunStatus::Integrating || self.queue.run_lease(run.id())?.is_some() {
                continue;
            }
            let processes = self.queue.processes(run.id())?;
            let health = run_health(&run, &processes, None, now, &*self.processes, &*self.files);
            if !health.recoverable {
                continue;
            }
            let report = json!({"run": health, "by": "supervisor"});
            match self.queue.recover_run(run.id(), processes.len(), report) {
                Ok(recovered) => self.log.note(&format!(
                    "run {} of task {} recovered from {}: nobody leases it and its session is gone; it goes to triage",
                    recovered.id(),
                    recovered.task_id(),
                    run.status().as_str()
                )),
                Err(error) => self.log.note(&format!(
                    "run {} could not be recovered: {error:#}",
                    run.id()
                )),
            }
        }
        Ok(())
    }

    /// Apply the answered `decide` asks of the triage (one of
    /// [`TRIAGE_OPTIONS`]) to their `failed` / `interrupted` run nobody
    /// leases: `retry` readies the task, `resume` parks the run as
    /// `needs_session` with the triage's reason, `cancel` cancels the task;
    /// the ask is closed with it. An ask whose task is no longer in progress,
    /// or has a newer run, has nothing left to apply and is closed. Any other answer is a
    /// person's to read.
    fn apply_triage_answers(&mut self) -> Result<()> {
        for ask in self.queue.triage_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            // Only an option the ask offered: a run whose resumes are used
            // up is not offered `resume`.
            if !TRIAGE_OPTIONS.contains(&answer.as_str())
                || !ask.options.contains(&answer)
                || !matches!(run.status(), RunStatus::Failed | RunStatus::Interrupted)
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            let detail = self.queue.show(run.task_id())?;
            if detail.task.status() != TaskStatus::InProgress
                || detail
                    .runs
                    .last()
                    .is_some_and(|latest| *latest.id() != *run.id())
            {
                self.log.note(&format!(
                    "ask {} of run {} is closed: task {} moved on without it",
                    ask.id,
                    run.id(),
                    run.task_id()
                ));
                self.queue.close_ask(ask.id)?;
                continue;
            }
            let reason = self
                .queue
                .run_events(run.id())?
                .iter()
                .rev()
                .find(|e| e.kind == "triage_finished")
                .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
                .map_or_else(
                    || run.last_error().map(str::to_owned).unwrap_or_default(),
                    str::to_owned,
                );
            let reason = format!("{reason} (a person chose {answer} in ask {})", ask.id);
            match self.queue.decide_triage(run.id(), ask.id, &answer, &reason) {
                Ok(decided) => self.log.note(&format!(
                    "run {} of task {}: {answer} as ask {} answered; the run is {}",
                    decided.id(),
                    decided.task_id(),
                    ask.id,
                    decided.status().as_str()
                )),
                Err(error) => self.log.note(&format!(
                    "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}",
                    run.id(),
                    ask.id
                )),
            }
        }
        Ok(())
    }

    /// Start the triage of `failed` / `interrupted` runs not triaged since
    /// their last resume, while slots are free (ADR-0024 decision 3). A run
    /// someone leases (a session still asked to exit) waits. A triage that
    /// cannot even start fails right away.
    fn triage_runs(&mut self, parallel: usize) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.runs_to_triage()? {
            if self.slots.len() >= parallel {
                break;
            }
            if triage_state(&self.queue.run_events(run.id())?) != TriageState::Pending
                || self
                    .queue
                    .run_lease(run.id())?
                    .is_some_and(|lease| !self.lease_stale(&lease, now))
            {
                continue;
            }
            let Some((run, attempt)) = self.queue.begin_triage(run.id(), &self.token)? else {
                continue;
            };
            match self.spawn_triage(&run, attempt) {
                Ok(watch) => {
                    self.log.note(&format!(
                        "run {} of task {} ({}) triage {attempt} started",
                        run.id(),
                        run.task_id(),
                        run.status().as_str()
                    ));
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Triage(watch),
                    });
                }
                Err(error) => {
                    let error = format!("the headless triage could not start: {error:#}");
                    self.fail_triage(&run, attempt, error, 0);
                    let run = self.queue.run(run.id())?;
                    self.note_triaged(&run);
                }
            }
        }
        Ok(())
    }

    /// Write the triage's prompt and start the headless job in the run's
    /// directory, allowed to read only (ADR-0024 decision 2).
    fn spawn_triage(&mut self, run: &TaskRun, attempt: usize) -> Result<TriageWatch> {
        let dir = match &run.run_dir() {
            Some(dir) => PathBuf::from(dir),
            None => self.layout.runs_dir.join(run.id().as_str()),
        };
        self.files
            .create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let detail = self.queue.show(run.task_id())?;
        let resumes = resume_attempts(&*self.queue, run.id());
        let prompt = triage_prompt(&*self.files, &detail, run, resumes, &dir)?;
        self.files.write(
            &dir.join(format!("triage-prompt-{attempt}.txt")),
            prompt.as_bytes(),
        )?;
        let stdout = dir.join(format!("triage-{attempt}.out"));
        let stderr = dir.join(format!("triage-{attempt}.err"));
        let mut command = self
            .reviewer
            .headless_command(&dir, &prompt, TRIAGE_TOOLS)?;
        // Like the review: the CLI knows the job by its role and allows it
        // only reads of this queue.
        command.envs(self.layout.job_env.iter().cloned());
        let child = self
            .spawner
            .spawn(
                &command,
                Streams::Files {
                    stdout: &stdout,
                    stderr: &stderr,
                },
            )
            .context("start the triage")?;
        Ok(TriageWatch {
            attempt,
            job: HeadlessJob {
                what: "triage",
                child,
                started: Instant::now(),
                timeout: self.reviewer.review_timeout(),
                stdout,
                stderr,
            },
        })
    }

    /// Act on the triage's verdict (ADR-0024 decision 3). The runtime's own
    /// rules come first: a task with [`TRIAGE_RETRY_FAILURES`] failed or
    /// interrupted runs is not retried, and a run without resumes left or
    /// without a worktree is not resumed; either becomes an ask. Then
    /// `triage_finished` with the action, the workspaces the run left open
    /// are closed, and the lease is released.
    fn act_on_triage(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        duration_secs: u64,
        verdict: TriageVerdict,
    ) -> Result<TaskRun> {
        let failures = self
            .queue
            .show(run.task_id())?
            .runs
            .iter()
            .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
            .count();
        ensure!(
            self.queue.holds_lease(run.id(), &self.token)?,
            "the triage's lease of run {} was lost",
            run.id()
        );
        let resumes = resume_attempts(&*self.queue, run.id());
        let worktree = run
            .worktree_path()
            .is_some_and(|path| self.files.is_dir(Path::new(path)));
        let overridden = match verdict.verdict {
            TriageDecision::Retry if failures >= TRIAGE_RETRY_FAILURES => Some(format!(
                "task {} has {failures} failed or interrupted runs, so it is not retried without a person",
                run.task_id()
            )),
            TriageDecision::Resume if resumes >= MAX_RESUME_ATTEMPTS => Some(format!(
                "the run was resumed {resumes} times already (at most {MAX_RESUME_ATTEMPTS})"
            )),
            TriageDecision::Resume if !worktree || run.receipt_path().is_none() => {
                Some("the run has no worktree a session could resume in".to_owned())
            }
            _ => None,
        };
        let action = match (verdict.verdict, &overridden) {
            (TriageDecision::Retry, None) => TriageAction::Retry,
            (TriageDecision::Resume, None) => TriageAction::Resume {
                instruction: if verdict.instruction.trim().is_empty() {
                    verdict.reason.clone()
                } else {
                    verdict.instruction.clone()
                },
            },
            _ => TriageAction::Ask {
                ask_id: self.open_triage_ask(run, attempt, &verdict, overridden.as_deref())?,
            },
        };
        let payload = json!({
            "attempt": attempt,
            "verdict": verdict.verdict,
            "reason": verdict.reason,
            "instruction": verdict.instruction,
            "overridden": overridden,
            "failures": failures,
            "duration_secs": duration_secs,
        });
        let triaged = self
            .queue
            .finish_triage(run.id(), &self.token, &action, payload)?;
        self.log.note(&format!(
            "run {} triage {attempt}: {}{} ({}); the run is {}",
            run.id(),
            verdict.verdict.as_str(),
            match &overridden {
                Some(why) => format!(" became ask: {why}"),
                None => String::new(),
            },
            verdict.reason,
            triaged.status().as_str()
        ));
        // The verdict is acted on: what fails from here on is logged, not
        // a failed triage.
        if let Err(error) = self.close_triaged_workspaces(&triaged) {
            self.log.note(&format!(
                "run {}: its workspaces could not all be closed: {error:#}",
                run.id()
            ));
        }
        if let Err(error) = self.queue.release_lease(run.id(), &self.token) {
            self.log.note(&format!(
                "run {}: could not release the lease: {error:#}",
                run.id()
            ));
        }
        self.queue.run(run.id())
    }

    /// Open the triage's `decide` ask (options [`TRIAGE_OPTIONS`]) through
    /// `ask`, so the inbox is notified; returns its ID.
    fn open_triage_ask(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        verdict: &TriageVerdict,
        overridden: Option<&str>,
    ) -> Result<i64> {
        let asked = match (verdict.verdict, overridden) {
            (TriageDecision::Ask, _) if !verdict.instruction.trim().is_empty() => {
                verdict.instruction.clone()
            }
            (_, Some(why)) => format!(
                "the triage answered {} ({}), but {why}",
                verdict.verdict.as_str(),
                verdict.instruction.trim()
            ),
            _ => "what should happen to this run?".to_owned(),
        };
        let mut question = format!(
            "The supervisor's triage of run {} (task {}, {}) asks a person: {asked}\nReason: {}\nLast error: {}",
            run.id(),
            run.task_id(),
            run.status().as_str(),
            verdict.reason,
            or_none(tail(run.last_error().unwrap_or_default(), 500))
        );
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!(
                "\nTriage material: {run_dir}/triage-prompt-{attempt}.txt"
            ));
        }
        question.push_str(
            "\nretry: make the task ready for a new run. resume: resume the run's own session with the triage's reason. cancel: cancel the task.",
        );
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: TRIAGE_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: TRIAGE_ASKER.to_owned(),
            },
            self.cmux,
        )?;
        outcome["id"].as_i64().context("ask returned no id")
    }

    /// Close the workspaces a triaged run left open: its worker workspace
    /// (unless the runtime closed it) and the resume workspaces its
    /// `resume_finished` events name as not closed, each only while cmux
    /// still lists it. A close records `workspace_closed` (`by: triage`); a
    /// cmux failure records `cleanup_failed` and the others go on. A
    /// `stuck_exit` ask of the run is closed with its workspace.
    fn close_triaged_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let mut workspaces: Vec<String> = run
            .workspace_id()
            .map(str::to_owned)
            .filter(|_| run.workspace_closed_at().is_none())
            .into_iter()
            .collect();
        for event in self.queue.run_events(run.id())? {
            if event.kind == "resume_finished"
                && event.payload["workspace_closed"] != true
                && let Some(workspace) = event.payload.get("workspace_id").and_then(Value::as_str)
                && !workspaces.iter().any(|w| w == workspace)
            {
                workspaces.push(workspace.to_owned());
            }
        }
        let mut closed = false;
        for workspace in workspaces {
            let result = self.cmux.exists(&workspace).and_then(|open| {
                if open {
                    self.cmux.close(&workspace)?;
                }
                Ok(open)
            });
            match result {
                Ok(true) => {
                    self.queue.triage_closed_workspace(run.id(), &workspace)?;
                    closed = true;
                }
                Ok(false) => {}
                Err(error) => {
                    let message = format!("workspace {workspace} could not be closed: {error:#}");
                    self.log.note(&format!("run {}: {message}", run.id()));
                    self.queue.record_runtime_event(
                        run.id(),
                        "cleanup_failed",
                        json!({"workspace_id": workspace, "message": message}),
                    )?;
                }
            }
        }
        if closed {
            self.queue
                .close_stuck_exit_asks(run.id(), "the triage closed the run's workspace")?;
        }
        // Whatever path took the run out of `running`, no dialog of it waits
        // for an answer any more.
        self.queue
            .close_answer_prompt_asks(run.id(), "the run was triaged; closed by the runtime")?;
        Ok(())
    }

    /// Record `triage_failed` (a person triages the run) and give the lease
    /// back; the run stays as it is.
    fn fail_triage(&mut self, run: &TaskRun, attempt: usize, error: String, duration_secs: u64) {
        self.log.note(&format!(
            "run {} triage {attempt} failed: {error}; the run waits for a triage by hand",
            run.id()
        ));
        let recorded = self.queue.record_runtime_event(
            run.id(),
            "triage_failed",
            json!({
                "attempt": attempt,
                "error": error,
                "duration_secs": duration_secs,
                "status": run.status().as_str(),
            }),
        );
        if let Err(error) = recorded {
            self.log.note(&format!(
                "run {}: could not record the triage failure: {error:#}",
                run.id()
            ));
        }
        if self
            .queue
            .holds_lease(run.id(), &self.token)
            .unwrap_or(false)
            && let Err(error) = self.queue.release_lease(run.id(), &self.token)
        {
            self.log.note(&format!(
                "run {}: could not release the lease: {error:#}",
                run.id()
            ));
        }
    }

    fn note_triaged(&mut self, run: &TaskRun) {
        let task = self
            .queue
            .show(run.task_id())
            .map(|detail| detail.task.status());
        self.log.note(&format!(
            "run {} triaged: the run is {}{}",
            run.id(),
            run.status().as_str(),
            match task {
                Ok(status) => format!(", task {} is {}", run.task_id(), status.as_str()),
                Err(_) => String::new(),
            }
        ));
        self.triaged.push(json!({
            "run_id": run.id(),
            "task_id": run.task_id(),
            "status": run.status(),
        }));
    }

    /// Resume `needs_session` runs with attempts left (ADR-0019 decision 1),
    /// oldest first, while slots are free: a run with a lease that is not
    /// stale, or whose last session still runs, is someone's already.
    fn resume_parked_runs(&mut self, parallel: usize) -> Result<()> {
        let candidates = self.queue.runs_needing_session()?;
        // A resumed session let go after the exit timeout raised a
        // stuck_exit ask; once it ended nobody needs to answer it, whether
        // or not a slot is free.
        for candidate in &candidates {
            let alive = candidate
                .wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            if !alive {
                for ask in self
                    .queue
                    .close_stuck_exit_asks(candidate.run.id(), STUCK_EXIT_CLOSED)?
                {
                    self.log.note(&format!(
                        "session of {} exited; closed its stuck_exit ask {}",
                        candidate.run.id(),
                        ask.id
                    ));
                }
            }
        }
        for candidate in candidates {
            let ResumeCandidate {
                run,
                lease,
                wrapper,
                attempts,
            } = candidate;
            let now = self.generators.clock.now();
            let session_alive = wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            // A previous session whose wrapper process lives on, however
            // silent, is never joined by a second one on the same worktree.
            if lease.is_some_and(|lease| !self.lease_stale(&lease, now)) || session_alive {
                continue;
            }
            // Out of attempts: a person decides, whether or not a slot is free.
            if attempts >= MAX_RESUME_ATTEMPTS {
                if let Err(error) = self.exhaust_resumes(&run, attempts) {
                    self.log.note(&format!(
                        "run {}: its used-up resumes could not be handed to a person: {error:#}",
                        run.id()
                    ));
                }
                continue;
            }
            if self.slots.len() >= parallel {
                break;
            }
            self.close_left_resume_workspaces(&run)?;
            let main = self.repository.main_head()?;
            if let Some(head) = self.resolved_head(&run, &main)? {
                self.skip_resume(&run, &head, &main)?;
                continue;
            }
            let (reason, kind) = resume_reason(&*self.queue, &run)?;
            let Some((run, attempt)) = self.queue.begin_resume(
                run.id(),
                &self.token,
                &main,
                reason.as_deref(),
                MAX_RESUME_ATTEMPTS,
            )?
            else {
                continue;
            };
            let request = ResumeRequest {
                main,
                reason: reason.unwrap_or_else(|| "(no reason recorded)".to_owned()),
                kind,
            };
            match self.start_resume(&run, attempt, &request) {
                Ok(watch) => {
                    self.log.note(&format!(
                        "run {} of task {} resumed (attempt {attempt} of {MAX_RESUME_ATTEMPTS}) in workspace {}",
                        run.id(), run.task_id(), watch.workspace
                    ));
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Resume(watch),
                    });
                }
                Err(error) => {
                    let message = format!("run {} could not be resumed: {error:#}", run.id());
                    self.log.note(&message);
                    self.give_up_resume(&run, attempt, None, message);
                }
            }
        }
        Ok(())
    }

    /// The worktree head of a `needs_session` run an earlier resume already
    /// resolved although it was not judged so (its session rewrote the
    /// receipt before the attempt that saw it, or went idle without
    /// rewriting it again): the run was last parked by the landing or
    /// validation (not a person's `send_back`, `landing_decided`), the last
    /// of the parking events, `resume_finished` and `resume_skipped` is a
    /// `resume_finished` with `outcome: unresolved` (a session ran; a resume
    /// that could not start changed nothing), the receipt parses with
    /// this run's `run_id`, `succeeded` and the task's required evidence,
    /// its `commit` is the head of a clean worktree, and that head has
    /// `main` as a proper ancestor. `None` whenever one of these fails or
    /// cannot be read, and the run is resumed as before. Back in
    /// `needs_session` after a skip, however it got there, the run needs a
    /// resume first, so a skip never repeats without one.
    fn resolved_head(&mut self, run: &TaskRun, main: &CommitSha) -> Result<Option<CommitSha>> {
        const PARKING: [&str; 5] = [
            "integration_deferred",
            "integration_error",
            "evidence_missing",
            "scope_violation",
            "landing_decided",
        ];
        let events = self.queue.run_events(run.id())?;
        let parked = events
            .iter()
            .rev()
            .find(|e| PARKING.contains(&e.kind.as_str()));
        let last = events.iter().rev().find(|e| {
            PARKING.contains(&e.kind.as_str())
                || matches!(e.kind.as_str(), "resume_finished" | "resume_skipped")
        });
        if parked.is_none_or(|e| e.kind == "landing_decided")
            || last
                .is_none_or(|e| e.kind != "resume_finished" || e.payload["outcome"] != "unresolved")
        {
            return Ok(None);
        }
        let (Some(worktree), Some(receipt_path)) = (&run.worktree_path(), &run.receipt_path())
        else {
            return Ok(None);
        };
        let Some(receipt) = self
            .files
            .read_to_string(Path::new(receipt_path))
            .ok()
            .and_then(|text| Receipt::parse(&text).ok())
        else {
            return Ok(None);
        };
        let task = self.queue.show(run.task_id())?.task;
        if receipt.run_id != *run.id().as_str()
            || receipt.result != ReceiptResult::Succeeded
            || !receipt
                .missing_evidence(task.required_evidence())
                .is_empty()
        {
            return Ok(None);
        }
        let worktree = Path::new(worktree);
        let Ok(head) = self.repository.head(worktree) else {
            return Ok(None);
        };
        let resolved = head.as_str() == receipt.commit.to_ascii_lowercase()
            && head != *main
            && self
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty())
            && self
                .repository
                .is_ancestor(main.as_str(), head.as_str())
                .unwrap_or(false);
        Ok(resolved.then_some(head))
    }

    /// Move a run [`Self::resolved_head`] found resolved on without opening
    /// a session or using an attempt: record `resume_skipped` and, under
    /// its lease, land it when its integrate was approved, or validate and
    /// review it (with no session to keep) otherwise.
    fn skip_resume(&mut self, run: &TaskRun, head: &CommitSha, main: &CommitSha) -> Result<()> {
        let approved = self.queue.has_run_event(run.id(), "integration_approved")?;
        let Some(run) = self
            .queue
            .skip_resume(run.id(), &self.token, head, main, approved)?
        else {
            return Ok(());
        };
        self.log.note(&format!(
            "run {} of task {} was already resolved at {head} on main {main}; {} without a resume",
            run.id(),
            run.task_id(),
            if approved {
                "landing it"
            } else {
                "validating it"
            }
        ));
        let phase = if approved {
            Phase::AwaitingSlot
        } else {
            Phase::Validating(Some(self.validate(run.clone())), None)
        };
        self.slots.push(Slot { run, phase });
        Ok(())
    }

    /// Hand a `needs_session` run whose resumes are used up to a person
    /// through the triage's `decide` ask (ADR-0024's Consequences): no
    /// headless triage runs, since resuming is no longer an option and a
    /// run that did not resolve in [`MAX_RESUME_ATTEMPTS`] sessions is not
    /// retried without a person. The ask (options `retry` and `cancel`,
    /// applied like a triage's answer) is opened first, then the run becomes
    /// `failed` with `triage_finished` naming the ask, and the workspaces it
    /// left open are closed as after a triage. A run of a task that moved
    /// on is left alone.
    fn exhaust_resumes(&mut self, run: &TaskRun, attempts: usize) -> Result<()> {
        let detail = self.queue.show(run.task_id())?;
        if detail.task.status() != TaskStatus::InProgress
            || detail
                .runs
                .last()
                .is_some_and(|latest| *latest.id() != *run.id())
        {
            return Ok(());
        }
        let last_error = run.last_error().map(str::to_owned).unwrap_or_default();
        let reason = format!(
            "resumed {attempts} times (at most {MAX_RESUME_ATTEMPTS}) and still needs a session: {}",
            tail(&last_error, 500)
        );
        let mut question = format!(
            "Run {} of task {} ({}) was resumed {attempts} times (at most {MAX_RESUME_ATTEMPTS}) and still needs a session, so the supervisor stops resuming it.\nLast error: {}",
            run.id(),
            run.task_id(),
            detail.task.title(),
            or_none(tail(&last_error, 500))
        );
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!("\nRun directory: {run_dir}"));
        }
        question.push_str(
            "\nretry: make the task ready for a new run. cancel: cancel the task. To change the task first, answer with what to change instead.",
        );
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: EXHAUSTED_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: TRIAGE_ASKER.to_owned(),
            },
            self.cmux,
        )?;
        let ask_id = outcome["id"].as_i64().context("ask returned no id")?;
        let Some(failed) =
            self.queue
                .exhaust_resumes(run.id(), MAX_RESUME_ATTEMPTS, ask_id, &reason)?
        else {
            // The run changed meanwhile (another supervisor took it): an ask
            // this pass opened has nothing left to decide.
            if outcome["created"] == true {
                self.queue.answer(
                    ask_id,
                    "withdrawn: the run changed before it was handed over",
                )?;
                self.queue.close_ask(ask_id)?;
            }
            return Ok(());
        };
        self.log.note(&format!(
            "run {} of task {} used up its resumes; it is failed and waits for ask {ask_id}",
            failed.id(),
            failed.task_id()
        ));
        self.close_triaged_workspaces(&failed)?;
        self.note_triaged(&failed);
        Ok(())
    }

    /// Close the resume workspaces earlier attempts of this run left open
    /// (a session let go after the exit timeout, or one that might have
    /// lived when a resume failed), found by the IDs recorded in its
    /// `resume_finished` events (ADR-0026). The caller checked that no
    /// session of the run is alive.
    fn close_left_resume_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let left: Vec<String> = self
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "resume_finished" && e.payload["workspace_closed"] != true)
            .filter_map(|e| e.payload.get("workspace_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        for workspace in left {
            if self.cmux.exists(&workspace)? {
                self.log.note(&format!(
                    "run {}: closing resume workspace {workspace} left by an earlier attempt; its session has ended",
                    run.id()
                ));
                self.cmux.close(&workspace)?;
            }
        }
        Ok(())
    }

    /// Write the resolution request, refresh the runtime snapshot (the one
    /// the worker ran may predate `session --resume`) and open the resume
    /// workspace with the same wrapper and settings as the worker's.
    fn start_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        request: &ResumeRequest,
    ) -> Result<ResumeWatch> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        ensure!(
            self.files.is_dir(worktree),
            "worktree {} is missing",
            worktree.display()
        );
        let task = self.queue.show(run.task_id())?.task;
        let landed = landed_since(&mut *self.queue, &*self.repository, run, &request.main)?;
        let message = resume_request(&task, run, request, &landed)?;
        self.files.write(
            &run_dir.join(format!("resume-{attempt}.txt")),
            message.as_bytes(),
        )?;
        self.files
            .copy(&self.layout.runner, &run_dir.join("runner"))
            .context("snapshot runtime binary")?;
        let command = shell_join(&[
            path_text(&run_dir.join("runner"))?,
            "--db".into(),
            path_text(&self.layout.db)?,
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            self.token.clone(),
            "--claude".into(),
            path_text(&self.layout.claude)?,
            "--resume".into(),
        ]);
        // The worker's env and group (the same session of the run) and the
        // description `run <run-id> resume` (ADR-0028).
        let tags = WorkspaceTags {
            env: self.layout.worker_env.clone(),
            description: Some(resume_workspace_description(run)),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create_resume(&task, run, &command, &tags)?;
        Ok(ResumeWatch {
            workspace,
            attempt,
            run_dir,
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at: self.files.now(),
            startup: Instant::now(),
            message,
            agent_seen: None,
            message_sent: None,
            exit_requested: None,
            required_evidence: task.required_evidence().to_vec(),
            approved: self.queue.has_run_event(run.id(), "integration_approved")?,
            silent: false,
            exit_for_silence: false,
        })
    }

    /// The resumed session ended, or resolved the run: record
    /// `resume_finished` and move the run on. A resolved run whose
    /// integrate was approved has exited; its workspace is closed and it
    /// keeps its lease and waits for the landing slot. An unapproved
    /// resolved run keeps its session and lease and goes through
    /// validation and review like the worker's (ADR-0027 decision 3); a
    /// `failed` receipt ends the run; anything else leaves it
    /// `needs_session` for the next attempt, or for a human after the last.
    fn finish_resumed_session(
        &mut self,
        slot: &mut Slot,
        attempt: usize,
        workspace: &str,
        verdict: ResumeVerdict,
    ) -> Result<Step> {
        let approved = self
            .queue
            .has_run_event(slot.run.id(), "integration_approved")?;
        let reviewed = matches!(verdict.kind, ResumeOutcome::Resolved) && !approved;
        // A session let go after the exit timeout still runs: its
        // workspace stays, and blocks the next attempt until it ends. A
        // session going on to review keeps it until the verdict.
        let closed = !verdict.exit_timed_out
            && !reviewed
            && match self.cmux.close(workspace) {
                Ok(()) => true,
                Err(error) => {
                    self.log.note(&format!(
                        "run {}: resume workspace {workspace} could not be closed: {error:#}",
                        slot.run.id()
                    ));
                    false
                }
            };
        let mut payload = json!({
            "attempt": attempt,
            "outcome": verdict.outcome(),
            "head": verdict.head,
            "workspace_id": workspace,
            "workspace_closed": closed,
            "approved": approved,
        });
        if verdict.exit_timed_out {
            payload["exit_timed_out"] = json!(true);
        }
        if reviewed {
            payload["session_live"] = json!(verdict.live);
        }
        let id = slot.run.id().clone();
        let run = match verdict.kind {
            ResumeOutcome::Resolved if approved => {
                let run = self
                    .queue
                    .finish_resume(&id, &self.token, None, None, true, payload)?;
                slot.run = run;
                slot.phase = Phase::AwaitingSlot;
                return Ok(Step::Continue);
            }
            ResumeOutcome::Resolved => {
                let run = self.queue.finish_resume(
                    &id,
                    &self.token,
                    Some(RunStatus::Validating),
                    None,
                    true,
                    payload,
                )?;
                let handle = self.validate(run.clone());
                slot.run = run;
                slot.phase = Phase::Validating(
                    Some(handle),
                    Some(SessionRef {
                        workspace: workspace.to_owned(),
                        resume: Some(attempt),
                    }),
                );
                return Ok(Step::Continue);
            }
            ResumeOutcome::Failed(reason) => self.queue.finish_resume(
                &id,
                &self.token,
                Some(RunStatus::Failed),
                Some(&reason),
                false,
                payload,
            )?,
            ResumeOutcome::Unresolved => {
                payload["exhausted"] = json!(attempt >= MAX_RESUME_ATTEMPTS);
                self.queue
                    .finish_resume(&id, &self.token, None, None, false, payload)?
            }
        };
        Ok(Step::Done(Box::new(run)))
    }

    /// Take over `running` / `validating` runs whose lease went stale under
    /// another token while their wrapper is alive (heartbeat within the
    /// lease TTL) or has already reported its exit (ADR-0012). A wrapper
    /// that is dead or silent is `recover`'s business; a run without a
    /// lease was abandoned or recovered on purpose and is never adopted.
    /// The staleness is judged here and again inside `adopt_run`, so two
    /// supervisors racing for one run take it exactly once.
    fn adopt_stale_runs(&mut self, parallel: usize) -> Result<()> {
        for candidate in self.queue.runs_leased_by_others(&self.token)? {
            if self.slots.len() >= parallel {
                break;
            }
            let now = self.generators.clock.now();
            let LeasedRun {
                run,
                lease,
                wrapper,
            } = candidate;
            if !self.lease_stale(&lease, now) {
                continue;
            }
            // A run moved on by `resume_skipped` has no session of its own
            // since: its supervisor alone owned it, whatever the wrapper of
            // an earlier session left behind.
            let skipped = self.skipped_resume(run.id())?;
            let alive = wrapper.as_ref().and_then(|wrapper| {
                wrapper.exited_at.is_none().then(|| {
                    self.processes.alive(wrapper.pid)
                        && now - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
                })
            });
            if !skipped && (wrapper.is_none() || alive == Some(false)) {
                continue;
            }
            let observed = match &wrapper {
                Some(wrapper) => json!({
                    "pid": wrapper.pid,
                    "alive": alive,
                    "exited_at": wrapper.exited_at,
                }),
                None => Value::Null,
            };
            let pid = self.layout.pid;
            let Some(run) =
                self.queue
                    .adopt_run(run.id(), &lease.token, &self.token, pid, observed)?
            else {
                self.log.note(&format!(
                    "run {} was not adopted: its lease changed while judging it",
                    run.id()
                ));
                continue;
            };
            self.log.note(&format!(
                "run {} adopted from supervisor {} (pid {}, heartbeat {}s old; wrapper pid {} {}): task {} in workspace {}",
                run.id(),
                lease.token,
                lease.pid,
                now - lease.heartbeat_at,
                wrapper.as_ref().map_or(0, |w| w.pid),
                match wrapper.as_ref().map(|w| w.exited_at) {
                    Some(Some(at)) => format!("exited at {at}"),
                    Some(None) if alive == Some(true) => "alive".to_owned(),
                    Some(None) => "gone".to_owned(),
                    None => "none since resume_skipped".to_owned(),
                },
                run.task_id(),
                run.workspace_id().unwrap_or("?")
            ));
            let phase = if run.status() == RunStatus::AwaitingIntegration {
                self.adopt_review(&run)
            } else {
                self.resume(&run)
            };
            match phase {
                Ok(phase) => self.slots.push(Slot { run, phase }),
                Err(error) => {
                    // The lease is this process's now; give it up like any
                    // other runtime error so `recover` can judge the run.
                    let message = format!("run {} could not be resumed: {error:#}", run.id());
                    self.log.note(&message);
                    self.abandon(&run, message);
                }
            }
        }
        Ok(())
    }

    /// Rebuild the slot of an adopted run from what the queue and the run
    /// directory hold: the planned paths, whether the receipt is already on
    /// disk and whether `/exit` was already requested (never sent twice; its
    /// timeout restarts now, and an `exit_request_timed_out` already recorded
    /// is not recorded again). The wrapper is registered, so no registration
    /// timeout applies. A `validating` run restarts validation from the
    /// beginning: it is a function of the receipt and the worktree alone.
    fn resume(&self, run: &TaskRun) -> Result<Phase> {
        Ok(match run.status() {
            RunStatus::Validating => {
                Phase::Validating(Some(self.validate(run.clone())), self.session_of(run)?)
            }
            _ => {
                let receipt_path =
                    PathBuf::from(run.receipt_path().context("missing receipt path")?);
                let receipt_seen = self.files.is_file(&receipt_path)
                    && self.queue.has_run_event(run.id(), "receipt_observed")?;
                let exit_requested = self
                    .queue
                    .has_run_event(run.id(), "exit_requested")?
                    .then(Instant::now);
                let exit_timed_out = self
                    .queue
                    .has_run_event(run.id(), "exit_request_timed_out")?;
                let first_commit_seen = self
                    .queue
                    .has_run_event(run.id(), "first_commit_observed")?;
                // A dialog recorded before adoption is not recorded again
                // while the same screen stays up.
                let prompt_hash = self
                    .queue
                    .run_events(run.id())?
                    .into_iter()
                    .rev()
                    .find(|e| {
                        matches!(
                            e.kind.as_str(),
                            "prompt_waiting" | "prompt_cleared" | "receipt_observed"
                        )
                    })
                    .filter(|e| e.kind == "prompt_waiting")
                    .and_then(|e| e.payload["screen_hash"].as_str().map(str::to_owned));
                Phase::Session(SessionWatch {
                    workspace: run
                        .workspace_id()
                        .map(str::to_owned)
                        .context("adopted run has no workspace")?,
                    run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
                    receipt_path,
                    idle_marker: run.idle_marker_path()?,
                    startup: Instant::now(),
                    receipt_seen,
                    receipt_seen_at: receipt_seen.then(Instant::now),
                    exit_requested,
                    exit_timed_out,
                    first_commit_seen,
                    agent_seen: None,
                    prompt_checked: None,
                    prompt_hash,
                    // A timeout recorded without its ask (by a binary that
                    // made none, or a supervisor that died between the two)
                    // still gets one; one asked before is not asked again.
                    exit_asked: !exit_timed_out || self.queue.has_stuck_exit_ask(run.id())?,
                    // Only a run whose wrapper heartbeats is adopted.
                    silent: false,
                    exit_for_silence: false,
                })
            }
        })
    }

    /// Whether the run's last resume event is `resume_skipped`: it was moved
    /// on without a session, and no resume opened one since.
    fn skipped_resume(&self, id: &RunId) -> Result<bool> {
        Ok(self
            .queue
            .run_events(id)?
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.kind.as_str(),
                    "resume_started" | "resume_finished" | "resume_skipped"
                )
            })
            .is_some_and(|e| e.kind == "resume_skipped"))
    }

    /// The session an accepted run keeps open (ADR-0027): the workspace of
    /// the resume that handed its live session to validation
    /// (`resume_finished` with status `validating`) unless a
    /// `workspace_closed` of that resume followed, else the worker's own
    /// workspace while it is not closed.
    fn session_of(&self, run: &TaskRun) -> Result<Option<SessionRef>> {
        let events = self.queue.run_events(run.id())?;
        let resumed = events.iter().rev().find(|e| {
            matches!(
                e.kind.as_str(),
                "resume_started" | "resume_finished" | "resume_skipped"
            )
        });
        // A run moved on by `resume_skipped` has no session open.
        if let Some(event) = resumed {
            if event.kind == "resume_finished"
                && event.payload["status"] == RunStatus::Validating.as_str()
                && let (Some(workspace), Some(attempt)) = (
                    event.payload["workspace_id"].as_str(),
                    event.payload["attempt"].as_u64(),
                )
            {
                let closed = events.iter().any(|e| {
                    e.id > event.id
                        && e.kind == "workspace_closed"
                        && e.payload["workspace_id"] == workspace
                });
                return Ok((!closed).then(|| SessionRef {
                    workspace: workspace.to_owned(),
                    resume: Some(attempt as usize),
                }));
            }
            return Ok(None);
        }
        Ok(run
            .workspace_id()
            .map(str::to_owned)
            .filter(|_| run.workspace_closed_at().is_none())
            .map(|workspace| SessionRef {
                workspace,
                resume: None,
            }))
    }

    /// Rebuild an adopted `awaiting_integration` run under review from its
    /// events: a `revise_requested` with nothing after it waits for the live
    /// session again; a verdict already recorded (`review_finished` with
    /// nothing after it), or an approved run not reviewed since its
    /// validation, goes on to its `/exit` without a second review and
    /// without a second `/exit` if one was already requested; anything else
    /// is reviewed from the start, the review being a function of the
    /// receipt and the commit.
    fn adopt_review(&mut self, run: &TaskRun) -> Result<Phase> {
        let session = self.session_of(run)?;
        let events = self.queue.run_events(run.id())?;
        let Some(anchor) = events.iter().rev().find(|e| {
            matches!(
                e.kind.as_str(),
                "validation_finished"
                    | "review_started"
                    | "review_finished"
                    | "revise_requested"
                    | "revise_finished"
                    | "conflict_precheck"
                    | "conflict_resolved"
            )
        }) else {
            return self.start_review(run, session);
        };
        let then = match anchor.kind.as_str() {
            "revise_requested" => {
                if let Some(live) = session.clone()
                    && session_alive(&*self.queue, run.id())?
                {
                    return Ok(Phase::Revise(ReviseWatch {
                        session: live,
                        attempt: anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        fix: Fix::Revise(
                            serde_json::from_value(anchor.payload["reasons"].clone())
                                .unwrap_or_default(),
                        ),
                        sent_at: UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        sent: Instant::now(),
                    }));
                }
                None
            }
            // A conflict request with nothing after it waits for the live
            // session again, with the passed verdict before it.
            "conflict_precheck" if anchor.payload["requested"] == true => {
                let passed = passed_before(&events, anchor.id);
                if let Some(live) = session.clone()
                    && let Some(verdict) = passed
                    && session_alive(&*self.queue, run.id())?
                {
                    return Ok(Phase::Revise(ReviseWatch {
                        session: live,
                        attempt: anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        fix: Fix::Conflict(verdict),
                        sent_at: UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        sent: Instant::now(),
                    }));
                }
                None
            }
            "review_finished" => {
                match serde_json::from_value::<ReviewVerdict>(json!({
                    "verdict": anchor.payload["verdict"],
                    "reasons": anchor.payload["reasons"],
                    "summary": anchor.payload["summary"],
                })) {
                    // A pass not yet followed by its /exit is prechecked
                    // (again): main may have moved.
                    Ok(verdict)
                        if verdict.verdict == ReviewDecision::Pass
                            && !events
                                .iter()
                                .any(|e| e.id > anchor.id && e.kind == "exit_requested") =>
                    {
                        return self.precheck(run, session, verdict);
                    }
                    Ok(verdict) if verdict.verdict == ReviewDecision::Pass => Some(AfterExit::Land),
                    Ok(verdict) => Some(AfterExit::Ask {
                        why: (verdict.verdict == ReviewDecision::Revise).then(|| {
                            "the revise could not go on when the supervisor was replaced".to_owned()
                        }),
                        decision: verdict.verdict,
                        reasons: verdict.reasons,
                        summary: verdict.summary,
                    }),
                    Err(_) => None,
                }
            }
            // A precheck that sent nothing decided to land (no session to
            // ask) or to ask a person (past the limit); before its /exit it
            // is prechecked again, as main may have moved.
            "conflict_precheck" => match passed_before(&events, anchor.id) {
                Some(verdict)
                    if !events
                        .iter()
                        .any(|e| e.id > anchor.id && e.kind == "exit_requested") =>
                {
                    return self.precheck(run, session, verdict);
                }
                Some(verdict) => Some(match anchor.payload["asked"].as_str() {
                    Some(why) => Fix::Conflict(verdict).ask(String::new(), why.to_owned()),
                    None => AfterExit::Land,
                }),
                None => None,
            },
            "validation_finished" if events.iter().any(|e| e.kind == "integration_approved") => {
                Some(AfterExit::Land)
            }
            _ => None,
        };
        let Some(then) = then else {
            return self.start_review(run, session);
        };
        let after = |kind: &str| events.iter().any(|e| e.id > anchor.id && e.kind == kind);
        let mut watch = ExitWatch::new(session, then);
        // Never a second /exit; its timeout restarts now.
        if after("exit_requested") {
            watch.requested = Some(Instant::now());
        }
        watch.timed_out = after("exit_request_timed_out");
        // A timeout recorded without its ask still gets one; one asked
        // before is not asked again (as for a running run, task 104).
        watch.exit_asked = !watch.timed_out || self.queue.has_stuck_exit_ask(run.id())?;
        Ok(Phase::Exiting(watch))
    }

    /// Whether a lease no longer has a working process behind it: its pid
    /// is dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
    fn lease_stale(&self, lease: &RunLease, now: i64) -> bool {
        heartbeat_stale(self.processes.alive(lease.pid), now - lease.heartbeat_at)
    }

    /// Start the validation of `run` on a thread (see [`spawn_validation`]).
    fn validate(&self, run: TaskRun) -> thread::JoinHandle<Result<Validation>> {
        spawn_validation(
            self.queues.clone(),
            self.repository.clone(),
            run,
            self.log.clone(),
        )
    }

    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
    /// The queue's workspace group, asked for with every run workspace:
    /// the call is idempotent by external ID, and cmux removes a group whose
    /// last workspace closes, so a handle kept from an earlier run could
    /// name a group that is gone. A group cmux cannot make is a warning in
    /// the log, and the run opens outside it.
    fn workspace_group(&self) -> Option<String> {
        let name = workspace_group_name(&self.layout.repo_root);
        match self.cmux.ensure_group(&self.layout.queue_hash, &name) {
            Ok(group) => Some(group),
            Err(error) => {
                self.log.note(&format!(
                    "warning: cmux workspace group {name:?} (external ID {}) could not be made, \
so the run workspace opens outside it: {error:#}",
                    self.layout.queue_hash
                ));
                None
            }
        }
    }

    fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = &self.layout.runs_dir;
        let paths = RunPaths::new(state_dir, claimed.id());
        let run_dir = paths.run_dir.clone();
        let plan = RunPlan {
            repo_path: path_text(&self.layout.repo_root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("dagq/{}", claimed.id()),
            worktree_path: path_text(&paths.worktree)?,
            receipt_path: path_text(&paths.receipt)?,
            log_path: path_text(&paths.log)?,
        };
        // Save intended paths before any external resource is created.
        self.queue.plan_run(claimed.id(), &self.token, &plan)?;
        self.files.create_dir_all(state_dir)?;
        self.files
            .create_new_dir(&run_dir)
            .context("run directory must be new")?;
        let run_env = self.verifier.run_env(&run_dir)?;
        let run = self.queue.run(claimed.id())?;
        let task = self.queue.show(run.task_id())?.task;
        let predecessors: Vec<PredecessorSummary> = self
            .queue
            .predecessors(task.id())?
            .iter()
            .map(PredecessorSummary::from_predecessor)
            .collect();
        let goal = match task.goal_id() {
            Some(goal_id) => Some(self.queue.show_goal(goal_id)?.goal),
            None => None,
        };
        let siblings = siblings_in_progress(&task, self.queue.tasks_in_progress()?);
        self.files.write(
            &run_dir.join("prompt.txt"),
            prompt(&task, &run, goal.as_ref(), &predecessors, &siblings)?.as_bytes(),
        )?;
        // A running wrapper must not change when the development binary is rebuilt.
        self.files
            .copy(&self.layout.runner, &run_dir.join("runner"))
            .context("snapshot runtime binary")?;
        let git_output = self.repository.create_worktree(&run)?;
        self.files
            .write(&run_dir.join("worktree-create.txt"), git_output.as_bytes())?;
        self.queue.record_runtime_event(
            run.id(),
            "worktree_created",
            json!({"path": plan.worktree_path, "branch": plan.branch}),
        )?;
        let command = shell_join(&[
            path_text(&run_dir.join("runner"))?,
            "--db".into(),
            path_text(&self.layout.db)?,
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            self.token.clone(),
            "--claude".into(),
            path_text(&self.layout.claude)?,
        ]);
        let mut env = self.layout.worker_env.clone();
        env.extend(run_env);
        let tags = WorkspaceTags {
            env,
            description: Some(workspace_description(
                SessionRole::Worker,
                &self.layout.queue_hash,
                Some(run.id()),
                Some(run.task_id()),
            )),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create(&task, &run, &command, &tags)?;
        self.queue
            .workspace_created(run.id(), &self.token, &workspace)?;
        self.log.note(&format!(
            "task {} running in workspace {}; run {}",
            run.task_id(),
            workspace,
            run.id()
        ));
        Ok(SessionWatch {
            workspace,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            receipt_seen_at: None,
            exit_requested: None,
            exit_timed_out: false,
            first_commit_seen: false,
            agent_seen: None,
            prompt_checked: None,
            prompt_hash: None,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
        })
    }
}

/// Watches one session: wrapper registration and heartbeat, receipt and idle
/// marker, the single exit request, and the wrapper's exit.
struct SessionWatch {
    workspace: String,
    run_dir: PathBuf,
    receipt_path: PathBuf,
    idle_marker: PathBuf,
    startup: Instant,
    receipt_seen: bool,
    /// When this supervisor first saw the receipt, for the wait on
    /// background work the session left running after it.
    receipt_seen_at: Option<Instant>,
    exit_requested: Option<Instant>,
    /// `exit_request_timed_out` is recorded once per run; the lease is kept.
    exit_timed_out: bool,
    /// `first_commit_observed` is recorded (also by a previous supervisor).
    first_commit_seen: bool,
    /// When this supervisor first saw the agent registered.
    agent_seen: Option<Instant>,
    /// When the screen was last read for a dialog.
    prompt_checked: Option<Instant>,
    /// `screen_hash` of the dialog last recorded as `prompt_waiting` and not
    /// cleared since.
    prompt_hash: Option<String>,
    /// The `stuck_exit` ask of the exit timeout is registered (also by a
    /// previous supervisor).
    exit_asked: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    silent: bool,
    /// The `/exit` was sent because of that silence.
    exit_for_silence: bool,
}

impl SessionWatch {
    /// One observation. `Some` once supervision finished (`validating` or
    /// `failed`): the wrapper exited, or the session went idle after its
    /// receipt and stays open for the review; an error means the run must
    /// be retained. An `/exit` an earlier supervisor already requested is
    /// waited out as before.
    fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<TaskRun>> {
        let processes = sv.queue.processes(run.id())?;
        self.watch_first_commit(sv, run)?;
        if !self.receipt_seen && sv.files.is_file(&self.receipt_path) {
            self.receipt_seen = true;
            self.receipt_seen_at = Some(Instant::now());
            sv.queue.record_runtime_event(
                run.id(),
                "receipt_observed",
                json!({"path": path_text(&self.receipt_path)?, "validated": false}),
            )?;
            sv.log.note(&format!(
                "receipt received for {}; waiting for the session to go idle (or a person's /exit)",
                run.id()
            ));
        }
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        // A session that already ended (on its own, by a person's /exit,
        // or before this supervisor adopted the run) is not asked to exit.
        let session_ended = wrapper.is_some_and(|w| w.exited_at.is_some());
        // Background work the session left running after its receipt is
        // waited for up to the resume timeout, like a resumed session's:
        // work that never ends must not hold the run without an attention.
        // Past it the run goes on, and a /exit held back by the dialog
        // becomes a stuck_exit ask.
        let waited_out = self
            .receipt_seen_at
            .is_some_and(|at| at.elapsed() >= sv.cmux.resume_timeout());
        if self.receipt_seen
            && self.exit_requested.is_none()
            && !session_ended
            && let Some(evidence) = match IdleMarker::read(&*sv.files, &self.idle_marker)? {
                Some(idle) if waited_out => {
                    idle.stopped_after_receipt(&*sv.files, &self.receipt_path)?
                }
                Some(idle) => idle.idle_after_receipt(&*sv.files, &self.receipt_path)?,
                None => None,
            }
        {
            sv.queue
                .record_runtime_event(run.id(), "session_idle_observed", evidence)?;
            // The session stays open through validation and review, and
            // is asked to exit only once the verdict is known (ADR-0027
            // decision 1).
            sv.log.note(&format!(
                "session of {} is idle after its receipt; validating with the session open",
                run.id()
            ));
            return sv
                .queue
                .finish_supervision_live(run.id(), &sv.token)
                .map(Some);
        }
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                match sv.cmux.capture(&self.workspace) {
                    Ok(screen) => sv
                        .files
                        .write(&self.run_dir.join("terminal-final.txt"), screen.as_bytes())?,
                    Err(error) => sv.queue.record_runtime_event(
                        run.id(),
                        "screen_capture_failed",
                        json!({"error": format!("{error:#}")}),
                    )?,
                }
                // Nobody needs to send /exit to a session that exited, nor
                // answer its dialog.
                for ask in sv
                    .queue
                    .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
                {
                    sv.log.note(&format!(
                        "session of {} exited; closed its stuck_exit ask {}",
                        run.id(),
                        ask.id
                    ));
                }
                close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
                return sv.queue.finish_supervision(run.id(), &sv.token).map(Some);
            }
            let pulse = wrapper_pulse(
                sv,
                run,
                wrapper,
                &self.workspace,
                &mut self.silent,
                "wrapper heartbeat expired; session may still be alive",
            )?;
            match pulse {
                WrapperPulse::Silent if self.exit_requested.is_none() => {
                    // The same single /exit a finished session gets,
                    // recorded before it is sent.
                    let timeout = sv.cmux.exit_timeout();
                    sv.queue.record_runtime_event(
                        run.id(),
                        "exit_requested",
                        json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                    )?;
                    sv.cmux.send_exit(&self.workspace)?;
                    sv.log.note(&format!(
                        "exit requested for {} after its wrapper went silent; waiting for session exit",
                        run.id()
                    ));
                    self.exit_requested = Some(Instant::now());
                    self.exit_for_silence = true;
                }
                WrapperPulse::Silent => (),
                WrapperPulse::Exited => return Ok(None),
                WrapperPulse::Fresh => {
                    if self.exit_requested.is_none() {
                        self.deliver_answers(sv, run)?;
                    }
                    if let Some(agent) = processes.iter().find(|p| p.role == "agent") {
                        self.watch_prompt(sv, run, agent)?;
                    }
                }
            }
        } else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "wrapper did not register within {} seconds",
                timeout.as_secs()
            );
        }
        if let Some(requested) = self.exit_requested
            && !self.exit_timed_out
        {
            let timeout = sv.cmux.exit_timeout();
            if requested.elapsed() >= timeout {
                // Something in the session (for example a dialog) held the
                // /exit back. Keep the lease and keep watching: the run
                // proceeds to validation once the session exits. /exit is not
                // sent again, since it could pick another option of a dialog.
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_request_timed_out",
                    json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                sv.log.note(&format!(
                    "session for {} did not exit within {}s of the exit request; keeping the run and asking the inbox to send /exit in workspace {}",
                    run.id(),
                    timeout.as_secs(),
                    self.workspace
                ));
                self.exit_timed_out = true;
            }
        }
        if self.exit_timed_out && !self.exit_asked {
            ask_stuck_exit(
                sv,
                run,
                &self.workspace,
                &stuck_exit_after(
                    self.exit_for_silence,
                    "The run stays running, and goes on to validating once the session exits",
                ),
            )?;
            self.exit_asked = true;
        }
        Ok(None)
    }

    /// Record `first_commit_observed` once, the first time the worktree's
    /// HEAD is seen away from the run's base commit: with `agent_started` it
    /// measures how long a session takes to start working (`stats`'s
    /// `startup`). The time is when this poll saw it, at most a tick late.
    /// A HEAD that cannot be read is noted and checked again next poll.
    fn watch_first_commit(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if self.first_commit_seen {
            return Ok(());
        }
        let Some(worktree) = run.worktree_path() else {
            return Ok(());
        };
        let head = match sv.repository.head(Path::new(worktree)) {
            Ok(head) => head,
            Err(error) => {
                sv.log.note(&format!(
                    "HEAD of {} could not be read for its first commit: {error:#}",
                    run.id()
                ));
                return Ok(());
            }
        };
        if head != *run.base_commit() {
            sv.queue.record_runtime_event(
                run.id(),
                "first_commit_observed",
                json!({"commit": head, "base_commit": run.base_commit()}),
            )?;
            self.first_commit_seen = true;
        }
        Ok(())
    }

    /// Read the screen of a session that has run for `prompt_wait` with
    /// neither a receipt nor an idle marker, its wrapper and agent alive, and
    /// record a dialog found there as `prompt_waiting` (once per screen) and
    /// its disappearance as `prompt_cleared`. No key is sent (ADR-0019). A
    /// dialog is raised to the inbox as an `answer_prompt` ask with the
    /// screen's excerpt (ADR-0024's Consequences), which the runtime closes
    /// once the dialog is gone, the receipt arrives or the session exits.
    fn watch_prompt(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        agent: &RunProcess,
    ) -> Result<()> {
        let started = *self.agent_seen.get_or_insert_with(Instant::now);
        let wait = sv.cmux.prompt_wait();
        if self.receipt_seen {
            // `receipt_observed` ends the dialog by itself.
            if self.prompt_hash.take().is_some() {
                close_answer_prompt_asks(sv, run, PROMPT_RECEIPT_CLOSED)?;
            }
            return Ok(());
        }
        if sv.files.exists(&self.idle_marker)
            || !sv.processes.alive(agent.pid)
            || sv.queue.has_unclosed_worker_question(run.id())?
        {
            // The agent finished a response, is gone, or stopped at an ask
            // that waits for its answer: no dialog holds it now, and a
            // recorded one must not stay an attention.
            return self.clear_prompt(sv, run);
        }
        // A recorded dialog (also one adopted from the previous supervisor)
        // is rechecked without waiting again, so an answer clears it soon.
        if (self.prompt_hash.is_none() && started.elapsed() < wait)
            || self
                .prompt_checked
                .is_some_and(|at| at.elapsed() < wait.min(PROMPT_CHECK_INTERVAL))
        {
            return Ok(());
        }
        self.prompt_checked = Some(Instant::now());
        let screen = match sv.cmux.capture(&self.workspace) {
            Ok(screen) => screen,
            Err(error) => {
                sv.log.note(&format!(
                    "screen of {} could not be read for a dialog: {error:#}",
                    run.id()
                ));
                return Ok(());
            }
        };
        match detect_prompt(&screen) {
            Some(kind) => {
                let excerpt = screen_tail(&screen, PROMPT_EXCERPT_LINES);
                let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
                if self.prompt_hash.as_deref() != Some(hash.as_str()) {
                    sv.queue.record_runtime_event(
                        run.id(),
                        "prompt_waiting",
                        json!({
                            "workspace_id": self.workspace,
                            "excerpt": excerpt,
                            "screen_hash": hash,
                            "prompt": kind.as_str(),
                        }),
                    )?;
                    sv.log.note(&format!(
                        "run {} waits at a {} dialog in workspace {}; asking the inbox",
                        run.id(),
                        kind.as_str(),
                        self.workspace
                    ));
                    // A changed screen under an open ask keeps that ask (the
                    // open ask of the run is returned, and nobody is notified
                    // again), so a ticking line cannot flood the inbox.
                    self.prompt_hash = Some(hash);
                    ask_answer_prompt(sv, run, &self.workspace, kind.as_str(), &excerpt)?;
                }
            }
            None => self.clear_prompt(sv, run)?,
        }
        Ok(())
    }

    /// Type the answer of each answered `worker_question` of the run into
    /// the worker's terminal, prefixed `answer to ask <id>:`, once the worker
    /// went idle after asking (its idle marker is no older than the ask, to
    /// the second), then close the ask and record `ask_delivered` (ADR-0022
    /// decision 2). Each answer is sent at most once: a failed send records
    /// `ask_delivery_failed` and leaves the ask unclosed for the inbox.
    fn deliver_answers(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let answers = sv.queue.undelivered_answers(run.id())?;
        if answers.is_empty() {
            return Ok(());
        }
        // Background work does not hold an answer back: typing into the
        // prompt opens no dialog, only /exit does.
        let idle_at = match sv.files.modified(&self.idle_marker) {
            Ok(modified) => unix_seconds(modified),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let failed: Vec<i64> = sv
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "ask_delivery_failed")
            .filter_map(|e| e.payload.get("ask_id").and_then(Value::as_i64))
            .collect();
        for ask in answers {
            if failed.contains(&ask.id) || idle_at < ask.created_at {
                continue;
            }
            let text = format!(
                "answer to ask {}: {}",
                ask.id,
                ask.answer.as_deref().unwrap_or_default()
            );
            match sv.cmux.send_text(&self.workspace, &text) {
                // Sent: failing to record it must not cost the live run its
                // lease, so it is only noted (the ask then shows unclosed).
                Ok(()) => match sv.queue.ask_delivered(ask.id, &self.workspace) {
                    Ok(_) => sv.log.note(&format!(
                        "answer of ask {} sent to run {} in workspace {}",
                        ask.id,
                        run.id(),
                        self.workspace
                    )),
                    Err(error) => sv.log.note(&format!(
                        "answer of ask {} was sent to run {} but could not be recorded: {error:#}",
                        ask.id,
                        run.id()
                    )),
                },
                Err(error) => {
                    sv.queue.record_runtime_event(
                        run.id(),
                        "ask_delivery_failed",
                        json!({
                            "ask_id": ask.id,
                            "workspace_id": self.workspace,
                            "error": format!("{error:#}"),
                        }),
                    )?;
                    sv.log.note(&format!(
                        "answer of ask {} could not be sent to run {} in workspace {}: {error:#}; it is left to the inbox",
                        ask.id, run.id(), self.workspace
                    ));
                }
            }
        }
        Ok(())
    }

    /// Record `prompt_cleared` if a dialog is recorded and not cleared yet.
    fn clear_prompt(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if self.prompt_hash.take().is_some() {
            sv.queue.record_runtime_event(
                run.id(),
                "prompt_cleared",
                json!({"workspace_id": self.workspace}),
            )?;
            sv.log.note(&format!("dialog of {} is gone", run.id()));
            close_answer_prompt_asks(sv, run, PROMPT_CLEARED_CLOSED)?;
        }
        Ok(())
    }
}

/// Raise a dialog a worker's session stopped at as an `answer_prompt` ask
/// to the inbox (ADR-0024's Consequences, in place of the attention of
/// ADR-0019 decision 6): the question names the run, the workspace and the
/// kind of dialog and carries the screen's excerpt. An open ask of the run
/// is not registered twice. The runtime sends no key: the person answers
/// the dialog, and the ask closes itself once the dialog is gone.
#[allow(clippy::too_many_arguments)]
fn ask_answer_prompt(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    prompt: &str,
    excerpt: &str,
) -> Result<()> {
    let question = format!(
        "The session of run {run_id} (task {task_id}) waits at a {prompt} dialog in workspace {workspace}. Answer with the choice to send to it (or what to do instead); the dialog is answered in that workspace, and this ask closes itself once the dialog is gone.\n\nLast lines of the screen:\n{excerpt}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: Vec::new(),
            asked_by: SessionRole::Supervisor.as_str().into(),
        },
        sv.cmux,
    )?;
    sv.log.note(&format!(
        "answer_prompt ask {} for {} (notified: {})",
        outcome["id"],
        run.id(),
        outcome["notified"]
    ));
    Ok(())
}

/// Close the run's `answer_prompt` asks nobody closed, noting each.
fn close_answer_prompt_asks(sv: &mut Supervisor<'_>, run: &TaskRun, answer: &str) -> Result<()> {
    for ask in sv.queue.close_answer_prompt_asks(run.id(), answer)? {
        sv.log.note(&format!(
            "closed the answer_prompt ask {} of {}: {answer}",
            ask.id,
            run.id()
        ));
    }
    Ok(())
}

/// The answers the runtime writes into an open `answer_prompt` ask it closes.
const PROMPT_CLEARED_CLOSED: &str = "the dialog is gone; closed by the runtime";
const PROMPT_RECEIPT_CLOSED: &str = "the receipt arrived; closed by the runtime";
const PROMPT_EXITED_CLOSED: &str = "the session exited; closed by the runtime";

/// Raise a session that held `/exit` back as a `stuck_exit` ask to the
/// inbox, with the last lines of its screen, through the ask path that
/// notifies once when the ask is new (ADR-0022 decision 5). An open ask of
/// the run is not registered twice. A screen that cannot be read leaves the
/// ask without an excerpt. `after` says where the run stands and what
/// follows once the session exits: a `running` run goes on to validating,
/// one the supervisor holds after its review (ADR-0027) to its landing, its
/// ask or its rest. The inbox shows the ask to the person, who acts on the
/// answer through it (the `dagq-recover` skill).
fn ask_stuck_exit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    after: &str,
) -> Result<()> {
    let screen = match sv.cmux.capture(workspace) {
        Ok(screen) => screen_tail(&screen, PROMPT_EXCERPT_LINES),
        Err(error) => format!("(the screen could not be read: {error:#})"),
    };
    let question = format!(
        "The session of run {run_id} (task {task_id}) did not exit within {timeout}s of the supervisor's /exit (exit_request_timed_out): something on its screen, usually one of Claude Code's own dialogs such as \"Background work is running\", holds the exit back. {after}; this ask then closes itself. Answer `exit` to have the dialog answered so that the session exits and /exit sent in workspace {workspace}, or `wait` to leave the session as it is (or write what to do instead).\n\nLast lines of the screen:\n{screen}",
        run_id = run.id(),
        task_id = run.task_id(),
        timeout = sv.cmux.exit_timeout().as_secs(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::StuckExit,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: vec!["exit".into(), "wait".into()],
            asked_by: SessionRole::Supervisor.as_str().into(),
        },
        sv.cmux,
    )?;
    sv.log.note(&format!(
        "stuck_exit ask {} for {} (notified: {})",
        outcome["id"],
        run.id(),
        outcome["notified"]
    ));
    Ok(())
}

/// How a registered wrapper that has not recorded its exit stands. Its
/// heartbeat is the supervisor's sign of life, but a wrapper whose
/// heartbeat stopped while its process lives on (a heartbeat that fails
/// against the queue, a stall) still holds a live session: waiting for
/// its exit alone left such sessions running for hours.
enum WrapperPulse {
    Fresh,
    /// The heartbeat expired while the wrapper's process is alive: the
    /// session is asked to `/exit` the way a finished one is, and a
    /// `stuck_exit` ask follows when it does not.
    Silent,
    /// The wrapper recorded its exit after this poll read its row: the next
    /// poll handles the exit.
    Exited,
}

/// `Silent` also records `wrapper_heartbeat_expired` once per watch (`noted`)
/// and logs it. A wrapper whose heartbeat expired and whose process is gone
/// is an error with `message`, as before: nothing is left to ask to exit,
/// and the run is given up to `recover`; a `stuck_exit` ask the silence
/// raised is closed then, since no session is left to exit. The row is read
/// again first, so a wrapper that recorded its exit just before it died is
/// `Exited`, not an error.
fn wrapper_pulse(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    noted: &mut bool,
    message: &str,
) -> Result<WrapperPulse> {
    let age = sv.generators.clock.now() - wrapper.heartbeat_at;
    if age <= HEARTBEAT_TIMEOUT_SECS {
        return Ok(WrapperPulse::Fresh);
    }
    if !sv.processes.alive(wrapper.pid) {
        let exited = sv
            .queue
            .processes(run.id())?
            .iter()
            .any(|p| p.role == "wrapper" && p.pid == wrapper.pid && p.exited_at.is_some());
        if exited {
            return Ok(WrapperPulse::Exited);
        }
        if *noted {
            for ask in sv
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                sv.log.note(&format!(
                    "wrapper of {} died without recording its exit; closed its stuck_exit ask {}",
                    run.id(),
                    ask.id
                ));
            }
        }
        bail!("{message}");
    }
    if !*noted {
        sv.queue.record_runtime_event(
            run.id(),
            "wrapper_heartbeat_expired",
            json!({"pid": wrapper.pid, "heartbeat_age_secs": age, "workspace_id": workspace}),
        )?;
        sv.log.note(&format!(
            "wrapper of {} (pid {}) stopped heartbeating {age}s ago but its process is alive; asking its session in workspace {workspace} to exit",
            run.id(),
            wrapper.pid
        ));
        *noted = true;
    }
    Ok(WrapperPulse::Silent)
}

/// What a `stuck_exit` ask says first when the `/exit` was sent because the
/// wrapper went silent, not because the session finished (a silence that
/// began after the `/exit` does not change why it was sent).
const SILENT_WRAPPER_EXIT: &str = "Its wrapper stopped heartbeating while its process lived on (wrapper_heartbeat_expired), so the supervisor sent the /exit";

/// `after` for a `stuck_exit` ask, led by [`SILENT_WRAPPER_EXIT`] when the
/// `/exit` was sent because the wrapper went silent.
fn stuck_exit_after(silent: bool, after: &str) -> String {
    if silent {
        format!("{SILENT_WRAPPER_EXIT}. {after}")
    } else {
        after.to_owned()
    }
}

/// The options of the `decide` ask of a run whose resumes are used up: a
/// subset of [`TRIAGE_OPTIONS`], applied the same way.
const EXHAUSTED_OPTIONS: &[&str] = &["retry", "cancel"];

/// The answer the runtime writes into an open `stuck_exit` ask it closes.
const STUCK_EXIT_CLOSED: &str = "the session exited; closed by the runtime";

/// A session's screen is read for a dialog at most this often.
const PROMPT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// `prompt_waiting` carries this many last non-empty lines of the screen.
const PROMPT_EXCERPT_LINES: usize = 15;
/// Only this many last non-empty lines are searched for a dialog: a dialog
/// sits at the bottom, and text higher up is usually the work itself.
const PROMPT_SCAN_LINES: usize = 30;

/// Another numbered option counts within this many lines of the `❯` one.
const OPTION_REACH: usize = 3;

/// Which dialog of the agent's TUI holds the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The folder trust question.
    Trust,
    /// A `❯`-marked choice among numbered options (a plugin
    /// recommendation, the auto mode notice, ...).
    Choice,
    /// A footer such as `Enter to confirm · Esc to cancel` alone.
    Confirm,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trust => "trust",
            Self::Choice => "choice",
            Self::Confirm => "confirm",
        }
    }
}

/// Whether the bottom of a screen shows a dialog: a line starting with `Do
/// you trust` or an option offering to trust the folder, a line starting
/// with `❯` and a numbered option within three lines of another numbered
/// option, or a
/// line starting with `Enter to confirm` or `Esc to cancel`. Box borders are
/// ignored, and a phrase inside other text (a quote, code) does not count.
pub fn detect_prompt(screen: &str) -> Option<PromptKind> {
    let lines: Vec<&str> = screen
        .lines()
        .map(strip_frame)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..];
    if tail.iter().any(|line| {
        line.starts_with("Do you trust")
            || option_text(line).is_some_and(|text| text.contains("trust this folder"))
    }) {
        return Some(PromptKind::Trust);
    }
    // An option's text can wrap, so another option may be a few lines away.
    let is_option = |i: usize| tail.get(i).is_some_and(|line| option_text(line).is_some());
    let near = |i: usize| {
        (i.saturating_sub(OPTION_REACH)..=i + OPTION_REACH).any(|j| j != i && is_option(j))
    };
    if (0..tail.len()).any(|i| tail[i].starts_with('❯') && is_option(i) && near(i)) {
        return Some(PromptKind::Choice);
    }
    tail.iter()
        .any(|line| line.starts_with("Enter to confirm") || line.starts_with("Esc to cancel"))
        .then_some(PromptKind::Confirm)
}

fn strip_frame(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '|'))
}

/// The text of a numbered option line (`1. Yes`, `❯ 2. No`), if it is one.
fn option_text(line: &str) -> Option<&str> {
    let line = line.strip_prefix('❯').unwrap_or(line).trim_start();
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = line[digits..].strip_prefix(". ")?;
    (digits > 0).then_some(rest)
}

/// The last `count` non-empty lines of a screen, right-trimmed.
fn screen_tail(screen: &str, count: usize) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// What the resolution request tells a resumed session.
struct ResumeRequest {
    /// The `main` head the session rebases onto.
    main: CommitSha,
    reason: String,
    kind: ResumeKind,
}

/// Why the run waits for a session, which decides the request's steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeKind {
    /// A landing was deferred (a conflict, failed verification): rebase.
    Landing,
    /// Validation's `evidence_missing`: add the evidence instead.
    EvidenceMissing,
    /// A person sent a review's concern back (`landing_decided`): fix
    /// the findings.
    SentBack,
    /// The diff changes paths outside the task's `paths` (validation's
    /// `scope_violation`, or a landing deferred for it): take them out.
    ScopeViolation,
    /// A passed run's live session, before its `/exit`: the precheck found
    /// that it conflicts with main (ADR-0027 decision 4). Rebase, like
    /// `Landing`.
    Precheck,
    /// The triage of a `failed` / `interrupted` run sent it back to its
    /// session (`triage_finished` with action `resume`, or a person's
    /// `resume` answer, `triage_decided`): do what the reason asks.
    Triage,
}

/// Why the run waits for a session: the reason of its latest
/// `integration_deferred` / `integration_error` / `evidence_missing` /
/// `scope_violation` / `landing_decided` event (a runtime error since, such
/// as a failed resume, may have replaced `last_error`), else `last_error`;
/// and what kind of request that makes: `evidence_missing` (or a landing
/// deferred for missing evidence, whose payload names the `checks`),
/// `scope_violation` (or a landing deferred for it, whose payload names the
/// paths), a review sent back, the triage's resume (`triage_finished`,
/// whose `instruction` is the reason, or a person's `triage_decided`), or a
/// landing.
fn resume_reason(queue: &dyn Queue, run: &TaskRun) -> Result<(Option<String>, ResumeKind)> {
    let events = queue.run_events(run.id())?;
    let parked = events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            "integration_deferred"
                | "integration_error"
                | "evidence_missing"
                | "scope_violation"
                | "landing_decided"
                | "triage_finished"
                | "triage_decided"
        )
    });
    // The triage's resume asks for its `instruction`, not its reason.
    let key = match parked {
        Some(e) if e.kind == "triage_finished" => "instruction",
        _ => "reason",
    };
    let reason = parked
        .and_then(|e| e.payload.get(key).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| run.last_error().map(str::to_owned));
    let kind = match parked {
        Some(e) if e.kind == "evidence_missing" || e.payload.get("checks").is_some() => {
            ResumeKind::EvidenceMissing
        }
        Some(e) if e.kind == "scope_violation" || e.payload.get("scope_violation").is_some() => {
            ResumeKind::ScopeViolation
        }
        Some(e) if e.kind == "landing_decided" => ResumeKind::SentBack,
        Some(e) if e.kind.starts_with("triage_") => ResumeKind::Triage,
        _ => ResumeKind::Landing,
    };
    Ok((reason, kind))
}

/// The tasks landed on `main` since the run's base, oldest first, from the
/// `Dagq-Task` trailers, each with its integrated run's receipt summary.
fn landed_since(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    run: &TaskRun,
    main: &CommitSha,
) -> Result<Vec<PredecessorSummary>> {
    let mut landed = Vec::new();
    for task_id in repository.landed_task_ids(run.base_commit().as_str(), main.as_str())? {
        let Ok(detail) = queue.show(task_id) else {
            continue;
        };
        let integrated_run = detail
            .runs
            .iter()
            .rev()
            .find(|r| r.status() == RunStatus::Integrated)
            .cloned();
        landed.push(PredecessorSummary::from_predecessor(&Predecessor {
            task: detail.task,
            integrated_run,
        }));
    }
    Ok(landed)
}

/// The fixed resolution request the supervisor types into a resumed
/// session (ADR-0019 decision 1), or into a passed run's live session whose
/// head conflicts with main (ADR-0027 decision 4), one instruction per
/// line; the backend sends it as one line.
fn resume_request(
    task: &Task,
    run: &TaskRun,
    request: &ResumeRequest,
    landed: &[PredecessorSummary],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let mut lines = vec![match request.kind {
        ResumeKind::EvidenceMissing => format!(
            "dagq: the supervisor's validation of run {} (task {}) found required evidence missing from the receipt, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::SentBack => format!(
            "dagq: the supervisor's review of run {} (task {}) raised findings a person sent back to you, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::ScopeViolation => format!(
            "dagq: run {} (task {}) changes paths outside the task's --paths ({}), so the run is needs_session.",
            run.id(),
            task.id(),
            task.paths().join(", ")
        ),
        ResumeKind::Landing => format!(
            "dagq: integrate could not land run {} (task {}) and returned needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::Precheck => format!(
            "dagq: the supervisor's review of run {} (task {}) passed, but integrate would conflict with main, so the run was not landed.",
            run.id(),
            task.id()
        ),
        ResumeKind::Triage => format!(
            "dagq: run {} (task {}) failed or was interrupted, and the supervisor's triage sent it back to this session to finish, so the run is needs_session.",
            run.id(),
            task.id()
        ),
    }];
    lines.push(format!("Reason: {}", request.reason));
    lines.push(format!(
        "main is now {} (your base commit was {}).",
        request.main,
        run.base_commit()
    ));
    if landed.is_empty() {
        lines.push("Tasks landed on main since your base: none.".to_owned());
    } else {
        lines.push("Tasks landed on main since your base:".to_owned());
        for task in landed {
            lines.push(format!(
                "- task {}: {}; summary: {}",
                task.task_id, task.title, task.summary
            ));
        }
    }
    lines.push("Steps:".to_owned());
    let verify = serde_json::to_string(task.verification_commands())?;
    if request.kind == ResumeKind::EvidenceMissing {
        lines.push(
            "1. Run the checks the reason names as missing and write their evidence into the receipt."
                .to_owned(),
        );
        lines.push(format!(
            "2. If that changes files, commit them and rerun the verification commands {verify}."
        ));
    } else if request.kind == ResumeKind::ScopeViolation {
        lines.push(format!(
            "1. Take the changes to the paths the reason names out of the run branch: restore each to its state at git merge-base HEAD {} (delete the ones that did not exist there) and commit; if the task cannot be done without them, write the receipt with result failed and say which paths it needs.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else if request.kind == ResumeKind::SentBack {
        lines.push(format!(
            "1. Fix the findings in the reason and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else if request.kind == ResumeKind::Triage {
        lines.push(format!(
            "1. Do what the reason asks in this worktree and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else {
        lines.push(format!(
            "1. In this worktree run git rebase {} and resolve the conflicts.",
            request.main
        ));
        lines.push(format!(
            "2. Rerun the verification commands {verify} and commit the result."
        ));
    }
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {STOP_BACKGROUND}"));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(
        "6. If the change is no longer needed, write the receipt with result failed and the reason in summary."
            .to_owned(),
    );
    lines.push(
        "7. Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// Watches one resumed session: its wrapper registration, the single
/// resolution request once its agent is up, the rewritten receipt and the
/// idle marker, the single `/exit`, and the wrapper's exit.
struct ResumeWatch {
    workspace: String,
    attempt: usize,
    run_dir: PathBuf,
    receipt_path: PathBuf,
    idle_marker: PathBuf,
    /// A receipt no newer than this is the one from before the resume.
    /// Like every time compared with a file's mtime (`sent_at` of a revise
    /// or conflict request, `message_sent`), it is read from the wall clock
    /// that stamps the files, not from the injected [`Clock`].
    started_at: SystemTime,
    startup: Instant,
    message: String,
    agent_seen: Option<Instant>,
    /// When the resolution request was sent (for its timeout, and for the
    /// idle marker of the response to it).
    message_sent: Option<(Instant, SystemTime)>,
    exit_requested: Option<Instant>,
    /// The task's required checks: a rewritten receipt still without them
    /// has not resolved the run.
    required_evidence: Vec<EvidenceCheck>,
    /// Its integrate was called: resolved, it exits and lands without a
    /// review; otherwise it stays open for validation and review.
    approved: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    silent: bool,
    /// The `/exit` was sent because of that silence.
    exit_for_silence: bool,
}

/// What a resumed session left behind when it exited.
enum ResumeOutcome {
    /// A rewritten `succeeded` receipt names the worktree head.
    Resolved,
    /// A rewritten receipt reports `failed`; the reason for `last_error`.
    Failed(String),
    /// Anything else: no rewritten receipt, or one for another commit.
    Unresolved,
}

struct ResumeVerdict {
    kind: ResumeOutcome,
    head: Option<CommitSha>,
    /// The session did not exit within the exit timeout of `/exit`: it is
    /// let go (still running, its workspace kept) so the slot and the lease
    /// are not held forever.
    exit_timed_out: bool,
    /// The session resolved the run and is still running, never asked to
    /// exit: it goes on to validation and review (ADR-0027 decision 3).
    live: bool,
}

impl ResumeVerdict {
    fn outcome(&self) -> &'static str {
        match self.kind {
            ResumeOutcome::Resolved => "resolved",
            ResumeOutcome::Failed(_) => "failed",
            ResumeOutcome::Unresolved => "unresolved",
        }
    }
}

impl ResumeWatch {
    /// The receipt the session rewrote during this resume, if any.
    fn rewritten_receipt(&self, files: &dyn RunFiles) -> Option<Receipt> {
        let modified = files.modified(&self.receipt_path).ok()?;
        if modified <= self.started_at {
            return None;
        }
        Receipt::parse(&files.read_to_string(&self.receipt_path).ok()?).ok()
    }

    /// `head` is the worktree's HEAD when the worktree is clean, `None`
    /// otherwise: a resolved receipt must name a clean head.
    fn verdict(
        &self,
        files: &dyn RunFiles,
        run: &TaskRun,
        head: Option<&CommitSha>,
    ) -> ResumeOutcome {
        match self.rewritten_receipt(files) {
            Some(receipt) if receipt.run_id != *run.id().as_str() => ResumeOutcome::Unresolved,
            Some(receipt) if receipt.result == ReceiptResult::Failed => ResumeOutcome::Failed(
                format!("session reported the run as failed: {}", receipt.summary),
            ),
            Some(receipt)
                if head
                    .is_some_and(|head| head.as_str() == receipt.commit.to_ascii_lowercase())
                    && receipt.missing_evidence(&self.required_evidence).is_empty() =>
            {
                ResumeOutcome::Resolved
            }
            _ => ResumeOutcome::Unresolved,
        }
    }

    /// One observation; `Some` once the wrapper exited.
    fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<ResumeVerdict>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "resumed session's wrapper did not register within {} seconds",
                timeout.as_secs()
            );
            return Ok(None);
        };
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        if wrapper.exited_at.is_some() {
            match sv.cmux.capture(&self.workspace) {
                Ok(screen) => sv.files.write(
                    &self
                        .run_dir
                        .join(format!("terminal-resume-{}.txt", self.attempt)),
                    screen.as_bytes(),
                )?,
                Err(error) => sv.queue.record_runtime_event(
                    run.id(),
                    "screen_capture_failed",
                    json!({"error": format!("{error:#}")}),
                )?,
            }
            let head = sv.repository.head(worktree).ok();
            let clean = sv
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty());
            return Ok(Some(ResumeVerdict {
                kind: self.verdict(&*sv.files, run, head.as_ref().filter(|_| clean)),
                head,
                exit_timed_out: false,
                live: false,
            }));
        }
        let pulse = wrapper_pulse(
            sv,
            run,
            wrapper,
            &self.workspace,
            &mut self.silent,
            "resumed session's wrapper heartbeat expired; session may still be alive",
        )?;
        if matches!(pulse, WrapperPulse::Exited) {
            return Ok(None);
        }
        if matches!(pulse, WrapperPulse::Silent) && self.exit_requested.is_none() {
            // Ask once, the way a person would; never kill the session.
            sv.cmux.send_exit(&self.workspace)?;
            sv.log.note(&format!(
                "resumed session of {} lost its wrapper heartbeat; exit requested",
                run.id()
            ));
            self.exit_requested = Some(Instant::now());
            self.exit_for_silence = true;
        }
        if let Some(requested) = self.exit_requested {
            if requested.elapsed() >= sv.cmux.exit_timeout() {
                // /exit is not resent (it could pick a dialog's option).
                sv.log.note(&format!(
                    "resumed session of {} did not exit within {}s of the exit request; letting it go as unresolved (its workspace {} is kept)",
                    run.id(),
                    sv.cmux.exit_timeout().as_secs(),
                    self.workspace
                ));
                // Its dialog stays until someone answers it: raise it to
                // the inbox, as for the worker's session (task 104). The
                // next pass closes the ask once the session ended. A failed
                // ask is only noted: the verdict stands without it.
                let after = stuck_exit_after(
                    self.exit_for_silence,
                    if self.attempt >= MAX_RESUME_ATTEMPTS {
                        "The run stays needs_session after its last resume attempt, and is left to the person once the session exits"
                    } else {
                        "The run stays needs_session, and the supervisor resumes it again once the session exits"
                    },
                );
                if let Err(error) = ask_stuck_exit(sv, run, &self.workspace, &after) {
                    sv.log.note(&format!(
                        "stuck_exit ask for {} could not be opened: {error:#}",
                        run.id()
                    ));
                }
                return Ok(Some(ResumeVerdict {
                    kind: ResumeOutcome::Unresolved,
                    head: sv.repository.head(worktree).ok(),
                    exit_timed_out: true,
                    live: false,
                }));
            }
            return Ok(None);
        }
        let Some((sent, sent_at)) = self.message_sent else {
            if processes.iter().any(|p| p.role == "agent") {
                let seen = *self.agent_seen.get_or_insert_with(Instant::now);
                if seen.elapsed() >= sv.cmux.resume_prompt_delay() {
                    sv.cmux.send_text(&self.workspace, &self.message)?;
                    self.message_sent = Some((Instant::now(), sv.files.now()));
                    sv.log.note(&format!(
                        "resolution request sent to run {} in workspace {}",
                        run.id(),
                        self.workspace
                    ));
                }
            }
            return Ok(None);
        };
        // The idle marker is read before the receipt and the
        // worktree: a receipt rewritten after this read is judged
        // at the next poll, never as idle without it.
        let idle = IdleMarker::read(&*sv.files, &self.idle_marker)?;
        let head = sv.repository.head(worktree)?;
        let clean = sv.repository.status(worktree)?.trim().is_empty();
        // Resolved (or failed) and idle after the receipt; or idle
        // after the request with no such receipt, which a session
        // that could not resolve it (or stopped at a question)
        // never ends by itself; or no idle at all within the
        // resume timeout (a lost request, a dialog, background
        // work that does not end).
        let verdict = self.verdict(&*sv.files, run, clean.then_some(&head));
        let idle_after_receipt = match (&idle, &verdict) {
            (Some(idle), ResumeOutcome::Resolved | ResumeOutcome::Failed(_)) => idle
                .idle_after_receipt(&*sv.files, &self.receipt_path)?
                .is_some(),
            _ => false,
        };
        // An unapproved resolved run keeps its session for
        // validation and review (ADR-0027 decision 3).
        if matches!(verdict, ResumeOutcome::Resolved) && !self.approved && idle_after_receipt {
            sv.log.note(&format!(
                "resumed session of {} rewrote its receipt and went idle (head {head}); validating with the session open",
                run.id()
            ));
            return Ok(Some(ResumeVerdict {
                kind: ResumeOutcome::Resolved,
                head: Some(head),
                exit_timed_out: false,
                live: true,
            }));
        }
        let why = match verdict {
            ResumeOutcome::Unresolved if idle.is_some_and(|idle| idle.idle_since(sent_at)) => {
                Some("went idle without a resolving receipt")
            }
            ResumeOutcome::Unresolved => None,
            _ => idle_after_receipt.then_some("rewrote its receipt and went idle"),
        }
        .or_else(|| {
            (sent.elapsed() >= sv.cmux.resume_timeout())
                .then_some("did not finish within the resume timeout")
        });
        if let Some(why) = why {
            // Ask once, the way a person would; never kill the session.
            sv.cmux.send_exit(&self.workspace)?;
            sv.log.note(&format!(
                "resumed session of {} {why} (head {head}); exit requested",
                run.id()
            ));
            self.exit_requested = Some(Instant::now());
        }
        Ok(None)
    }
}

/// The verdict of the last `review_finished` before event `before`: the
/// pass a conflict precheck followed.
fn passed_before(events: &[crate::domain::RunEvent], before: i64) -> Option<ReviewVerdict> {
    events
        .iter()
        .rev()
        .find(|e| e.id < before && e.kind == "review_finished")
        .and_then(|e| {
            serde_json::from_value(json!({
                "verdict": e.payload["verdict"],
                "reasons": e.payload["reasons"],
                "summary": e.payload["summary"],
            }))
            .ok()
        })
}

/// Kill the headless job (a review or a triage) of a slot the supervisor
/// stops watching.
fn stop_job(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Review(watch) => watch.job.stop(),
        Phase::Triage(watch) => watch.job.stop(),
        _ => {}
    }
}

/// Whether the run's session wrapper is registered and has not exited.
fn session_alive(queue: &dyn Queue, run_id: &RunId) -> Result<bool> {
    Ok(queue
        .processes(run_id)?
        .iter()
        .any(|p| p.role == "wrapper" && p.exited_at.is_none()))
}

/// The `reasons` of the run's latest `review_finished`.
fn latest_review_reasons(queue: &dyn Queue, run_id: &RunId) -> Result<Vec<String>> {
    Ok(queue
        .run_events(run_id)?
        .into_iter()
        .rev()
        .find(|e| e.kind == "review_finished")
        .and_then(|e| serde_json::from_value(e.payload["reasons"].clone()).ok())
        .unwrap_or_default())
}

/// A headless job's process (a review or a triage) whose stdout and stderr
/// go to files, waited for at most `timeout`.
struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `triage`.
    what: &'static str,
    child: Box<dyn Spawned>,
    started: Instant,
    timeout: Duration,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl HeadlessJob {
    /// `Some` once the job ended: its stdout, or why it failed (a non-zero
    /// exit, or the timeout, after which the process is killed).
    fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<String, String>>> {
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None if self.started.elapsed() < self.timeout => return Ok(None),
            None => {
                self.stop();
                return Ok(Some(Err(format!(
                    "the headless {} did not finish within {} seconds",
                    self.what,
                    self.timeout.as_secs()
                ))));
            }
        };
        if !status.success {
            let stderr = files.read_to_string(&self.stderr).unwrap_or_default();
            return Ok(Some(Err(format!(
                "the headless {} exited with {status}: {}",
                self.what,
                or_none(tail(stderr.trim(), 500))
            ))));
        }
        Ok(Some(Ok(files
            .read_to_string(&self.stdout)
            .unwrap_or_default())))
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The headless review in progress, with its output in `review-N.out` /
/// `review-N.err` in the run directory.
struct ReviewWatch {
    session: Option<SessionRef>,
    attempt: usize,
    job: HeadlessJob,
}

impl ReviewWatch {
    /// `Some` once the review ended: its verdict, or why it failed (the
    /// job's failure, or stdout without a verdict).
    fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<ReviewVerdict, String>>> {
        Ok(self
            .job
            .poll(files)?
            .map(|output| output.and_then(|stdout| ReviewVerdict::parse(&stdout))))
    }
}

/// The headless triage in progress, with its output in `triage-N.out` /
/// `triage-N.err` next to the run.
struct TriageWatch {
    attempt: usize,
    job: HeadlessJob,
}

impl TriageWatch {
    fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<TriageVerdict, String>>> {
        Ok(self
            .job
            .poll(files)?
            .map(|output| output.and_then(|stdout| TriageVerdict::parse(&stdout))))
    }
}

/// A `revise` verdict, or a conflict the precheck found, sent to the live
/// session: it is waited for until the session rewrites its receipt and
/// goes idle.
struct ReviseWatch {
    session: SessionRef,
    attempt: usize,
    fix: Fix,
    /// A receipt or idle marker no newer than this predates the request.
    sent_at: SystemTime,
    sent: Instant,
}

/// What the live session was asked to fix (ADR-0027 decisions 2 and 4).
enum Fix {
    /// A `revise` verdict's findings.
    Revise(Vec<String>),
    /// A conflict with main found after a `pass`; the passed verdict, for
    /// the ask if the session does not resolve it.
    Conflict(ReviewVerdict),
}

impl Fix {
    /// How the request is named in logs and texts: `revise N` or
    /// `conflict request N`.
    fn label(&self, attempt: usize) -> String {
        match self {
            Fix::Revise(_) => format!("revise {attempt}"),
            Fix::Conflict(_) => format!("conflict request {attempt}"),
        }
    }

    /// The `approve_landing` ask when the session cannot fix it: a revise
    /// asks with `summary`; a conflict asks with its passed verdict.
    fn ask(&self, summary: String, why: String) -> AfterExit {
        match self {
            Fix::Revise(reasons) => AfterExit::Ask {
                decision: ReviewDecision::Revise,
                reasons: reasons.clone(),
                summary,
                why: Some(why),
            },
            Fix::Conflict(verdict) => AfterExit::Ask {
                decision: verdict.verdict,
                reasons: verdict.reasons.clone(),
                summary: verdict.summary.clone(),
                why: Some(why),
            },
        }
    }
}

enum ReviseOutcome {
    /// The receipt was rewritten after the request, names the clean
    /// worktree HEAD (or reports `failed`), and the session went idle after
    /// it; the worktree HEAD at that time. Validation judges the receipt.
    Rewritten(CommitSha),
    /// The session rewrote the receipt and went idle, but the receipt does
    /// not name the clean worktree HEAD (an old commit, a commit after the
    /// receipt, uncommitted changes) or cannot be read: validation would
    /// fail the run and its work, so the session is asked to fix it.
    Mismatch(String),
    /// The session will not rewrite it: why.
    Ended(String),
}

impl ReviseWatch {
    fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<ReviseOutcome>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes
            .iter()
            .find(|p| p.role == "wrapper" && p.exited_at.is_none())
        else {
            return Ok(Some(ReviseOutcome::Ended(
                "ended before it rewrote the receipt".to_owned(),
            )));
        };
        if sv.generators.clock.now() - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS {
            ensure!(
                sv.processes.alive(wrapper.pid),
                "wrapper heartbeat expired; session may still be alive"
            );
            // The exit that follows records `wrapper_heartbeat_expired` and
            // sends the /exit.
            return Ok(Some(ReviseOutcome::Ended(
                "went silent (its wrapper stopped heartbeating while its process lives on)"
                    .to_owned(),
            )));
        }
        let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
        // The idle marker is read before the receipt: a receipt rewritten
        // after this read is judged at the next poll, never as idle without
        // it.
        let idle = IdleMarker::read(&*sv.files, &run.idle_marker_path()?)?;
        let rewritten = sv
            .files
            .modified(receipt)
            .is_ok_and(|modified| modified > self.sent_at);
        let idle_after_receipt = match &idle {
            Some(idle) if rewritten => idle.idle_after_receipt(&*sv.files, receipt)?.is_some(),
            _ => false,
        };
        if idle_after_receipt {
            let worktree = Path::new(run.worktree_path().context("missing worktree")?);
            let head = sv.repository.head(worktree)?;
            let clean = sv.repository.status(worktree)?.trim().is_empty();
            let parsed = sv
                .files
                .read_to_string(receipt)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(Receipt::parse(&text)?));
            return Ok(Some(match parsed {
                // A session that gives the change up is validation's to fail.
                Ok(receipt) if receipt.result == ReceiptResult::Failed => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if receipt.commit.to_ascii_lowercase() == head.as_str() && clean => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if receipt.commit.to_ascii_lowercase() != head.as_str() => {
                    ReviseOutcome::Mismatch(format!(
                        "the rewritten receipt names commit {} but the worktree HEAD is {head}",
                        receipt.commit
                    ))
                }
                Ok(_) => ReviseOutcome::Mismatch(format!(
                    "the worktree has uncommitted changes on top of HEAD {head}"
                )),
                Err(error) => {
                    ReviseOutcome::Mismatch(format!("the rewritten receipt is invalid: {error:#}"))
                }
            }));
        }
        if !rewritten && idle.is_some_and(|idle| idle.idle_since(self.sent_at)) {
            return Ok(Some(ReviseOutcome::Ended(
                "went idle without rewriting the receipt".to_owned(),
            )));
        }
        if self.sent.elapsed() >= sv.cmux.resume_timeout() {
            return Ok(Some(ReviseOutcome::Ended(format!(
                "did not rewrite the receipt within {} seconds",
                sv.cmux.resume_timeout().as_secs()
            ))));
        }
        Ok(None)
    }
}

/// Asks the run's session to `/exit` once (unless it ended already) and
/// waits for its wrapper to exit; then the supervisor closes the workspace
/// and does `then`. The `/exit` waits while the idle marker shows background
/// work running (task 147), for at most the resume timeout. A session that
/// holds the `/exit` back past the exit timeout is recorded as
/// `exit_request_timed_out` and waited for, keeping the lease, as before
/// (ADR-0027 leaves it unchanged).
struct ExitWatch {
    session: Option<SessionRef>,
    /// When the watch began, for the wait on background work.
    since: Instant,
    /// The wait on background work is logged.
    background_noted: bool,
    requested: Option<Instant>,
    timed_out: bool,
    /// The `stuck_exit` ask of the exit timeout is registered (also by a
    /// previous supervisor), as for a running run's session (task 104).
    exit_asked: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    silent: bool,
    /// The `/exit` was sent because of that silence.
    exit_for_silence: bool,
    then: AfterExit,
}

impl ExitWatch {
    fn new(session: Option<SessionRef>, then: AfterExit) -> Self {
        Self {
            session,
            since: Instant::now(),
            background_noted: false,
            requested: None,
            timed_out: false,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
            then,
        }
    }

    /// Where the run stands while its session holds the `/exit` back, and
    /// what follows once it exits: the `stuck_exit` question's sentence.
    fn after(&self, run: &TaskRun) -> String {
        let next = match &self.then {
            AfterExit::Land => "lands on main",
            AfterExit::Ask { .. } => "opens an approve_landing ask for the person",
            AfterExit::ReviewFailed { .. } => "waits for a review by hand",
            AfterExit::Rest { close: true } => "is resumed in a session of its own",
            AfterExit::Rest { close: false } => "is left to the person",
        };
        stuck_exit_after(
            self.exit_for_silence,
            &format!(
                "The run stays {} under the supervisor after its validation and review, and {next} once the session exits",
                run.status().as_str()
            ),
        )
    }

    /// Whether the session is gone (or there was none). A session that
    /// exited has its `stuck_exit` asks closed.
    fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        let Some(session) = &self.session else {
            return Ok(true);
        };
        let processes = sv.queue.processes(run.id())?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        let Some(wrapper) = wrapper.filter(|w| w.exited_at.is_none()) else {
            if self.requested.is_some() {
                let name = match session.resume {
                    Some(attempt) => format!("terminal-resume-{attempt}.txt"),
                    None => "terminal-final.txt".to_owned(),
                };
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                match sv.cmux.capture(&session.workspace) {
                    Ok(screen) => sv.files.write(&run_dir.join(name), screen.as_bytes())?,
                    Err(error) => sv.queue.record_runtime_event(
                        run.id(),
                        "screen_capture_failed",
                        json!({"error": format!("{error:#}")}),
                    )?,
                }
            }
            // Nobody needs to send /exit to a session that exited.
            for ask in sv
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                sv.log.note(&format!(
                    "session of {} exited; closed its stuck_exit ask {}",
                    run.id(),
                    ask.id
                ));
            }
            return Ok(true);
        };
        // A silent wrapper's session gets the same single /exit.
        let pulse = wrapper_pulse(
            sv,
            run,
            wrapper,
            &session.workspace,
            &mut self.silent,
            "wrapper heartbeat expired; session may still be alive",
        )?;
        if matches!(pulse, WrapperPulse::Exited) {
            return Ok(false);
        }
        match self.requested {
            None if self.since.elapsed() < sv.cmux.resume_timeout()
                && background_running(&*sv.files, &run.idle_marker_path()?)? =>
            {
                // A /exit now would stop at the "Background work is
                // running" dialog; Claude Code takes the turn up again when
                // the work ends and writes a marker without it.
                if !self.background_noted {
                    self.background_noted = true;
                    sv.log.note(&format!(
                        "session of {} has background work running; /exit waits for it",
                        run.id()
                    ));
                }
            }
            None => {
                // Recorded before sending: the session may exit, and its
                // wrapper record `session_exited`, before the send returns.
                let timeout = sv.cmux.exit_timeout();
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_requested",
                    json!({"workspace_id": session.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                // Ask once, the way a person would; never kill the session.
                sv.cmux.send_exit(&session.workspace)?;
                sv.log.note(&format!(
                    "exit requested for {}; waiting for session exit",
                    run.id()
                ));
                self.requested = Some(Instant::now());
                self.exit_for_silence = matches!(pulse, WrapperPulse::Silent);
            }
            Some(requested) if !self.timed_out && requested.elapsed() >= sv.cmux.exit_timeout() => {
                let timeout = sv.cmux.exit_timeout();
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_request_timed_out",
                    json!({"workspace_id": session.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                sv.log.note(&format!(
                    "session for {} did not exit within {}s of the exit request; keeping the run and asking the inbox to send /exit in workspace {}",
                    run.id(),
                    timeout.as_secs(),
                    session.workspace
                ));
                self.timed_out = true;
            }
            Some(_) => (),
        }
        if self.timed_out && !self.exit_asked {
            let workspace = session.workspace.clone();
            ask_stuck_exit(sv, run, &workspace, &self.after(run))?;
            self.exit_asked = true;
        }
        Ok(false)
    }
}

/// The fixed request the supervisor types into the live session when the
/// receipt it rewrote for a revise or a conflict request does not name its clean worktree HEAD.
fn revise_mismatch_request(run: &TaskRun, label: &str, why: &str) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    Ok([
        format!(
            "dagq: the receipt you rewrote for {label} of run {} cannot be accepted: {why}.",
            run.id()
        ),
        "Steps:".to_owned(),
        "1. Commit every change you meant to make, so the worktree is clean.".to_owned(),
        format!(
            "2. Rewrite the receipt at {receipt} with the current HEAD commit (git rev-parse HEAD), writing a temporary file in the same directory and renaming it."
        ),
        format!("3. {STOP_BACKGROUND}"),
        "4. Do not merge or push. When done, report briefly and stop; do not run /exit."
            .to_owned(),
    ]
    .join("\n"))
}

/// What the headless reviewer is asked (ADR-0023 decision 2, ADR-0027
/// decision 2): where the material is, the task's acceptance, the verdict
/// schema and where `revise` ends and `concern` begins.
pub fn review_prompt(task: &Task, run: &TaskRun, review_path: &str) -> String {
    format!(
        "You review run {run_id} of dagq task {task_id} ({title}) before it lands on main.\n\
         Read the review material at {review_path}: the task, its goal, the receipt, the commits and the full diff. Read the worktree if you need more. Do not change any file.\n\n\
         Acceptance criteria of the task:\n{acceptance}\n\n\
         Decide one verdict:\n\
         - pass: the diff meets the acceptance criteria and the task's instructions and nothing needs fixing.\n\
         - revise: findings the worker can fix without a person's judgment: missing tests or evidence, lint, fmt or clippy findings, a receipt that disagrees with the diff where fixing the diff settles it, or an obvious gap inside the instructed scope.\n\
         - concern: findings that need a person's judgment: a mismatch with the acceptance criteria, changes the task did not ask for, or a finding that involves a judgment call.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [string], \"summary\": string}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        acceptance = or_none(task.acceptance()),
    )
}

/// The tools the headless triage may use beyond what needs no permission:
/// reading only.
pub const TRIAGE_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// Bytes of each log, receipt and screen the triage prompt carries (their
/// ends).
const TRIAGE_TAIL_BYTES: usize = 3000;

/// Logs of a run directory the triage reads: the latest integrate
/// attempt's `integrate-<attempt>-verify-N.log` (see [`integrate_logs`]) and
/// `verify-N.log`, at most this many.
const TRIAGE_LOGS: usize = 8;

/// What the headless triage is asked (ADR-0024 decision 3): the task, the
/// run's error, receipt, verification logs, final screen and events, the
/// task's earlier runs, the verdict schema and the rule that a task with
/// [`TRIAGE_RETRY_FAILURES`] failed or interrupted runs is not retried.
/// `dir` is where the run's files are.
pub fn triage_prompt(
    files: &dyn RunFiles,
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: usize,
    dir: &Path,
) -> Result<String> {
    let task = &detail.task;
    let failures = detail
        .runs
        .iter()
        .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
        .count();
    let read = |path: &Path| {
        files
            .read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let mut material = String::new();
    let receipt = run.receipt_path().map(Path::new).and_then(read);
    material.push_str(&format!(
        "Receipt ({}):\n{}\n",
        run.receipt_path().unwrap_or("none"),
        fenced(
            "json",
            or_none(tail(
                receipt.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let (latest, earlier) = integrate_logs(dir);
    let mut logs = latest;
    // `verify-N.log` is what validation wrote before ADR-0023.
    let mut validation: Vec<PathBuf> = log_names(dir)
        .into_iter()
        .filter(|(name, _)| name.starts_with("verify-") && name.ends_with(".log"))
        .map(|(_, path)| path)
        .collect();
    validation.sort();
    logs.extend(validation);
    logs.truncate(TRIAGE_LOGS);
    if !earlier.is_empty() {
        material.push_str(&format!(
            "Logs of earlier integrate attempts (not shown): {}\n",
            earlier
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if logs.is_empty() {
        material.push_str("Verification logs: none\n");
    }
    for log in &logs {
        let text = read(log).unwrap_or_default();
        material.push_str(&format!(
            "Verification log {} (end):\n{}\n",
            log.display(),
            fenced("text", or_none(tail(text.trim(), TRIAGE_TAIL_BYTES)))
        ));
    }
    let screen = read(&dir.join("terminal-final.txt"));
    material.push_str(&format!(
        "Final screen of the session (end of terminal-final.txt):\n{}\n",
        fenced(
            "text",
            or_none(tail(
                screen.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let events: Vec<Value> = detail
        .events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(run.id()))
        .map(crate::watch::compact_event)
        .collect();
    let events = &events[events.len().saturating_sub(40)..];
    material.push_str(&format!(
        "Events of the run (the last {}):\n{}\n",
        events.len(),
        fenced(
            "json",
            &events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    ));
    let earlier: Vec<String> = detail
        .runs
        .iter()
        .filter(|r| *r.id() != *run.id())
        .map(|r| {
            let verdicts: Vec<String> = detail
                .events
                .iter()
                .filter(|e| e.run_id.as_ref() == Some(r.id()) && e.kind == "triage_finished")
                .map(|e| format!("{}", e.payload.get("action").unwrap_or(&Value::Null)))
                .collect();
            format!(
                "- run {} {}: {}{}",
                r.id(),
                r.status().as_str(),
                or_none(tail(r.last_error().unwrap_or_default(), 300)),
                if verdicts.is_empty() {
                    String::new()
                } else {
                    format!(" (triaged: {})", verdicts.join(", "))
                }
            )
        })
        .collect();
    let retry_rule = if failures >= TRIAGE_RETRY_FAILURES {
        format!(
            "This task has {failures} failed or interrupted runs, this one included: do not answer retry (the supervisor turns it into ask)."
        )
    } else {
        format!(
            "This task has {failures} failed or interrupted run(s), this one included; from {TRIAGE_RETRY_FAILURES} on, retry is not allowed and the supervisor turns it into ask."
        )
    };
    let resume_rule = if resumes >= MAX_RESUME_ATTEMPTS {
        format!("The run was resumed {resumes} times already: do not answer resume.")
    } else {
        format!(
            "The run was resumed {resumes} time(s) (at most {MAX_RESUME_ATTEMPTS}); resume needs the run's worktree."
        )
    };
    Ok(format!(
        "You triage run {run_id} of dagq task {task_id} ({title}), which ended {status}. Decide what the supervisor does next.\n\
         Read only: the material below, and the files it names if you need more (the run directory is {dir}, the worktree {worktree}). Do not change any file.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Last error of the run:\n{last_error}\n\n\
         {material}\n\
         Earlier runs of the task:\n{earlier}\n\n\
         Decide one verdict:\n\
         - retry: the failure is transient or came from the environment (the machine slept, a process was killed, the session never started, an outage), and a new run from the current main is likely to succeed. The task goes back to ready and a new run starts from scratch; this run's work is not reused.\n\
         - resume: this run's worktree holds useful work that its own session can finish with a concrete instruction (fix the failing test, commit and rewrite the receipt, rebase). instruction is what the session must do, written to it.\n\
         - ask: a person has to decide: the task's instructions or acceptance look wrong or impossible, the same failure repeats, the work is no longer needed, or you cannot tell. instruction is the question for the person.\n\
         Rules: {retry_rule} {resume_rule}\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"retry\" | \"resume\" | \"ask\", \"reason\": string, \"instruction\": string}}\n\
         reason is one or two sentences on why; instruction may be empty for retry.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        status = run.status().as_str(),
        dir = dir.display(),
        worktree = run.worktree_path().unwrap_or("none"),
        description = or_none(task.description()),
        acceptance = or_none(task.acceptance()),
        last_error = or_none(run.last_error().unwrap_or_default()),
        earlier = if earlier.is_empty() {
            "none".to_owned()
        } else {
            earlier.join("\n")
        },
    ))
}

/// The fixed request the supervisor types into the live session for a
/// `revise` verdict (ADR-0027 decision 2), one instruction per line; the
/// backend sends it as one line.
fn revise_request(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    reasons: &[String],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let verify = serde_json::to_string(task.verification_commands())?;
    let mut lines = vec![format!(
        "dagq: the supervisor's review of run {} (task {}) asks for changes (revise {attempt} of {MAX_REVISE_ATTEMPTS}).",
        run.id(),
        task.id()
    )];
    lines.push("Findings:".to_owned());
    for reason in reasons {
        lines.push(format!("- {reason}"));
    }
    lines.push("Steps:".to_owned());
    lines.push("1. Fix the findings in this worktree and commit.".to_owned());
    lines.push(format!("2. Run the verification commands {verify}."));
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {STOP_BACKGROUND}"));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(
        "6. Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// The idle marker as the provider's Stop hook last wrote it: when, and the
/// hook's input JSON. The agent is idle when it finished a response and
/// left no background work running (task 147): Claude Code lists the
/// background tasks of the turn in `background_tasks`, and a `/exit` sent
/// while one is `running` stops at its "Background work is running" dialog,
/// which stays until someone answers it. When the work ends, Claude Code
/// takes the turn up again and the hook writes a new marker. A hook input
/// without `background_tasks` (an older Claude Code) counts as idle. The
/// screen is not read (ADR-0016).
struct IdleMarker {
    path: PathBuf,
    modified: SystemTime,
    hook: Value,
}

impl IdleMarker {
    /// `None` when the hook never wrote one. Time and content come from one
    /// open file, so they belong to the same write (the hook replaces the
    /// marker by a rename).
    fn read(files: &dyn RunFiles, path: &Path) -> Result<Option<Self>> {
        let Some((modified, bytes)) = files.read_stamped(path)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            path: path.to_owned(),
            modified,
            hook: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }))
    }

    /// Background work the agent left running when it stopped.
    fn background_running(&self) -> bool {
        self.hook
            .get("background_tasks")
            .and_then(Value::as_array)
            .is_some_and(|tasks| tasks.iter().any(|task| task["status"] == "running"))
    }

    /// Idle, by a marker written after `since`.
    fn idle_since(&self, since: SystemTime) -> bool {
        !self.background_running() && self.modified > since
    }

    /// Evidence that the agent went idle after publishing the receipt: the
    /// marker is no older than the receipt. Markers from earlier turns (for
    /// example a question to the inbox) do not count.
    fn idle_after_receipt(&self, files: &dyn RunFiles, receipt: &Path) -> Result<Option<Value>> {
        if self.background_running() {
            return Ok(None);
        }
        self.stopped_after_receipt(files, receipt)
    }

    /// Evidence that the agent stopped after publishing the receipt, idle
    /// or not (its background work still running).
    fn stopped_after_receipt(&self, files: &dyn RunFiles, receipt: &Path) -> Result<Option<Value>> {
        let receipt_modified = files.modified(receipt)?;
        if self.modified < receipt_modified {
            return Ok(None);
        }
        let field = |name: &str| self.hook.get(name).cloned().unwrap_or(Value::Null);
        Ok(Some(json!({
            "marker_path": path_text(&self.path)?,
            "marker_modified": unix_seconds(self.modified),
            "receipt_modified": unix_seconds(receipt_modified),
            "hook_event_name": field("hook_event_name"),
            "session_id": field("session_id"),
            "stop_hook_active": field("stop_hook_active"),
            "background_running": self.background_running(),
        })))
    }
}

/// Whether the marker shows background work the agent left running.
fn background_running(files: &dyn RunFiles, marker: &Path) -> Result<bool> {
    Ok(IdleMarker::read(files, marker)?.is_some_and(|idle| idle.background_running()))
}

/// Cross-check the agent's receipt against Git on a thread with its own
/// connection. The task's verification commands do not run here: `integrate`
/// runs them once, after its rebase (ADR-0023 decision 1). Rejections become a
/// `Validation` that is not accepted; only errors in the checks themselves
/// propagate, leaving the run in `validating`.
fn spawn_validation(
    queues: Arc<dyn QueueOpener>,
    repository: Arc<dyn Repository + Send + Sync>,
    run: TaskRun,
    log: Arc<dyn NoteLog>,
) -> thread::JoinHandle<Result<Validation>> {
    thread::spawn(move || {
        let mut queue = queues.open()?;
        let task = queue.show(run.task_id())?.task;
        let checked = check_receipt(&*repository, &task, &run)?;
        Ok(match checked {
            Ok((receipt, commit)) => Validation {
                accepted: true,
                result_commit: Some(commit),
                reason: None,
                receipt: serde_json::to_value(receipt)?,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
            },
            Err(rejection) => {
                log.note(&format!("run {} rejected: {}", run.id(), rejection.reason));
                Validation {
                    accepted: false,
                    result_commit: rejection.commit,
                    reason: Some(rejection.reason),
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
    log: &dyn NoteLog,
) -> Result<TaskRun> {
    let workspace = run.workspace_id().context("missing workspace")?;
    match cmux.close(workspace) {
        Ok(()) => queue.workspace_closed(run.id(), token),
        Err(error) => {
            let message = format!("workspace {workspace} could not be closed: {error:#}");
            log.note(&format!("run {}: {message}", run.id()));
            queue.cleanup_failed(run.id(), token, &message)
        }
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    const TRUST: &str = "\
╭──────────────────────────────────────────────────────────────────────╮
│ Do you trust the files in this folder?                               │
│                                                                      │
│ /Users/me/.local/share/dagq/0123/runs/abcd/worktree                  │
│                                                                      │
│ Claude Code may read, write, or execute files contained in this      │
│ directory. This can pose security risks, so only use files from      │
│ trusted sources.                                                     │
│                                                                      │
│ ❯ 1. Yes, proceed                                                    │
│   2. No, exit                                                        │
│                                                                      │
╰──────────────────────────────────────────────────────────────────────╯
   Enter to confirm · Esc to exit
";

    const LSP_PLUGIN: &str = "\
 ✻ Welcome to Claude Code!

 Plugin recommendation

 This project uses Rust. The rust-analyzer LSP plugin gives Claude
 go-to-definition and diagnostics.

   1. Install rust-analyzer-lsp
 ❯ 2. Not now
   3. Don't suggest this again

 Enter to confirm · Esc to cancel


";

    const AUTO_MODE: &str = "\
> Implement the task

⏺ Reading the repository instructions.

────────────────────────────────────────────────────────────────────
 Auto mode is available

 Claude can run commands and edit files without asking each time,
 with a classifier that stops risky actions.

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

    const WORK: &str = "\
⏺ Bash(cargo test --locked)
  ⎿  test result: ok. 42 passed; 0 failed
     grep -n \"Esc to cancel\" src/runtime.rs
     let text = \"Do you trust the files in this folder?\";

⏺ Update(src/runtime.rs)
  ⎿  Updated src/runtime.rs with 3 additions
     1. Added the check
     2. Added the test

✽ Compiling… (esc to interrupt)

╭──────────────────────────────────────────────────────────────────────╮
│ ❯ run the tests again                                                │
╰──────────────────────────────────────────────────────────────────────╯
  ? for shortcuts
";

    #[test]
    fn detect_prompt_finds_the_three_dialogs() {
        assert_eq!(detect_prompt(TRUST), Some(PromptKind::Trust));
        assert_eq!(detect_prompt(LSP_PLUGIN), Some(PromptKind::Choice));
        assert_eq!(detect_prompt(AUTO_MODE), Some(PromptKind::Choice));
        let newer_trust = "│ Quick safety check: Is this a project you created or one you trust?\n│ ❯ 1. Yes, I trust this folder\n│   2. No, exit\n";
        assert_eq!(detect_prompt(newer_trust), Some(PromptKind::Trust));
        let wrapped = "Allow this edit?\n❯ 1. Yes, and don't ask again for edits in\n     /Users/me/worktree\n  2. No\n";
        assert_eq!(detect_prompt(wrapped), Some(PromptKind::Choice));
        assert_eq!(
            detect_prompt("Save changes?\n  Enter to confirm · Esc to cancel\n"),
            Some(PromptKind::Confirm)
        );
    }

    #[test]
    fn detect_prompt_ignores_a_working_session() {
        assert_eq!(detect_prompt(WORK), None);
        assert_eq!(detect_prompt(""), None);
        // A single marked option is not a choice among options.
        assert_eq!(detect_prompt("❯ 1. only line\n"), None);
        // A dialog scrolled far above the bottom no longer counts.
        let scrolled = format!("{AUTO_MODE}{}", "output line\n".repeat(PROMPT_SCAN_LINES));
        assert_eq!(detect_prompt(&scrolled), None);
    }

    #[test]
    fn screen_tail_keeps_the_last_non_empty_lines() {
        assert_eq!(screen_tail("a  \n\nb\nc\n\n\n", 2), "b\nc");
        assert_eq!(screen_tail("a\n", 15), "a");
        assert_eq!(option_text("❯ 12. Twelve"), Some("Twelve"));
        assert_eq!(option_text(". none"), None);
        assert_eq!(PromptKind::Confirm.as_str(), "confirm");
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use std::{collections::HashMap, io, sync::Mutex};

    /// Run files in memory, each with the time it was written.
    #[derive(Default)]
    struct MemoryFiles {
        files: Mutex<HashMap<PathBuf, (SystemTime, Vec<u8>)>>,
    }

    impl MemoryFiles {
        fn put(&self, path: &Path, modified: SystemTime, contents: &str) {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_owned(), (modified, contents.as_bytes().to_vec()));
        }
    }

    fn missing() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }

    impl RunFiles for MemoryFiles {
        fn create_dir_all(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn create_new_dir(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
            self.put(path, self.now(), &String::from_utf8_lossy(contents));
            Ok(())
        }
        fn copy(&self, _: &Path, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            let files = self.files.lock().unwrap();
            files
                .get(path)
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(missing)
        }
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
        }
        fn modified(&self, path: &Path) -> io::Result<SystemTime> {
            let files = self.files.lock().unwrap();
            files.get(path).map(|(at, _)| *at).ok_or_else(missing)
        }
        fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
            Ok(self.files.lock().unwrap().get(path).cloned())
        }
        fn is_file(&self, path: &Path) -> bool {
            self.exists(path)
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_000_000)
        }
    }

    fn idle_after_receipt(files: &MemoryFiles, receipt: &Path, marker: &Path) -> Option<Value> {
        IdleMarker::read(files, marker)
            .unwrap()
            .and_then(|idle| idle.idle_after_receipt(files, receipt).unwrap())
    }

    #[test]
    fn idle_marker_is_idle_unless_background_work_runs() {
        let files = MemoryFiles::default();
        let marker = Path::new("/run/idle.json");
        let receipt = Path::new("/run/receipt.json");
        assert!(IdleMarker::read(&files, marker).unwrap().is_none());
        assert!(!background_running(&files, marker).unwrap());
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        files.write(receipt, b"{}").unwrap();
        let before = files.now() - Duration::from_secs(60);
        for (hook, running) in [
            // An older Claude Code writes no `background_tasks`.
            (json!({"hook_event_name": "Stop"}), false),
            (json!({"background_tasks": []}), false),
            (
                json!({"background_tasks": [{"id": "b1", "status": "completed"}]}),
                false,
            ),
            (
                json!({"background_tasks": [
                    {"id": "b1", "status": "completed"},
                    {"id": "b2", "type": "shell", "status": "running"}
                ]}),
                true,
            ),
        ] {
            files.write(marker, hook.to_string().as_bytes()).unwrap();
            let idle = IdleMarker::read(&files, marker).unwrap().unwrap();
            assert_eq!(idle.background_running(), running, "{hook}");
            assert_eq!(background_running(&files, marker).unwrap(), running);
            assert_eq!(idle.idle_since(before), !running, "{hook}");
            assert!(!idle.idle_since(files.now() + Duration::from_secs(60)));
            assert_eq!(
                idle_after_receipt(&files, receipt, marker).is_some(),
                !running,
                "{hook}"
            );
            // Past the wait, the stop counts whatever still runs.
            let stopped = idle
                .stopped_after_receipt(&files, receipt)
                .unwrap()
                .unwrap();
            assert_eq!(stopped["background_running"], running);
            assert_eq!(stopped["marker_path"], "/run/idle.json");
        }
        // A marker that is not JSON still tells the agent stopped.
        files.write(marker, b"not json").unwrap();
        let evidence = idle_after_receipt(&files, receipt, marker).unwrap();
        assert_eq!(evidence["hook_event_name"], Value::Null);
        // Nor does a marker older than the receipt count.
        files.put(marker, files.now() - Duration::from_secs(3600), "{}");
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        // A receipt that cannot be read is an error, not idleness.
        let idle = IdleMarker::read(&files, marker).unwrap().unwrap();
        assert!(
            idle.stopped_after_receipt(&files, Path::new("/run/none"))
                .is_err()
        );
    }
}
