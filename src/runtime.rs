//! Execute claimed tasks in parallel, validate their receipts, close the
//! workspaces of accepted runs, land them on main one at a time, and recover
//! orphaned runs. One run's state machine is unchanged from the single-run
//! supervisor; the loop multiplexes independent slots and isolates failures.
//! A run whose supervisor died while its session lives on is adopted by a
//! supervisor with a free slot instead of being rerun (ADR-0012).
use crate::{
    application::{
        AgentProvider, MainRemote, SupervisorEnvironment, TaskStore, WorkspaceBackend,
        WorkspaceTags, dependency_graph,
    },
    domain::{
        ClaimOutcome, EvidenceCheck, Goal, IntegrationOutcome, MAX_RESUME_ATTEMPTS, NewTask,
        PUSH_REMOTE, Predecessor, PushReport, PushResult, Receipt, ReceiptResult,
        RegisteredFollowUp, RunLease, RunPaths, RunProcess, RunStatus, SessionRole, SupervisorMode,
        SupervisorRegistration, Task, TaskRun, evidence_missing_reason, heartbeat_stale,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, load_average, path_text, process_alive,
            resume_workspace_description, run_shell_to_log, shell_join, workspace_description,
            workspace_group_name,
        },
        location::{QueueLocation, runs_dir},
        run_env::load_run_env,
        runtime_store::{
            HEARTBEAT_TIMEOUT_SECS, Landing, LeasedRun, ResumeCandidate, RunPlan, Validation,
            lease_is_stale,
        },
        sqlite::SqliteQueue,
    },
    lifecycle::session_env,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, BufWriter, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::observer::ObserveMode;

pub fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct SuperviseOptions {
    /// Upper bound on runs executing at once.
    pub parallel: usize,
    /// Exit when no run is active and no task can be claimed, instead of
    /// polling for new work.
    pub once: bool,
    pub stop: Arc<AtomicBool>,
    /// Directory for one `supervisor-<started_at>-<pid>.log` per start,
    /// created if missing; `None` keeps the messages on stderr only.
    pub log_dir: Option<PathBuf>,
    /// Start the observer job when this long passed since the last one
    /// started or finished (ADR-0024 decision 4); zero disables the
    /// observer, the daily one included.
    pub observe_interval: Duration,
    /// Also run the daily observation once every 24 hours.
    pub observe_daily: bool,
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            log_dir: None,
            observe_interval: Duration::ZERO,
            observe_daily: false,
        }
    }
}

/// Where the supervisor's progress messages go: stderr as always and, with
/// `--log-dir`, a file per start so a launchd-resident supervisor (whose
/// stderr is one shared `launchd.log`) leaves a record per process.
#[derive(Clone, Default)]
pub struct SupervisorLog {
    file: Option<Arc<Mutex<fs::File>>>,
    pub path: Option<PathBuf>,
}

impl SupervisorLog {
    /// `<dir>/supervisor-<started_at>-<pid>.log`, appended to if it exists.
    pub fn open(dir: &Path, started_at: i64, pid: u32) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(format!("supervisor-{started_at}-{pid}.log"));
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self {
            file: Some(Arc::new(Mutex::new(file))),
            path: Some(path),
        })
    }

    /// One line on stderr and, timestamped, in the file. A file that stops
    /// accepting writes does not stop the supervisor.
    pub fn note(&self, message: &str) {
        eprintln!("{message}");
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "[{}] {message}", unix_time());
        }
    }
}

/// One process heartbeats its registration (a resident supervisor) and every
/// lease it holds with a single token.
struct Heartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Heartbeat {
    fn start(db: PathBuf, token: String) -> Self {
        let (stop, recv) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut queue = SqliteQueue::open(db)?;
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

    fn check(&self) -> Result<()> {
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
    pub run_id: String,
    pub task_id: i64,
    pub message: String,
}

/// Run and monitor tasks until the loop ends: with `once`, when nothing is
/// active or claimable; otherwise on `stop`, or after a provisioning failure
/// has drained the active runs (an error). Every task unblocked by `integrate`
/// is picked up on a later pass with the then-current `main` as its base.
pub fn supervise(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    ensure!(options.parallel >= 1, "parallel must be at least 1");
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let repository = GitRepository::inspect(repo)?;
    ensure!(
        !db.starts_with(&repository.root) || db.starts_with(&repository.common_dir),
        "keep the queue outside the worktree or under its Git common directory"
    );
    cmux.preflight()?;
    ClaudeCode {
        executable: claude.into(),
    }
    .preflight()?;
    let mut queue = SqliteQueue::open(&db)?;
    queue.bind_repository(&path_text(&repository.common_dir)?)?;
    let token = Uuid::new_v4().to_string();
    // Registered before the first heartbeat so the loop is visible to
    // `status` from its first second, runs or not.
    let parallel =
        u32::try_from(options.parallel).context("parallel does not fit a registration")?;
    let pid = std::process::id();
    let registration = queue.register_supervisor(&token, pid, parallel, crate::VERSION)?;
    let log = match &options.log_dir {
        Some(dir) => match SupervisorLog::open(dir, registration.started_at, pid) {
            Ok(log) => log,
            Err(error) => {
                // Not a supervisor after all: leave no row for `status`.
                let _ = queue.deregister_supervisor(&token);
                return Err(error);
            }
        },
        None => SupervisorLog::default(),
    };
    log.note(&format!(
        "supervisor {token} started: version {}, pid {pid}, parallel {parallel}, db {}, repository {}",
        crate::VERSION,
        db.display(),
        repository.root.display()
    ));
    let heartbeat = Heartbeat::start(db.clone(), token.clone());
    let queue_hash = QueueLocation::explicit(&db).hash();
    let cmux = RecordingBackend::new(cmux, db.clone(), Some(token.clone()));
    let mut supervisor = Supervisor {
        queue,
        db,
        repository,
        cmux: &cmux,
        claude,
        runner,
        token,
        heartbeat,
        log: log.clone(),
        slots: Vec::new(),
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
        queue_hash,
        observer: None,
        observers_launched: Vec::new(),
    };
    let result = supervisor.run_loop(options);
    match &result {
        Ok(value) => log.note(&format!("supervisor {} exiting: {value}", supervisor.token)),
        Err(error) => log.note(&format!(
            "supervisor {} failed: {error:#}",
            supervisor.token
        )),
    }
    result
}

/// `backend_call_failed` keeps this many leading characters of the error.
pub const BACKEND_ERROR_CHARS: usize = 300;

/// The payload of `backend_call_failed`: the call (`op`, the workspace it
/// was for, the backend's per-call timeout), its error cut to
/// [`BACKEND_ERROR_CHARS`] characters, and the load it failed under — the
/// 1-minute load average (null when unavailable), the slots held and the
/// `parallel` offered (null without a supervisor).
pub fn backend_failure_payload(
    op: &str,
    workspace_id: Option<&str>,
    timeout: Duration,
    error: &str,
    load_avg: Option<f64>,
    slots: i64,
    parallel: Option<i64>,
) -> Value {
    json!({
        "op": op,
        "workspace_id": workspace_id,
        "timeout_secs": timeout.as_secs(),
        "error": error.chars().take(BACKEND_ERROR_CHARS).collect::<String>(),
        "load_avg": load_avg,
        "slots": slots,
        "parallel": parallel,
    })
}

/// A [`WorkspaceBackend`] that records every failed or timed-out call as
/// `backend_call_failed` before handing the error back unchanged, so the
/// queue keeps how often cmux fails and under what load (task 109). The
/// record is made here, in the application layer, and not in the cmux
/// adapter (ADR-0013). A call made for a run (`create`, or any call on a
/// workspace a run opened) is recorded on that run; one that belongs to no
/// run (`up`'s workspaces, the queue's group, `down`'s close) without one.
/// `token` is the supervisor whose slots are reported; `None` (`up`,
/// `down`) reports every lease and supervisor. The record is written
/// through its own connection, and a record that cannot be written is
/// dropped: it must never hide the backend's error.
pub struct RecordingBackend<'a> {
    inner: &'a dyn WorkspaceBackend,
    db: PathBuf,
    token: Option<String>,
}

impl<'a> RecordingBackend<'a> {
    pub fn new(inner: &'a dyn WorkspaceBackend, db: PathBuf, token: Option<String>) -> Self {
        Self { inner, db, token }
    }

    fn recorded<T>(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&str>,
        result: Result<T>,
    ) -> Result<T> {
        if let Err(error) = &result {
            let _ = self.record(op, workspace_id, run_id, &format!("{error:#}"));
        }
        result
    }

    fn record(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&str>,
        error: &str,
    ) -> Result<()> {
        let queue = SqliteQueue::open(&self.db)?;
        let run_id = match (run_id, workspace_id) {
            (Some(run_id), _) => Some(run_id.to_owned()),
            (None, Some(workspace_id)) => queue.run_in_workspace(workspace_id)?,
            (None, None) => None,
        };
        let (slots, parallel) = queue.backend_slots(self.token.as_deref())?;
        queue.record_backend_failure(
            run_id.as_deref(),
            backend_failure_payload(
                op,
                workspace_id,
                self.inner.call_timeout(),
                error,
                load_average(),
                slots,
                parallel,
            ),
        )
    }
}

impl WorkspaceBackend for RecordingBackend<'_> {
    fn preflight(&self) -> Result<()> {
        self.inner.preflight()
    }
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.inner.preflight_detached(environment)
    }
    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create(task, run, command, tags);
        self.recorded("create", None, Some(&run.id), result)
    }
    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_resume(task, run, command, tags);
        self.recorded("create_resume", None, Some(&run.id), result)
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        let result = self.inner.send_text(workspace_id, text);
        self.recorded("send_text", Some(workspace_id), None, result)
    }
    fn capture(&self, workspace_id: &str) -> Result<String> {
        let result = self.inner.capture(workspace_id);
        self.recorded("capture", Some(workspace_id), None, result)
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.close(workspace_id);
        self.recorded("close", Some(workspace_id), None, result)
    }
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_exit(workspace_id);
        self.recorded("send_exit", Some(workspace_id), None, result)
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        let result = self.inner.exists(workspace_id);
        self.recorded("exists", Some(workspace_id), None, result)
    }
    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_named(name, cwd, command, tags);
        self.recorded("create_named", None, None, result)
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let result = self.inner.ensure_group(external_id, name);
        self.recorded("ensure_group", None, None, result)
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let result = self.inner.notify(title, body, workspace);
        self.recorded("notify", workspace, None, result)
    }
    fn call_timeout(&self) -> Duration {
        self.inner.call_timeout()
    }
    fn exit_timeout(&self) -> Duration {
        self.inner.exit_timeout()
    }
    fn registration_timeout(&self) -> Duration {
        self.inner.registration_timeout()
    }
    fn prompt_wait(&self) -> Duration {
        self.inner.prompt_wait()
    }
    fn resume_prompt_delay(&self) -> Duration {
        self.inner.resume_prompt_delay()
    }
    fn resume_timeout(&self) -> Duration {
        self.inner.resume_timeout()
    }
}

const IDLE_POLL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);

struct Supervisor<'a> {
    queue: SqliteQueue,
    db: PathBuf,
    repository: GitRepository,
    cmux: &'a dyn WorkspaceBackend,
    claude: &'a Path,
    runner: &'a Path,
    token: String,
    heartbeat: Heartbeat,
    log: SupervisorLog,
    slots: Vec<Slot>,
    finished: Vec<TaskRun>,
    errors: Vec<RunError>,
    /// Cleared after a provisioning failure so an unavailable cmux or Git
    /// does not burn through every candidate.
    claiming: bool,
    provisioning_error: Option<String>,
    /// The queue hash: the external ID of the queue's workspace group and
    /// part of every run workspace's description (ADR-0026).
    queue_hash: String,
    /// The observer job running now: one at a time, outside the run slots.
    observer: Option<(ObserveMode, std::process::Child)>,
    /// When this process last launched each observation, so one that dies
    /// before it records anything is not relaunched on every pass.
    observers_launched: Vec<(ObserveMode, Instant)>,
}

/// One executing run between provisioning and rest.
struct Slot {
    run: TaskRun,
    phase: Phase,
}

enum Phase {
    Session(SessionWatch),
    /// Receipt validation runs off the loop because verification commands may
    /// take minutes; the loop only joins the result.
    Validating(Option<thread::JoinHandle<Result<Validation>>>),
    /// A resumed session of a `needs_session` run (ADR-0019).
    Resume(ResumeWatch),
    /// A resolved run whose integrate was approved waits for the single
    /// integration slot, keeping its lease.
    AwaitingSlot,
    /// The approved run lands off the loop, like validation; the landing
    /// releases the lease itself.
    Landing(Option<thread::JoinHandle<Result<IntegrationOutcome>>>),
}

enum Step {
    Continue,
    Done(Box<TaskRun>),
    /// The lease now carries another token (an adopter took the run, or
    /// `recover` released it): this process must not touch the run again.
    Disowned,
}

impl Supervisor<'_> {
    /// Drive the loop, then remove this process's registration: it is about
    /// to exit, whether it drained its runs, ran out of work, or failed on
    /// a claim or provisioning. Only a heartbeat failure keeps the row (the
    /// database may be unreachable), and it goes stale with the leases.
    fn run_loop(&mut self, options: &SuperviseOptions) -> Result<Value> {
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

    fn drive(&mut self, options: &SuperviseOptions) -> Result<Value> {
        loop {
            if let Err(error) = self.heartbeat.check() {
                // Supervisor-level failure: note it on every run and keep the
                // leases and the registration; they go stale once this
                // process is gone.
                for slot in &self.slots {
                    let _ = self
                        .queue
                        .record_runtime_error(&slot.run.id, &format!("{error:#}"));
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
                    TICK
                } else {
                    IDLE_POLL
                });
                continue;
            }
            self.tick();
            thread::sleep(TICK);
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
        Ok(json!({"outcome": outcome, "runs": self.finished, "errors": self.errors}))
    }

    /// Adopt the runs other supervisors left behind, then claim and
    /// provision candidates until every slot is taken or nothing is
    /// claimable. `main` is reread per claim so a task released by
    /// `integrate` starts from the main that contains its predecessor.
    fn fill_slots(&mut self, parallel: usize) -> Result<()> {
        if self.slots.len() < parallel {
            self.adopt_stale_runs(parallel)?;
        }
        if self.slots.len() < parallel {
            self.resume_parked_runs(parallel)?;
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
                    let run = self.queue.run(&run.id)?;
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Session(watch),
                    });
                }
                Err(error) => {
                    let message = format!("run {} provisioning failed: {error:#}", run.id);
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
                        .note(&format!("run {} is {}", run.id, run.status.as_str()));
                    self.finished.push(*run);
                }
                Ok(Step::Disowned) => self.disown(&slot),
                // A lease-guarded write that failed because the lease
                // changed hands mid-step (this process was stalled and
                // adopted from) is the other owner's run to describe.
                Err(_)
                    if !self
                        .queue
                        .holds_lease(&slot.run.id, &self.token)
                        .unwrap_or(true) =>
                {
                    self.disown(&slot)
                }
                Err(error) if matches!(slot.phase, Phase::AwaitingSlot) => {
                    // The resume already recorded its `resume_finished`;
                    // only the lease it kept for the landing goes.
                    let message = format!("landing after the resume could not start: {error:#}");
                    self.log.note(&format!("run {}: {message}", slot.run.id));
                    if let Err(error) = self.queue.release_lease(&slot.run.id, &self.token) {
                        self.log.note(&format!(
                            "run {}: could not release the lease: {error:#}",
                            slot.run.id
                        ));
                    }
                    self.errors.push(RunError {
                        run_id: slot.run.id.clone(),
                        task_id: slot.run.task_id,
                        message,
                    });
                }
                Err(error) if matches!(slot.phase, Phase::Resume(_)) => {
                    let message = format!("{error:#}");
                    self.log.note(&format!(
                        "run {} resume stopped: {message}; its workspace is kept for inspection",
                        slot.run.id
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
                    // and keep serving the other slots.
                    let message = format!("{error:#}");
                    self.log.note(&format!(
                        "run {} retained for inspection: {message}; see show {} and doctor",
                        slot.run.id, slot.run.task_id
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
    fn due_observation(&self, options: &SuperviseOptions) -> Result<Option<ObserveMode>> {
        if options.observe_interval.is_zero() {
            return Ok(None);
        }
        let now = unix_time();
        let mut modes = vec![(
            ObserveMode::Hourly,
            i64::try_from(options.observe_interval.as_secs())?,
        )];
        if options.observe_daily {
            modes.insert(0, (ObserveMode::Daily, crate::observer::DAILY_WINDOW_SECS));
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
    fn start_observer_when_due(&mut self, options: &SuperviseOptions) {
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
        let mut command = std::process::Command::new(self.runner);
        command
            .arg("--db")
            .arg(&self.db)
            .arg("observe")
            .arg("--claude")
            .arg(self.claude)
            .current_dir(&self.repository.root)
            .env_remove(crate::lifecycle::ROLE_ENV)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if mode == ObserveMode::Daily {
            command.arg("--daily");
        }
        match command.spawn() {
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
            slot.run.id
        );
        if matches!(slot.phase, Phase::Validating(Some(_))) {
            // Its checks finish on their own; the new owner runs its own.
            message.push_str("; a validation already in progress runs to completion unrecorded");
        }
        self.log.note(&message);
        self.errors.push(RunError {
            run_id: slot.run.id.clone(),
            task_id: slot.run.task_id,
            message,
        });
    }

    fn abandon(&mut self, run: &TaskRun, message: String) {
        if let Err(error) = self.queue.abandon_run(&run.id, &self.token, &message) {
            self.log.note(&format!(
                "run {}: could not record the error: {error:#}",
                run.id
            ));
        }
        self.errors.push(RunError {
            run_id: run.id.clone(),
            task_id: run.task_id,
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
                .finish_resume(&run.id, &self.token, None, None, false, payload)
        {
            self.log.note(&format!(
                "run {}: could not record the resume error: {error:#}",
                run.id
            ));
        }
        let session_may_live = self.queue.processes(&run.id).map_or(true, |processes| {
            processes
                .iter()
                .any(|p| p.role == "wrapper" && p.exited_at.is_none())
        });
        if let Some(workspace) = workspace {
            if session_may_live {
                self.log.note(&format!(
                    "run {}: resume workspace {workspace} is kept; its session may still run",
                    run.id
                ));
            } else if let Err(error) = self.cmux.close(&workspace) {
                self.log.note(&format!(
                    "run {}: resume workspace {workspace} could not be closed: {error:#}",
                    run.id
                ));
            }
        }
        self.errors.push(RunError {
            run_id: run.id.clone(),
            task_id: run.task_id,
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
                        "run {} landing after its resume: {}",
                        slot.run.id, outcome["outcome"]
                    ));
                }
                Err(error) => {
                    let message = format!("landing after the resume failed: {error:#}");
                    self.log.note(&format!("run {}: {message}", slot.run.id));
                    self.errors.push(RunError {
                        run_id: slot.run.id.clone(),
                        task_id: slot.run.task_id,
                        message,
                    });
                }
            }
            return Ok(Step::Done(Box::new(self.queue.run(&slot.run.id)?)));
        }
        if !self.queue.holds_lease(&slot.run.id, &self.token)? {
            return Ok(Step::Disowned);
        }
        match &mut slot.phase {
            Phase::Resume(watch) => {
                let Some(verdict) = watch.poll(
                    &mut self.queue,
                    self.cmux,
                    &self.repository,
                    &slot.run,
                    &self.log,
                )?
                else {
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
                let main = self.repository.main_head()?;
                let run = match self
                    .queue
                    .begin_integration(&slot.run.id, &self.token, &main)
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
                    "run {} was approved for integration; landing it onto main {main}",
                    run.id
                ));
                let db = self.db.clone();
                let repository = self.repository.clone();
                let token = self.token.clone();
                let common_dir = path_text(&self.repository.common_dir)?;
                // Push as the approving `integrate` would have (`--no-push`
                // records `push: false`).
                let push = self
                    .queue
                    .run_events(&run.id)?
                    .iter()
                    .find(|e| e.kind == "integration_approved")
                    .is_none_or(|e| e.payload.get("push") != Some(&json!(false)));
                let landing = run.clone();
                slot.run = run;
                slot.phase = Phase::Landing(Some(thread::spawn(move || {
                    let mut queue = SqliteQueue::open(&db)?;
                    land_integrating(
                        &mut queue,
                        &db,
                        &repository,
                        &landing,
                        RunStatus::NeedsSession,
                        &main,
                        &token,
                        &common_dir,
                        push.then_some(&repository as &dyn MainRemote),
                    )
                })));
                Ok(Step::Continue)
            }
            Phase::Landing(_) => unreachable!("joined above"),
            Phase::Session(watch) => {
                let Some(run) = watch.poll(
                    &mut self.queue,
                    self.cmux,
                    &self.repository,
                    &self.token,
                    &slot.run,
                    &self.log,
                )?
                else {
                    return Ok(Step::Continue);
                };
                if run.status != RunStatus::Validating {
                    self.queue.release_lease(&run.id, &self.token)?;
                    return Ok(Step::Done(Box::new(run)));
                }
                let handle = spawn_validation(
                    self.db.clone(),
                    self.repository.clone(),
                    run.clone(),
                    self.log.clone(),
                );
                slot.run = run;
                slot.phase = Phase::Validating(Some(handle));
                Ok(Step::Continue)
            }
            Phase::Validating(handle) => {
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
                    .finish_validation(&slot.run.id, &self.token, &validation)?;
                // An accepted run gives up its workspace, and so does one
                // parked for evidence: its session ended, and a resume opens
                // a workspace of its own. Failures keep it for inspection.
                let run = if matches!(
                    run.status,
                    RunStatus::AwaitingIntegration | RunStatus::NeedsSession
                ) {
                    close_workspace(&mut self.queue, self.cmux, &self.token, &run, &self.log)?
                } else {
                    run
                };
                self.queue.release_lease(&run.id, &self.token)?;
                Ok(Step::Done(Box::new(run)))
            }
        }
    }

    /// Resume `needs_session` runs with attempts left (ADR-0019 decision 1),
    /// oldest first, while slots are free: a run with a lease that is not
    /// stale, or whose last session still runs, is someone's already.
    fn resume_parked_runs(&mut self, parallel: usize) -> Result<()> {
        for candidate in self.queue.runs_needing_session()? {
            if self.slots.len() >= parallel {
                break;
            }
            let ResumeCandidate {
                run,
                lease,
                wrapper,
                attempts,
            } = candidate;
            let now = unix_time();
            // A previous session whose wrapper process lives on, however
            // silent, is never joined by a second one on the same worktree.
            if attempts >= MAX_RESUME_ATTEMPTS
                || lease.is_some_and(|lease| !lease_is_stale(&lease, now))
                || wrapper.is_some_and(|w| w.exited_at.is_none() && process_alive(w.pid))
            {
                continue;
            }
            self.close_left_resume_workspaces(&run)?;
            let main = self.repository.main_head()?;
            let (reason, evidence_missing) = resume_reason(&self.queue, &run)?;
            let Some((run, attempt)) = self.queue.begin_resume(
                &run.id,
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
                evidence_missing,
            };
            match self.start_resume(&run, attempt, &request) {
                Ok(watch) => {
                    self.log.note(&format!(
                        "run {} of task {} resumed (attempt {attempt} of {MAX_RESUME_ATTEMPTS}) in workspace {}",
                        run.id, run.task_id, watch.workspace
                    ));
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Resume(watch),
                    });
                }
                Err(error) => {
                    let message = format!("run {} could not be resumed: {error:#}", run.id);
                    self.log.note(&message);
                    self.give_up_resume(&run, attempt, None, message);
                }
            }
        }
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
            .run_events(&run.id)?
            .iter()
            .filter(|e| e.kind == "resume_finished" && e.payload["workspace_closed"] != true)
            .filter_map(|e| e.payload.get("workspace_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        for workspace in left {
            if self.cmux.exists(&workspace)? {
                self.log.note(&format!(
                    "run {}: closing resume workspace {workspace} left by an earlier attempt; its session has ended",
                    run.id
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
        let run_dir = PathBuf::from(run.run_dir.as_ref().context("missing run directory")?);
        let worktree = Path::new(run.worktree_path.as_ref().context("missing worktree")?);
        ensure!(
            worktree.is_dir(),
            "worktree {} is missing",
            worktree.display()
        );
        let task = self.queue.show(run.task_id)?.task;
        let landed = landed_since(&mut self.queue, &self.repository, run, &request.main)?;
        let message = resume_request(&task, run, request, &landed)?;
        fs::write(run_dir.join(format!("resume-{attempt}.txt")), &message)?;
        fs::copy(self.runner, run_dir.join("runner")).context("snapshot runtime binary")?;
        let command = shell_join(&[
            path_text(&run_dir.join("runner"))?,
            "--db".into(),
            path_text(&self.db)?,
            "session".into(),
            "--run".into(),
            run.id.clone(),
            "--lease".into(),
            self.token.clone(),
            "--claude".into(),
            path_text(self.claude)?,
            "--resume".into(),
        ]);
        // The worker's env and group (the same session of the run) and the
        // description `run <run-id> resume` (ADR-0028).
        let tags = WorkspaceTags {
            env: session_env(SessionRole::Worker, &self.db)?,
            description: Some(resume_workspace_description(run)),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create_resume(&task, run, &command, &tags)?;
        Ok(ResumeWatch {
            workspace,
            attempt,
            run_dir,
            receipt_path: PathBuf::from(run.receipt_path.as_ref().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at: SystemTime::now(),
            startup: Instant::now(),
            message,
            agent_seen: None,
            message_sent: None,
            exit_requested: None,
            required_evidence: task.required_evidence.clone(),
        })
    }

    /// The resumed session ended: close its workspace, record
    /// `resume_finished` and move the run on. A resolved run whose
    /// integrate was approved keeps its lease and waits for the landing
    /// slot; an unapproved one goes back to `awaiting_integration`; a
    /// `failed` receipt ends the run; anything else leaves it
    /// `needs_session` for the next attempt, or for a human after the last.
    fn finish_resumed_session(
        &mut self,
        slot: &mut Slot,
        attempt: usize,
        workspace: &str,
        verdict: ResumeVerdict,
    ) -> Result<Step> {
        // A session let go after the exit timeout still runs: its
        // workspace stays, and blocks the next attempt until it ends.
        let closed = !verdict.exit_timed_out
            && match self.cmux.close(workspace) {
                Ok(()) => true,
                Err(error) => {
                    self.log.note(&format!(
                        "run {}: resume workspace {workspace} could not be closed: {error:#}",
                        slot.run.id
                    ));
                    false
                }
            };
        let approved = self
            .queue
            .has_run_event(&slot.run.id, "integration_approved")?;
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
        let id = slot.run.id.clone();
        let run = match verdict.kind {
            ResumeOutcome::Resolved if approved => {
                let run = self
                    .queue
                    .finish_resume(&id, &self.token, None, None, true, payload)?;
                slot.run = run;
                slot.phase = Phase::AwaitingSlot;
                return Ok(Step::Continue);
            }
            ResumeOutcome::Resolved => self.queue.finish_resume(
                &id,
                &self.token,
                Some(RunStatus::AwaitingIntegration),
                None,
                false,
                payload,
            )?,
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
            let now = unix_time();
            let LeasedRun {
                run,
                lease,
                wrapper,
            } = candidate;
            if !lease_is_stale(&lease, now) {
                continue;
            }
            let Some(wrapper) = wrapper else {
                continue;
            };
            let alive = wrapper.exited_at.is_none().then(|| {
                process_alive(wrapper.pid) && now - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
            });
            if alive == Some(false) {
                continue;
            }
            let observed = json!({
                "pid": wrapper.pid,
                "alive": alive,
                "exited_at": wrapper.exited_at,
            });
            let pid = std::process::id();
            let Some(run) =
                self.queue
                    .adopt_run(&run.id, &lease.token, &self.token, pid, observed)?
            else {
                self.log.note(&format!(
                    "run {} was not adopted: its lease changed while judging it",
                    run.id
                ));
                continue;
            };
            self.log.note(&format!(
                "run {} adopted from supervisor {} (pid {}, heartbeat {}s old; wrapper pid {} {}): task {} in workspace {}",
                run.id,
                lease.token,
                lease.pid,
                now - lease.heartbeat_at,
                wrapper.pid,
                match wrapper.exited_at {
                    Some(at) => format!("exited at {at}"),
                    None => "alive".to_owned(),
                },
                run.task_id,
                run.workspace_id.as_deref().unwrap_or("?")
            ));
            match self.resume(&run) {
                Ok(phase) => self.slots.push(Slot { run, phase }),
                Err(error) => {
                    // The lease is this process's now; give it up like any
                    // other runtime error so `recover` can judge the run.
                    let message = format!("run {} could not be resumed: {error:#}", run.id);
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
        Ok(match run.status {
            RunStatus::Validating => Phase::Validating(Some(spawn_validation(
                self.db.clone(),
                self.repository.clone(),
                run.clone(),
                self.log.clone(),
            ))),
            _ => {
                let receipt_path =
                    PathBuf::from(run.receipt_path.as_ref().context("missing receipt path")?);
                let receipt_seen = receipt_path.is_file()
                    && self.queue.has_run_event(&run.id, "receipt_observed")?;
                let exit_requested = self
                    .queue
                    .has_run_event(&run.id, "exit_requested")?
                    .then(Instant::now);
                let exit_timed_out = self
                    .queue
                    .has_run_event(&run.id, "exit_request_timed_out")?;
                let first_commit_seen =
                    self.queue.has_run_event(&run.id, "first_commit_observed")?;
                // A dialog recorded before adoption is not recorded again
                // while the same screen stays up.
                let prompt_hash = self
                    .queue
                    .run_events(&run.id)?
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
                        .workspace_id
                        .clone()
                        .context("adopted run has no workspace")?,
                    run_dir: PathBuf::from(run.run_dir.as_ref().context("missing run directory")?),
                    receipt_path,
                    idle_marker: run.idle_marker_path()?,
                    startup: Instant::now(),
                    receipt_seen,
                    exit_requested,
                    exit_timed_out,
                    first_commit_seen,
                    agent_seen: None,
                    prompt_checked: None,
                    prompt_hash,
                })
            }
        })
    }

    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
    /// The queue's workspace group, asked for with every run workspace:
    /// the call is idempotent by external ID, and cmux removes a group whose
    /// last workspace closes, so a handle kept from an earlier run could
    /// name a group that is gone. A group cmux cannot make is a warning in
    /// the log, and the run opens outside it.
    fn workspace_group(&self) -> Option<String> {
        let name = workspace_group_name(&self.repository.root);
        match self.cmux.ensure_group(&self.queue_hash, &name) {
            Ok(group) => Some(group),
            Err(error) => {
                self.log.note(&format!(
                    "warning: cmux workspace group {name:?} (external ID {}) could not be made, \
so the run workspace opens outside it: {error:#}",
                    self.queue_hash
                ));
                None
            }
        }
    }

    fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = runs_dir(&self.db);
        let paths = RunPaths::new(&state_dir, &claimed.id);
        let run_dir = paths.run_dir.clone();
        let plan = RunPlan {
            repo_path: path_text(&self.repository.root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("dagq/{}", claimed.id),
            worktree_path: path_text(&paths.worktree)?,
            receipt_path: path_text(&paths.receipt)?,
            log_path: path_text(&paths.log)?,
        };
        // Save intended paths before any external resource is created.
        self.queue.plan_run(&claimed.id, &self.token, &plan)?;
        fs::create_dir_all(&state_dir)?;
        fs::create_dir(&run_dir).context("run directory must be new")?;
        let run_env = run_env(&self.repository, &self.db, &run_dir)?;
        let run = self.queue.run(&claimed.id)?;
        let task = self.queue.show(run.task_id)?.task;
        let predecessors: Vec<PredecessorSummary> = self
            .queue
            .predecessors(task.id)?
            .iter()
            .map(PredecessorSummary::from_predecessor)
            .collect();
        let goal = match task.goal_id {
            Some(goal_id) => Some(self.queue.show_goal(goal_id)?.goal),
            None => None,
        };
        let siblings = siblings_in_progress(&task, self.queue.tasks_in_progress()?);
        fs::write(
            run_dir.join("prompt.txt"),
            prompt(&task, &run, goal.as_ref(), &predecessors, &siblings)?,
        )?;
        // A running wrapper must not change when the development binary is rebuilt.
        fs::copy(self.runner, run_dir.join("runner")).context("snapshot runtime binary")?;
        let git_output = self.repository.create_worktree(&run)?;
        fs::write(run_dir.join("worktree-create.txt"), git_output)?;
        self.queue.record_runtime_event(
            &run.id,
            "worktree_created",
            json!({"path": plan.worktree_path, "branch": plan.branch}),
        )?;
        let command = shell_join(&[
            path_text(&run_dir.join("runner"))?,
            "--db".into(),
            path_text(&self.db)?,
            "session".into(),
            "--run".into(),
            run.id.clone(),
            "--lease".into(),
            self.token.clone(),
            "--claude".into(),
            path_text(self.claude)?,
        ]);
        let mut env = session_env(SessionRole::Worker, &self.db)?;
        env.extend(run_env);
        let tags = WorkspaceTags {
            env,
            description: Some(workspace_description(
                SessionRole::Worker,
                &self.queue_hash,
                Some(&run.id),
                Some(run.task_id),
            )),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create(&task, &run, &command, &tags)?;
        self.queue
            .workspace_created(&run.id, &self.token, &workspace)?;
        self.log.note(&format!(
            "task {} running in workspace {}; run {}",
            run.task_id, workspace, run.id
        ));
        Ok(SessionWatch {
            workspace,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            exit_requested: None,
            exit_timed_out: false,
            first_commit_seen: false,
            agent_seen: None,
            prompt_checked: None,
            prompt_hash: None,
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
}

impl SessionWatch {
    /// One observation. `Some` once the wrapper exited and supervision finished
    /// (`validating` or `failed`); an error means the run must be retained.
    fn poll(
        &mut self,
        queue: &mut SqliteQueue,
        cmux: &dyn WorkspaceBackend,
        repository: &GitRepository,
        token: &str,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<Option<TaskRun>> {
        let processes = queue.processes(&run.id)?;
        self.watch_first_commit(queue, repository, run, log)?;
        if !self.receipt_seen && self.receipt_path.is_file() {
            self.receipt_seen = true;
            queue.record_runtime_event(
                &run.id,
                "receipt_observed",
                json!({"path": path_text(&self.receipt_path)?, "validated": false}),
            )?;
            log.note(&format!(
                "receipt received for {}; waiting for the session to go idle (or a maintainer /exit)",
                run.id
            ));
        }
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        // A session that already ended (on its own, by a maintainer's /exit,
        // or before this supervisor adopted the run) is not asked to exit.
        let session_ended = wrapper.is_some_and(|w| w.exited_at.is_some());
        if self.receipt_seen
            && self.exit_requested.is_none()
            && !session_ended
            && let Some(evidence) = idle_after_receipt(&self.receipt_path, &self.idle_marker)?
        {
            queue.record_runtime_event(&run.id, "session_idle_observed", evidence)?;
            // Recorded before sending: the session may exit, and its wrapper
            // record `session_exited`, before the send returns. A failed send
            // abandons the run lease-less; a supervisor killed between the two
            // leaves an adopter that never sends, and the run waits out the
            // exit timeout for a human `/exit` (never a second `/exit`).
            let timeout = cmux.exit_timeout();
            queue.record_runtime_event(
                &run.id,
                "exit_requested",
                json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
            )?;
            // Ask once, the way the maintainer would; never kill the session.
            cmux.send_exit(&self.workspace)?;
            log.note(&format!(
                "exit requested for {}; waiting for session exit",
                run.id
            ));
            self.exit_requested = Some(Instant::now());
        }
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                match cmux.capture(&self.workspace) {
                    Ok(screen) => fs::write(self.run_dir.join("terminal-final.txt"), screen)?,
                    Err(error) => queue.record_runtime_event(
                        &run.id,
                        "screen_capture_failed",
                        json!({"error": format!("{error:#}")}),
                    )?,
                }
                return queue.finish_supervision(&run.id, token).map(Some);
            }
            ensure!(
                unix_time() - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS,
                "wrapper heartbeat expired; session may still be alive"
            );
            if self.exit_requested.is_none() {
                self.deliver_answers(queue, cmux, run, log)?;
            }
            if let Some(agent) = processes.iter().find(|p| p.role == "agent") {
                self.watch_prompt(queue, cmux, run, agent, log)?;
            }
        } else {
            let timeout = cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "wrapper did not register within {} seconds",
                timeout.as_secs()
            );
        }
        if let Some(requested) = self.exit_requested
            && !self.exit_timed_out
        {
            let timeout = cmux.exit_timeout();
            if requested.elapsed() >= timeout {
                // Something in the session (for example a dialog) held the
                // /exit back. Keep the lease and keep watching: the run
                // proceeds to validation once the session exits. /exit is not
                // sent again, since it could pick another option of a dialog.
                queue.record_runtime_event(
                    &run.id,
                    "exit_request_timed_out",
                    json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                log.note(&format!(
                    "session for {} did not exit within {}s of the exit request; keeping the run and waiting (send /exit in workspace {})",
                    run.id,
                    timeout.as_secs(),
                    self.workspace
                ));
                self.exit_timed_out = true;
            }
        }
        Ok(None)
    }

    /// Record `first_commit_observed` once, the first time the worktree's
    /// HEAD is seen away from the run's base commit: with `agent_started` it
    /// measures how long a session takes to start working (`stats`'s
    /// `startup`). The time is when this poll saw it, at most a tick late.
    /// A HEAD that cannot be read is noted and checked again next poll.
    fn watch_first_commit(
        &mut self,
        queue: &mut SqliteQueue,
        repository: &GitRepository,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<()> {
        if self.first_commit_seen {
            return Ok(());
        }
        let Some(worktree) = run.worktree_path.as_deref() else {
            return Ok(());
        };
        let head = match repository.head(Path::new(worktree)) {
            Ok(head) => head,
            Err(error) => {
                log.note(&format!(
                    "HEAD of {} could not be read for its first commit: {error:#}",
                    run.id
                ));
                return Ok(());
            }
        };
        if head != run.base_commit {
            queue.record_runtime_event(
                &run.id,
                "first_commit_observed",
                json!({"commit": head, "base_commit": run.base_commit}),
            )?;
            self.first_commit_seen = true;
        }
        Ok(())
    }

    /// Read the screen of a session that has run for `prompt_wait` with
    /// neither a receipt nor an idle marker, its wrapper and agent alive, and
    /// record a dialog found there as `prompt_waiting` (once per screen) and
    /// its disappearance as `prompt_cleared`. No key is sent (ADR-0019).
    fn watch_prompt(
        &mut self,
        queue: &mut SqliteQueue,
        cmux: &dyn WorkspaceBackend,
        run: &TaskRun,
        agent: &RunProcess,
        log: &SupervisorLog,
    ) -> Result<()> {
        let started = *self.agent_seen.get_or_insert_with(Instant::now);
        let wait = cmux.prompt_wait();
        if self.receipt_seen {
            // `receipt_observed` ends the attention by itself.
            self.prompt_hash = None;
            return Ok(());
        }
        if self.idle_marker.exists()
            || !process_alive(agent.pid)
            || queue.has_unclosed_worker_question(&run.id)?
        {
            // The agent finished a response, is gone, or stopped at an ask
            // that waits for its answer: no dialog holds it now, and a
            // recorded one must not stay an attention.
            return self.clear_prompt(queue, run, log);
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
        let screen = match cmux.capture(&self.workspace) {
            Ok(screen) => screen,
            Err(error) => {
                log.note(&format!(
                    "screen of {} could not be read for a dialog: {error:#}",
                    run.id
                ));
                return Ok(());
            }
        };
        match detect_prompt(&screen) {
            Some(kind) => {
                let excerpt = screen_tail(&screen, PROMPT_EXCERPT_LINES);
                let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
                if self.prompt_hash.as_deref() != Some(hash.as_str()) {
                    queue.record_runtime_event(
                        &run.id,
                        "prompt_waiting",
                        json!({
                            "workspace_id": self.workspace,
                            "excerpt": excerpt,
                            "screen_hash": hash,
                            "prompt": kind.as_str(),
                        }),
                    )?;
                    log.note(&format!(
                        "run {} waits at a {} dialog; answer the prompt in workspace {}",
                        run.id,
                        kind.as_str(),
                        self.workspace
                    ));
                    self.prompt_hash = Some(hash);
                }
            }
            None => self.clear_prompt(queue, run, log)?,
        }
        Ok(())
    }

    /// Type the answer of each answered `worker_question` of the run into
    /// the worker's terminal, prefixed `answer to ask <id>:`, once the worker
    /// went idle after asking (its idle marker is no older than the ask, to
    /// the second), then close the ask and record `ask_delivered` (ADR-0022
    /// decision 2). Each answer is sent at most once: a failed send records
    /// `ask_delivery_failed` and leaves the ask unclosed for the maintainer.
    fn deliver_answers(
        &mut self,
        queue: &mut SqliteQueue,
        cmux: &dyn WorkspaceBackend,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<()> {
        let answers = queue.undelivered_answers(&run.id)?;
        if answers.is_empty() {
            return Ok(());
        }
        let idle_at = match fs::metadata(&self.idle_marker) {
            Ok(meta) => unix_seconds(meta.modified()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let failed: Vec<i64> = queue
            .run_events(&run.id)?
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
            match cmux.send_text(&self.workspace, &text) {
                // Sent: failing to record it must not cost the live run its
                // lease, so it is only noted (the ask then shows unclosed).
                Ok(()) => match queue.ask_delivered(ask.id, &self.workspace) {
                    Ok(_) => log.note(&format!(
                        "answer of ask {} sent to run {} in workspace {}",
                        ask.id, run.id, self.workspace
                    )),
                    Err(error) => log.note(&format!(
                        "answer of ask {} was sent to run {} but could not be recorded: {error:#}",
                        ask.id, run.id
                    )),
                },
                Err(error) => {
                    queue.record_runtime_event(
                        &run.id,
                        "ask_delivery_failed",
                        json!({
                            "ask_id": ask.id,
                            "workspace_id": self.workspace,
                            "error": format!("{error:#}"),
                        }),
                    )?;
                    log.note(&format!(
                        "answer of ask {} could not be sent to run {} in workspace {}: {error:#}; it is left to the maintainer",
                        ask.id, run.id, self.workspace
                    ));
                }
            }
        }
        Ok(())
    }

    /// Record `prompt_cleared` if a dialog is recorded and not cleared yet.
    fn clear_prompt(
        &mut self,
        queue: &mut SqliteQueue,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<()> {
        if self.prompt_hash.take().is_some() {
            queue.record_runtime_event(
                &run.id,
                "prompt_cleared",
                json!({"workspace_id": self.workspace}),
            )?;
            log.note(&format!("dialog of {} is gone", run.id));
        }
        Ok(())
    }
}

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

/// How many resumes of the run were started, for a resume error recorded
/// outside the resume watch; unreadable counts as the last attempt.
fn resume_attempts(queue: &SqliteQueue, id: &str) -> usize {
    queue
        .run_events(id)
        .map(|events| events.iter().filter(|e| e.kind == "resume_started").count())
        .unwrap_or(MAX_RESUME_ATTEMPTS)
}

/// What the resolution request tells a resumed session.
struct ResumeRequest {
    /// The `main` head the session rebases onto.
    main: String,
    reason: String,
    /// The run came from validation's `evidence_missing`, not a landing:
    /// the session adds evidence instead of rebasing.
    evidence_missing: bool,
}

/// Why the run waits for a session: the reason of its latest
/// `integration_deferred` / `integration_error` / `evidence_missing` event
/// (a runtime error since, such as a failed resume, may have replaced
/// `last_error`), else `last_error`; and whether that event was
/// `evidence_missing` (or a landing deferred for missing evidence, whose
/// payload names the `checks`).
fn resume_reason(queue: &SqliteQueue, run: &TaskRun) -> Result<(Option<String>, bool)> {
    let events = queue.run_events(&run.id)?;
    let parked = events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            "integration_deferred" | "integration_error" | "evidence_missing"
        )
    });
    let reason = parked
        .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| run.last_error.clone());
    let evidence =
        parked.is_some_and(|e| e.kind == "evidence_missing" || e.payload.get("checks").is_some());
    Ok((reason, evidence))
}

/// The tasks landed on `main` since the run's base, oldest first, from the
/// `Dagq-Task` trailers, each with its integrated run's receipt summary.
fn landed_since(
    queue: &mut SqliteQueue,
    repository: &GitRepository,
    run: &TaskRun,
    main: &str,
) -> Result<Vec<PredecessorSummary>> {
    let mut landed = Vec::new();
    for task_id in repository.landed_task_ids(&run.base_commit, main)? {
        let Ok(detail) = queue.show(task_id) else {
            continue;
        };
        let integrated_run = detail
            .runs
            .iter()
            .rev()
            .find(|r| r.status == RunStatus::Integrated)
            .cloned();
        landed.push(PredecessorSummary::from_predecessor(&Predecessor {
            task: detail.task,
            integrated_run,
        }));
    }
    Ok(landed)
}

/// The fixed resolution request the supervisor types into a resumed
/// session (ADR-0019 decision 1), one instruction per line; the backend
/// sends it as one line.
fn resume_request(
    task: &Task,
    run: &TaskRun,
    request: &ResumeRequest,
    landed: &[PredecessorSummary],
) -> Result<String> {
    let receipt = run.receipt_path.as_ref().context("missing receipt path")?;
    let mut lines = vec![if request.evidence_missing {
        format!(
            "dagq: the supervisor's validation of run {} (task {}) found required evidence missing from the receipt, so the run is needs_session.",
            run.id, task.id
        )
    } else {
        format!(
            "dagq: integrate could not land run {} (task {}) and returned needs_session.",
            run.id, task.id
        )
    }];
    lines.push(format!("Reason: {}", request.reason));
    lines.push(format!(
        "main is now {} (your base commit was {}).",
        request.main, run.base_commit
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
    let verify = serde_json::to_string(&task.verification_commands)?;
    if request.evidence_missing {
        lines.push(
            "1. Run the checks the reason names as missing and write their evidence into the receipt."
                .to_owned(),
        );
        lines.push(format!(
            "2. If that changes files, commit them and rerun the verification commands {verify}."
        ));
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
    head: Option<String>,
    /// The session did not exit within the exit timeout of `/exit`: it is
    /// let go (still running, its workspace kept) so the slot and the lease
    /// are not held forever.
    exit_timed_out: bool,
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
    fn rewritten_receipt(&self) -> Option<Receipt> {
        let modified = fs::metadata(&self.receipt_path).ok()?.modified().ok()?;
        if modified <= self.started_at {
            return None;
        }
        Receipt::parse(&fs::read_to_string(&self.receipt_path).ok()?).ok()
    }

    /// `head` is the worktree's HEAD when the worktree is clean, `None`
    /// otherwise: a resolved receipt must name a clean head.
    fn verdict(&self, run: &TaskRun, head: Option<&str>) -> ResumeOutcome {
        match self.rewritten_receipt() {
            Some(receipt) if receipt.run_id != run.id => ResumeOutcome::Unresolved,
            Some(receipt) if receipt.result == ReceiptResult::Failed => ResumeOutcome::Failed(
                format!("session reported the run as failed: {}", receipt.summary),
            ),
            Some(receipt)
                if head.is_some_and(|head| head == receipt.commit.to_ascii_lowercase())
                    && receipt.missing_evidence(&self.required_evidence).is_empty() =>
            {
                ResumeOutcome::Resolved
            }
            _ => ResumeOutcome::Unresolved,
        }
    }

    /// One observation; `Some` once the wrapper exited.
    fn poll(
        &mut self,
        queue: &mut SqliteQueue,
        cmux: &dyn WorkspaceBackend,
        repository: &GitRepository,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<Option<ResumeVerdict>> {
        let processes = queue.processes(&run.id)?;
        let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") else {
            let timeout = cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "resumed session's wrapper did not register within {} seconds",
                timeout.as_secs()
            );
            return Ok(None);
        };
        let worktree = Path::new(run.worktree_path.as_ref().context("missing worktree")?);
        if wrapper.exited_at.is_some() {
            match cmux.capture(&self.workspace) {
                Ok(screen) => fs::write(
                    self.run_dir
                        .join(format!("terminal-resume-{}.txt", self.attempt)),
                    screen,
                )?,
                Err(error) => queue.record_runtime_event(
                    &run.id,
                    "screen_capture_failed",
                    json!({"error": format!("{error:#}")}),
                )?,
            }
            let head = repository.head(worktree).ok();
            let clean = repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty());
            return Ok(Some(ResumeVerdict {
                kind: self.verdict(run, head.as_deref().filter(|_| clean)),
                head,
                exit_timed_out: false,
            }));
        }
        ensure!(
            unix_time() - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS,
            "resumed session's wrapper heartbeat expired; session may still be alive"
        );
        let Some((sent, sent_at)) = self.message_sent else {
            if processes.iter().any(|p| p.role == "agent") {
                let seen = *self.agent_seen.get_or_insert_with(Instant::now);
                if seen.elapsed() >= cmux.resume_prompt_delay() {
                    cmux.send_text(&self.workspace, &self.message)?;
                    self.message_sent = Some((Instant::now(), SystemTime::now()));
                    log.note(&format!(
                        "resolution request sent to run {} in workspace {}",
                        run.id, self.workspace
                    ));
                }
            }
            return Ok(None);
        };
        match self.exit_requested {
            None => {
                let head = repository.head(worktree)?;
                let clean = repository.status(worktree)?.trim().is_empty();
                // Resolved (or failed) and idle after the receipt; or idle
                // after the request with no such receipt, which a session
                // that could not resolve it (or stopped at a question)
                // never ends by itself; or no idle at all within the
                // resume timeout (a lost request, a dialog).
                let why = match self.verdict(run, Some(head.as_str()).filter(|_| clean)) {
                    ResumeOutcome::Unresolved if marker_newer_than(&self.idle_marker, sent_at)? => {
                        Some("went idle without a resolving receipt")
                    }
                    ResumeOutcome::Unresolved => None,
                    _ => idle_after_receipt(&self.receipt_path, &self.idle_marker)?
                        .map(|_| "rewrote its receipt and went idle"),
                }
                .or_else(|| {
                    (sent.elapsed() >= cmux.resume_timeout())
                        .then_some("did not finish within the resume timeout")
                });
                if let Some(why) = why {
                    // Ask once, the way the maintainer would; never kill the session.
                    cmux.send_exit(&self.workspace)?;
                    log.note(&format!(
                        "resumed session of {} {why} (head {head}); exit requested",
                        run.id
                    ));
                    self.exit_requested = Some(Instant::now());
                }
            }
            Some(requested) if requested.elapsed() >= cmux.exit_timeout() => {
                // /exit is not resent (it could pick a dialog's option).
                log.note(&format!(
                    "resumed session of {} did not exit within {}s of the exit request; letting it go as unresolved (its workspace {} is kept)",
                    run.id,
                    cmux.exit_timeout().as_secs(),
                    self.workspace
                ));
                return Ok(Some(ResumeVerdict {
                    kind: ResumeOutcome::Unresolved,
                    head: repository.head(worktree).ok(),
                    exit_timed_out: true,
                }));
            }
            Some(_) => (),
        }
        Ok(None)
    }
}

/// Whether the marker exists and was modified after `since`.
fn marker_newer_than(marker: &Path, since: SystemTime) -> Result<bool> {
    match fs::metadata(marker) {
        Ok(meta) => Ok(meta.modified()? > since),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect idle marker"),
    }
}

/// Evidence that the agent finished a response after publishing the receipt: an
/// idle marker written by the provider's stop hook no older than the receipt.
/// Markers from earlier turns (for example a question to the maintainer) do not count.
fn idle_after_receipt(receipt: &Path, marker: &Path) -> Result<Option<Value>> {
    let marker_meta = match fs::metadata(marker) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect idle marker"),
    };
    let receipt_modified = fs::metadata(receipt)?.modified()?;
    let marker_modified = marker_meta.modified()?;
    if marker_modified < receipt_modified {
        return Ok(None);
    }
    let hook: Value = serde_json::from_str(&fs::read_to_string(marker)?).unwrap_or(Value::Null);
    let field = |name: &str| hook.get(name).cloned().unwrap_or(Value::Null);
    Ok(Some(json!({
        "marker_path": path_text(marker)?,
        "marker_modified": unix_seconds(marker_modified),
        "receipt_modified": unix_seconds(receipt_modified),
        "hook_event_name": field("hook_event_name"),
        "session_id": field("session_id"),
        "stop_hook_active": field("stop_hook_active"),
    })))
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// The expanded `[run.env]` of the repository's `dagq.toml` for the run in
/// `run_dir` (ADR-0023 decision 3). The file is read from the main checkout,
/// since `integrate` may be called from any worktree of the repository.
fn run_env(repository: &GitRepository, db: &Path, run_dir: &Path) -> Result<Vec<(String, String)>> {
    let queue_dir = db.parent().context("queue database has no directory")?;
    load_run_env(&main_checkout(repository), queue_dir, run_dir)
}

/// The main worktree of the repository: the parent of a `.git` common
/// directory, or the inspected root for a bare common directory.
fn main_checkout(repository: &GitRepository) -> PathBuf {
    match repository.common_dir.parent() {
        Some(parent) if repository.common_dir.file_name() == Some(".git".as_ref()) => {
            parent.to_path_buf()
        }
        _ => repository.root.clone(),
    }
}

/// Cross-check the agent's receipt against Git on a thread with its own
/// connection. The task's verification commands do not run here: `integrate`
/// runs them once, after its rebase (ADR-0023 decision 1). Rejections become a
/// `Validation` that is not accepted; only errors in the checks themselves
/// propagate, leaving the run in `validating`.
fn spawn_validation(
    db: PathBuf,
    repository: GitRepository,
    run: TaskRun,
    log: SupervisorLog,
) -> thread::JoinHandle<Result<Validation>> {
    thread::spawn(move || {
        let mut queue = SqliteQueue::open(&db)?;
        let task = queue.show(run.task_id)?.task;
        let checked = check_receipt(&repository, &task, &run)?;
        Ok(match checked {
            Ok((receipt, commit)) => Validation {
                accepted: true,
                result_commit: Some(commit),
                reason: None,
                receipt: serde_json::to_value(receipt)?,
                evidence_missing: Vec::new(),
            },
            Err(rejection) => {
                log.note(&format!("run {} rejected: {}", run.id, rejection.reason));
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
                }
            }
        })
    })
}

/// Close the cmux workspace of an accepted run. The worktree and branch stay
/// until integration. A close failure is recorded but does not change the run
/// status; `workspace_closed_at` stays null so nothing treats it as cleaned.
fn close_workspace(
    queue: &mut SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    token: &str,
    run: &TaskRun,
    log: &SupervisorLog,
) -> Result<TaskRun> {
    let workspace = run.workspace_id.as_ref().context("missing workspace")?;
    match cmux.close(workspace) {
        Ok(()) => queue.workspace_closed(&run.id, token),
        Err(error) => {
            let message = format!("workspace {workspace} could not be closed: {error:#}");
            log.note(&format!("run {}: {message}", run.id));
            queue.cleanup_failed(&run.id, token, &message)
        }
    }
}

struct Rejection {
    reason: String,
    commit: Option<String>,
    receipt: Option<Receipt>,
    /// The task's required checks the receipt does not back, when that is
    /// all that is wrong: the run waits for a session instead of failing.
    evidence_missing: Vec<EvidenceCheck>,
}

fn check_receipt(
    repository: &GitRepository,
    task: &Task,
    run: &TaskRun,
) -> Result<std::result::Result<(Receipt, String), Rejection>> {
    let reject = |reason: String, commit: Option<String>, receipt: Option<Receipt>| {
        Ok(Err(Rejection {
            reason,
            commit,
            receipt,
            evidence_missing: Vec::new(),
        }))
    };
    let receipt_path = Path::new(run.receipt_path.as_ref().context("missing receipt path")?);
    let text = match fs::read_to_string(receipt_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(
                format!("receipt was not submitted at {}", receipt_path.display()),
                None,
                None,
            );
        }
        Err(error) => return Err(error).context("read receipt"),
    };
    let receipt = match Receipt::parse(&text) {
        Ok(receipt) => receipt,
        Err(error) => return reject(format!("{error:#}"), None, None),
    };
    if let Err(error) = receipt.check_requiring(&run.id, &task.required_evidence) {
        return reject(format!("{error:#}"), None, Some(receipt));
    }
    // The commit must be the head of the run branch, checked out in the worktree,
    // and new work on top of the base commit.
    let worktree = Path::new(run.worktree_path.as_ref().context("missing worktree")?);
    let branch = run.branch.as_ref().context("missing branch")?;
    let expected_ref = format!("refs/heads/{branch}");
    match repository.current_branch(worktree)? {
        Some(current) if current == expected_ref => (),
        current => {
            return reject(
                format!(
                    "worktree is on {} instead of {expected_ref}",
                    current.as_deref().unwrap_or("a detached HEAD")
                ),
                None,
                Some(receipt),
            );
        }
    }
    let head = repository.head(worktree)?;
    if head != receipt.commit.to_ascii_lowercase() {
        return reject(
            format!(
                "receipt commit {} is not the head of {branch} ({head})",
                receipt.commit
            ),
            None,
            Some(receipt),
        );
    }
    let commit = head;
    if commit == run.base_commit {
        return reject(
            format!("no commit was made on top of base {}", run.base_commit),
            Some(commit),
            Some(receipt),
        );
    }
    if !repository.is_ancestor(&run.base_commit, &commit)? {
        return reject(
            format!(
                "commit {commit} does not descend from base {}",
                run.base_commit
            ),
            Some(commit),
            Some(receipt),
        );
    }
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return reject(
            format!("worktree is not clean:\n{}", status.trim_end()),
            Some(commit),
            Some(receipt),
        );
    }
    // Checked last: only a run that is otherwise sound waits for a session
    // to add the evidence (ADR-0019 decision 5).
    let missing = receipt.missing_evidence(&task.required_evidence);
    if !missing.is_empty() {
        return Ok(Err(Rejection {
            reason: evidence_missing_reason(&missing),
            commit: Some(commit),
            receipt: Some(receipt),
            evidence_missing: missing,
        }));
    }
    Ok(Ok((receipt, commit)))
}

/// Which run `integrate` lands.
#[derive(Debug, Clone, Copy)]
pub enum IntegrateTarget {
    /// The task's run that awaits integration or comes back from a session.
    Task(i64),
    /// The oldest run awaiting integration by validation time (FIFO).
    Next,
}

/// Land one validated run on `main`: take the single integration slot,
/// rebase the run worktree onto the current `refs/heads/main`, re-validate
/// (receipt, descent from main, clean tree) and run the verification
/// commands, the only run of them for the commit (ADR-0023), squash
/// the tree into one commit with `Dagq-Task` / `Dagq-Run` trailers and
/// fast-forward `main` to it. Never a merge commit, never a fast-forward of
/// the run branch itself. A conflict or a failed re-validation parks the run
/// as `needs_session` for a resumed session to fix; a rewritten receipt that
/// reports `failed` ends the run. `repo` is any checkout of the repository
/// the queue is bound to. After a landing, `main` is pushed to `origin`
/// through `remote` (ADR-0019 decision 3); `None` is `--no-push`. The push
/// never changes the landing: its outcome is an event and the `push` of the
/// result. The landed receipt's `follow_ups` become draft tasks
/// (ADR-0019 decision 4), listed as the result's `follow_ups`.
pub fn integrate(
    db: &Path,
    target: IntegrateTarget,
    repo: &Path,
    remote: Option<&dyn MainRemote>,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let repository = GitRepository::inspect(repo)?;
    let common_dir = path_text(&repository.common_dir)?;
    let bound = queue
        .repository_binding()?
        .context("queue is not bound to a repository; no run was supervised")?;
    ensure!(
        bound == common_dir,
        "{} belongs to {common_dir}, but the queue is bound to {bound}",
        repo.display()
    );
    let run = match target {
        IntegrateTarget::Task(task_id) => {
            let detail = queue.show(task_id)?;
            if let Some(busy) = detail
                .runs
                .iter()
                .find(|r| r.status == RunStatus::Integrating)
            {
                bail!(
                    "run {} of task {task_id} is already integrating (see doctor if it is stuck)",
                    busy.id
                );
            }
            detail
                .runs
                .iter()
                .find(|r| {
                    matches!(
                        r.status,
                        RunStatus::AwaitingIntegration | RunStatus::NeedsSession
                    )
                })
                .cloned()
                .with_context(|| {
                    format!(
                        "task {task_id} ({}) has no run awaiting integration or a session",
                        detail.task.status.as_str()
                    )
                })?
        }
        IntegrateTarget::Next => match queue.next_awaiting_integration()? {
            Some(run) => run,
            None => return Ok(serde_json::to_value(IntegrationOutcome::NoRunAwaiting)?),
        },
    };
    // The call is the approval to land (ADR-0016 decision 5): a run it
    // parks as `needs_session` is landed by the supervisor once a resumed
    // session resolved it (ADR-0019 decision 1).
    if !queue.has_run_event(&run.id, "integration_approved")? {
        queue.record_runtime_event(
            &run.id,
            "integration_approved",
            json!({"status": run.status.as_str(), "pid": std::process::id(), "push": remote.is_some()}),
        )?;
    }
    let previous = run.status;
    let token = Uuid::new_v4().to_string();
    let main = repository.main_head()?;
    let run = queue.begin_integration(&run.id, &token, &main)?;
    let heartbeat = Heartbeat::start(db.clone(), token.clone());
    let outcome = land_integrating(
        &mut queue,
        &db,
        &repository,
        &run,
        previous,
        &main,
        &token,
        &common_dir,
        remote,
    )?;
    drop(heartbeat); // Stops the lease heartbeat before this process reports.
    Ok(serde_json::to_value(outcome)?)
}

/// Land a run that holds the integration slot under `token` (see
/// [`integrate`]) and record the outcome; shared by `integrate` and by the
/// supervisor landing an approved run it resumed. An error before `main`
/// moved gives the slot back and returns the run to `previous`.
#[allow(clippy::too_many_arguments)]
fn land_integrating(
    queue: &mut SqliteQueue,
    db: &Path,
    repository: &GitRepository,
    run: &TaskRun,
    previous: RunStatus,
    main: &str,
    token: &str,
    common_dir: &str,
    remote: Option<&dyn MainRemote>,
) -> Result<IntegrationOutcome> {
    let task = queue.show(run.task_id)?.task;
    eprintln!(
        "run {} integrating task {} onto main {main}",
        run.id, run.task_id
    );
    let verdict = match land(queue, db, repository, &task, run, main) {
        Ok(verdict) => verdict,
        Err(error) => {
            // Nothing reached main: give the slot back and keep the run where it was.
            let message = format!("integration stopped before main moved: {error:#}");
            if let Err(record) =
                queue.abort_integration(&run.id, token, previous.as_str(), &message)
            {
                eprintln!("run {}: could not record the error: {record:#}", run.id);
            }
            return Err(error.context(format!("run {} returned to {}", run.id, previous.as_str())));
        }
    };
    Ok(match verdict {
        Verdict::Landed(landing, proposed) => {
            let verification_skipped = landing.verification_skipped;
            let (task, run) = queue
                .finish_integration(&run.id, token, &landing, common_dir)
                .with_context(|| {
                    format!(
                        "main advanced to {} but run {} could not be completed; inspect show and doctor",
                        landing.commit, run.id
                    )
                })?;
            eprintln!(
                "task {} landed as {} on main; run {} integrated",
                task.id, landing.commit, run.id
            );
            remove_landed_worktree(queue, repository, &run);
            let push = push_main(queue, remote, &run.id, &landing.commit);
            let follow_ups = register_follow_ups(queue, &task, &run.id, proposed.as_ref());
            IntegrationOutcome::Integrated {
                task: Box::new(task),
                run: Box::new(run),
                verification_skipped,
                push: Box::new(push),
                follow_ups,
            }
        }
        Verdict::Deferred { reason, mut detail } => {
            eprintln!("run {} needs a session: {reason}", run.id);
            // How many more times the supervisor resumes it (ADR-0019); the
            // event is the maintainer's only once none are left.
            detail["resumes_left"] =
                json!(MAX_RESUME_ATTEMPTS.saturating_sub(resume_attempts(queue, &run.id)));
            let run = queue.defer_integration(&run.id, token, &reason, detail)?;
            IntegrationOutcome::NeedsSession {
                run: Box::new(run),
                main: main.to_owned(),
                reason,
            }
        }
        Verdict::ReceiptFailed { reason, receipt } => {
            eprintln!("run {} failed: {reason}", run.id);
            let run = queue.fail_integration(&run.id, token, &reason, receipt)?;
            IntegrationOutcome::Failed {
                run: Box::new(run),
                reason,
            }
        }
    })
}

/// Push the landed `main` to [`PUSH_REMOTE`] and record the outcome as
/// `push_finished`, `push_skipped` or `push_failed` on the landed run. A
/// failure to record is only reported: the landing stands either way.
fn push_main(
    queue: &SqliteQueue,
    remote: Option<&dyn MainRemote>,
    run_id: &str,
    commit: &str,
) -> PushReport {
    let skipped = |reason: &str| PushReport {
        outcome: PushResult::Skipped,
        remote: PUSH_REMOTE.to_owned(),
        error: None,
        reason: Some(reason.to_owned()),
    };
    let report = match remote {
        None => skipped("--no-push"),
        Some(remote) => match remote.has_remote(PUSH_REMOTE) {
            Ok(false) => skipped(&format!("the repository has no remote {PUSH_REMOTE}")),
            Ok(true) => match remote.push_main(PUSH_REMOTE) {
                Ok(()) => PushReport {
                    outcome: PushResult::Pushed,
                    remote: PUSH_REMOTE.to_owned(),
                    error: None,
                    reason: None,
                },
                Err(error) => failed_push(&error),
            },
            Err(error) => failed_push(&error),
        },
    };
    let (kind, payload) = match report.outcome {
        PushResult::Pushed => (
            "push_finished",
            json!({"remote": report.remote, "commit": commit}),
        ),
        PushResult::Skipped => (
            "push_skipped",
            json!({"remote": report.remote, "commit": commit, "reason": report.reason}),
        ),
        PushResult::Failed => (
            "push_failed",
            json!({"remote": report.remote, "commit": commit, "error": report.error}),
        ),
    };
    match &report.error {
        Some(error) => eprintln!("run {run_id}: push of main failed: {error}"),
        None => eprintln!("run {run_id}: {kind} ({PUSH_REMOTE})"),
    }
    if let Err(error) = queue.record_runtime_event(run_id, kind, payload) {
        eprintln!("run {run_id}: could not record {kind}: {error:#}");
    }
    report
}

fn failed_push(error: &anyhow::Error) -> PushReport {
    PushReport {
        outcome: PushResult::Failed,
        remote: PUSH_REMOTE.to_owned(),
        error: Some(format!("{error:#}")),
        reason: None,
    }
}

/// Register the landed receipt's `follow_ups` of `task`'s run `run_id` as
/// draft tasks of the task's goal (ADR-0019 decision 4): the title and
/// description as proposed, no acceptance, verification commands or
/// dependencies, and a context naming where they came from. A closed goal
/// takes no task, so the follow-up is registered without a goal and its
/// `follow_up_registered` says `goal_closed: true`. An entry whose `title`
/// is not a non-blank string or whose `description` is not a string is not
/// registered: its `follow_up_registered` has `task_id: null`, the `skipped`
/// reason and the entry itself as `follow_up`. Every event carries the
/// entry's `index`, and an entry already recorded is not looked at again, so
/// a second call for the same run adds nothing (the task and its event are
/// written one after the other, so only a failure to record between them
/// could let a later call register it twice). A registration that fails is
/// only reported: the landing stands either way. Returns what this call
/// registered.
pub fn register_follow_ups(
    queue: &mut SqliteQueue,
    task: &Task,
    run_id: &str,
    follow_ups: Option<&Value>,
) -> Vec<RegisteredFollowUp> {
    let Some(entries) = follow_ups.and_then(Value::as_array) else {
        return Vec::new();
    };
    let registered: Vec<u64> = match queue.run_events(run_id) {
        Ok(events) => events
            .iter()
            .filter(|e| e.kind == "follow_up_registered")
            .filter_map(|e| e.payload["index"].as_u64())
            .collect(),
        Err(error) => {
            eprintln!("run {run_id}: follow_ups not registered: {error:#}");
            return Vec::new();
        }
    };
    let goal_closed = match task.goal_id {
        Some(goal_id) => match queue.show_goal(goal_id) {
            Ok(detail) => detail.closed,
            Err(error) => {
                eprintln!("run {run_id}: follow_ups not registered: {error:#}");
                return Vec::new();
            }
        },
        None => false,
    };
    let mut added = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if registered.contains(&(index as u64)) {
            continue;
        }
        let title = entry["title"].as_str().map(str::trim).unwrap_or_default();
        let description = entry["description"].as_str();
        let skipped = if title.is_empty() {
            Some("title is not a non-blank string")
        } else if description.is_none() {
            Some("description is not a string")
        } else {
            None
        };
        if let Some(reason) = skipped {
            eprintln!("run {run_id}: follow_up {index} was not registered: {reason}");
            let payload = json!({
                "task_id": null,
                "title": entry["title"],
                "index": index,
                "skipped": reason,
                "follow_up": entry,
            });
            if let Err(error) = queue.record_runtime_event(run_id, "follow_up_registered", payload)
            {
                eprintln!("run {run_id}: could not record follow_up_registered: {error:#}");
            }
            continue;
        }
        let new = NewTask {
            title: title.to_owned(),
            description: description.unwrap_or_default().to_owned(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            dependencies: Vec::new(),
            goal_id: task.goal_id.filter(|_| !goal_closed),
            context: format!(
                "task {}（{}）の run {run_id} の receipt が提案した follow_up",
                task.id, task.title
            ),
        };
        let created = match queue.add(new) {
            Ok(created) => created,
            Err(error) => {
                eprintln!("run {run_id}: follow_up {title:?} was not registered: {error:#}");
                continue;
            }
        };
        let mut payload = json!({"task_id": created.id, "title": created.title, "index": index});
        if goal_closed {
            payload["goal_closed"] = json!(true);
        }
        if let Err(error) = queue.record_runtime_event(run_id, "follow_up_registered", payload) {
            eprintln!("run {run_id}: could not record follow_up_registered: {error:#}");
        }
        eprintln!(
            "run {run_id}: follow_up {:?} registered as draft task {}",
            created.title, created.id
        );
        added.push(RegisteredFollowUp {
            task_id: created.id,
            title: created.title,
        });
    }
    added
}

enum Verdict {
    /// Landed with the receipt's `follow_ups`.
    Landed(Landing, Option<Value>),
    /// Re-validation did not pass; the worktree is left for a session.
    Deferred { reason: String, detail: Value },
    /// The session's rewritten receipt reports `failed`; `receipt` is its
    /// JSON, kept with the `integration_failed` event.
    ReceiptFailed { reason: String, receipt: Value },
}

/// Rebase, re-validate and land one run. `Ok(Deferred)` and
/// `Ok(ReceiptFailed)` are verdicts on the run; `Err` is a failure of the
/// landing itself (Git, files) before `main` moved.
fn land(
    queue: &mut SqliteQueue,
    db: &Path,
    repository: &GitRepository,
    task: &Task,
    run: &TaskRun,
    main: &str,
) -> Result<Verdict> {
    let defer = |reason: String, detail: Value| Ok(Verdict::Deferred { reason, detail });
    let worktree = Path::new(run.worktree_path.as_ref().context("missing worktree")?);
    ensure!(
        worktree.is_dir(),
        "worktree {} is missing",
        worktree.display()
    );
    // A worktree whose queue directory moved is still found through its own
    // `.git` file, but the repository's record of it points at the old path
    // until repaired, and removing it after landing would fail (ADR-0017).
    repository.repair_worktree(worktree)?;
    let branch = run.branch.as_ref().context("missing branch")?;
    let run_dir = Path::new(run.run_dir.as_ref().context("missing run directory")?);
    // A rebase left behind by a crashed landing or an unfinished session is undone first.
    if repository.rebase_in_progress(worktree)? {
        repository.rebase_abort(worktree)?;
        queue.record_runtime_event(
            &run.id,
            "integration_rebase_aborted",
            json!({"reason": "a rebase was left in progress"}),
        )?;
    }
    let expected_ref = format!("refs/heads/{branch}");
    match repository.current_branch(worktree)? {
        Some(current) if current == expected_ref => (),
        current => {
            return defer(
                format!(
                    "worktree is on {} instead of {expected_ref}",
                    current.as_deref().unwrap_or("a detached HEAD")
                ),
                json!({}),
            );
        }
    }
    let head = repository.head(worktree)?;
    // The receipt must describe this head: the validated one for a fresh run,
    // the one the session rewrote after resolving otherwise. A stale receipt
    // means the session is not done.
    let receipt_path = Path::new(run.receipt_path.as_ref().context("missing receipt path")?);
    let receipt = match fs::read_to_string(receipt_path) {
        Ok(text) => match Receipt::parse(&text) {
            Ok(receipt) => receipt,
            Err(error) => return defer(format!("{error:#}"), json!({})),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return defer(
                format!("receipt is missing at {}", receipt_path.display()),
                json!({}),
            );
        }
        Err(error) => return Err(error).context("read receipt"),
    };
    if receipt.result == ReceiptResult::Failed {
        return Ok(Verdict::ReceiptFailed {
            reason: format!("session reported the run as failed: {}", receipt.summary),
            receipt: serde_json::to_value(&receipt)?,
        });
    }
    if let Err(error) = receipt.check_requiring(&run.id, &task.required_evidence) {
        return defer(format!("{error:#}"), json!({}));
    }
    // A resumed session may have come back without the evidence it was
    // asked for; `checks` tells the next resume to ask for it again.
    let missing = receipt.missing_evidence(&task.required_evidence);
    if !missing.is_empty() {
        return defer(
            evidence_missing_reason(&missing),
            json!({"checks": missing}),
        );
    }
    // The receipt read here is the one that lands (or the one a session
    // rewrote after resolving), so it is recorded whatever happens next: the
    // DB otherwise keeps only the receipt seen at validation time.
    queue.record_runtime_event(
        &run.id,
        "integration_receipt",
        json!({
            "main": main,
            "commit": receipt.commit,
            "receipt": serde_json::to_value(&receipt)?,
        }),
    )?;
    if head != receipt.commit.to_ascii_lowercase() {
        return defer(
            format!(
                "receipt commit {} is not the head of {branch} ({head}); rerun the verification commands and rewrite the receipt for the current head",
                receipt.commit
            ),
            json!({"head": head}),
        );
    }
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            format!("worktree is not clean:\n{}", status.trim_end()),
            json!({"head": head}),
        );
    }
    // Onto the current main. A no-op when the run already sits on it.
    if let Err(output) = repository.rebase(worktree, main)? {
        let conflicts = repository.conflicted_files(worktree).unwrap_or_default();
        if repository.rebase_in_progress(worktree)? {
            repository.rebase_abort(worktree)?;
        }
        return defer(
            format!(
                "rebase onto main {main} conflicted in {}; resolve it in the worktree (git rebase {main}), rerun the verification commands, and rewrite the receipt with the new head",
                if conflicts.is_empty() {
                    "the run branch".to_owned()
                } else {
                    conflicts.join(", ")
                }
            ),
            json!({
                "main": main,
                "head": head,
                "conflicts": conflicts,
                "output_tail": tail(&output, 2000),
                "aborted": true,
            }),
        );
    }
    let rebased = repository.head(worktree)?;
    queue.record_runtime_event(
        &run.id,
        "integration_rebased",
        json!({"main": main, "head_before": head, "head_after": rebased}),
    )?;
    if rebased == main {
        return defer(
            format!(
                "no commit remains on top of main {main} after the rebase; if the change is no longer needed, write a failed receipt with the reason"
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    ensure!(
        repository.is_ancestor(main, &rebased)?,
        "rebased head {rebased} does not descend from main {main}"
    );
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            format!(
                "worktree is not clean after the rebase:\n{}",
                status.trim_end()
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    // The task's verification commands run here, once per commit, on the
    // rebased tree: validation only checks the receipt (ADR-0023 decision 1).
    let commands = &task.verification_commands;
    let run_env = if commands.is_empty() {
        Vec::new()
    } else {
        run_env(repository, db, run_dir)?
    };
    for (index, command) in commands.iter().enumerate() {
        let log = run_dir.join(format!("integrate-verify-{}.log", index + 1));
        let status = run_shell_to_log(command, worktree, &run_env, &log)?;
        let exit_code = status.code().unwrap_or(128);
        let output = fs::read_to_string(&log).unwrap_or_default();
        queue.record_runtime_event(
            &run.id,
            "verification_command",
            json!({
                "phase": "integration",
                "index": index + 1,
                "command": command,
                "exit_code": exit_code,
                "log_path": path_text(&log)?,
                "output_tail": tail(&output, 2000),
            }),
        )?;
        if exit_code != 0 {
            return defer(
                format!(
                    "verification command {command:?} exited with {exit_code} after the rebase onto {main}; see {}",
                    log.display()
                ),
                json!({"main": main, "head": rebased, "command": command, "exit_code": exit_code}),
            );
        }
    }
    // One commit on main with the rebased tree; the run's own history stays
    // reachable under refs/dagq/runs/<run-id>.
    let paragraphs = commit_message(task, run, &receipt);
    let tree = repository.tree_of(&rebased)?;
    let commit = repository.commit_tree(&tree, main, &paragraphs)?;
    let history_ref = format!("refs/dagq/runs/{}", run.id);
    repository.update_ref(&history_ref, &rebased)?;
    repository.advance_main(main, &commit)?;
    Ok(Verdict::Landed(
        Landing {
            commit,
            source_commit: rebased,
            main_before: main.to_owned(),
            history_ref,
            message: paragraphs.join("\n\n"),
            verification_skipped: false,
        },
        receipt.follow_ups,
    ))
}

/// Title, the receipt's summary, and the trailers that tie the commit to
/// the queue, as paragraphs.
fn commit_message(task: &Task, run: &TaskRun, receipt: &Receipt) -> Vec<String> {
    let mut paragraphs = vec![task.title.trim().to_owned()];
    let summary = receipt.summary.trim();
    if !summary.is_empty() {
        paragraphs.push(summary.to_owned());
    }
    paragraphs.push(format!("Dagq-Task: {}\nDagq-Run: {}", task.id, run.id));
    paragraphs
}

/// Drop the landed run's worktree and branch. The result is already on
/// `main` and under the history ref, so a failure here is only recorded.
fn remove_landed_worktree(queue: &mut SqliteQueue, repository: &GitRepository, run: &TaskRun) {
    let (Some(worktree), Some(branch)) = (&run.worktree_path, &run.branch) else {
        return;
    };
    let recorded = match repository.remove_worktree_and_branch(Path::new(worktree), branch) {
        Ok(()) => queue.record_runtime_event(
            &run.id,
            "worktree_removed",
            json!({"path": worktree, "branch": branch}),
        ),
        Err(error) => {
            let message = format!("landed worktree {worktree} could not be removed: {error:#}");
            eprintln!("run {}: {message}", run.id);
            queue.record_cleanup_failure(&run.id, &message)
        }
    };
    if let Err(error) = recorded {
        eprintln!("run {}: could not record the cleanup: {error:#}", run.id);
    }
}

/// Write the review material of the task's run that awaits integration or a
/// session to `<run_dir>/review.md` (temporary file, then rename) and report
/// where it is with the size of the diff (ADR-0016, decision 7). The file holds the
/// task, its goal, the receipt, the commits, the diffstat and the full diff
/// `<base>...<head>`, `base` being the run's base commit and `head` the
/// receipt's commit. When a session already rebased `head` onto the current
/// `main`, `base` is that `main` instead, so the review does not repeat
/// what other tasks landed meanwhile. The diff itself is never returned, so the maintainer
/// hands the path to a subagent instead of reading it.
pub fn review(db: &Path, task_id: i64) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let detail = queue.show(task_id)?;
    let run = detail
        .runs
        .iter()
        .find(|r| {
            matches!(
                r.status,
                RunStatus::AwaitingIntegration | RunStatus::NeedsSession
            )
        })
        .cloned()
        .with_context(|| {
            format!(
                "task {task_id} ({}) has no run awaiting integration or a session",
                detail.task.status.as_str()
            )
        })?;
    let task = detail.task;
    let goal = match task.goal_id {
        Some(goal_id) => Some(queue.show_goal(goal_id)?.goal),
        None => None,
    };
    let run_dir = Path::new(run.run_dir.as_ref().context("missing run directory")?);
    let receipt_path = Path::new(run.receipt_path.as_ref().context("missing receipt path")?);
    let receipt = Receipt::parse(
        &fs::read_to_string(receipt_path)
            .with_context(|| format!("read receipt {}", receipt_path.display()))?,
    )?;
    let checkout = run
        .repo_path
        .as_ref()
        .or(run.worktree_path.as_ref())
        .context("run has no repository path")?;
    let repository = GitRepository::inspect(Path::new(checkout))?;
    let head = receipt.commit.to_ascii_lowercase();
    let main = repository.main_head()?;
    let base = if main != run.base_commit && repository.is_ancestor(&main, &head)? {
        main
    } else {
        run.base_commit.clone()
    };
    let log = repository.log_oneline(&base, &head)?;
    let stat = repository.diff_stat(&base, &head)?;
    let numbers = repository.diff_numbers(&base, &head)?;
    let text = review_markdown(
        &task,
        &run,
        goal.as_ref(),
        &receipt,
        &base,
        &head,
        &log,
        &stat,
    );
    let path = run_dir.join("review.md");
    let temporary = run_dir.join(format!(".review.md.{}.tmp", std::process::id()));
    let diff = run_dir.join(format!(".review.md.{}.diff.tmp", std::process::id()));
    let written =
        write_review(&repository, &base, &head, &text, &diff, &temporary).and_then(|()| {
            fs::rename(&temporary, &path).with_context(|| format!("write {}", path.display()))
        });
    let _ = fs::remove_file(&diff);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(json!({
        "run_id": run.id,
        "task_id": task.id,
        "path": path_text(&path)?,
        "base": base,
        "head": head,
        "files_changed": numbers.files_changed,
        "insertions": numbers.insertions,
        "deletions": numbers.deletions,
    }))
}

/// Write `text` and then the full diff `<base>...<head>` as a fenced block to
/// `temporary`. Git streams the diff to the file `diff` first, as raw bytes
/// and never through memory, because the fence must be longer than any
/// backtick run in it; the file is then copied under the fence.
fn write_review(
    repository: &GitRepository,
    base: &str,
    head: &str,
    text: &str,
    diff: &Path,
    temporary: &Path,
) -> Result<()> {
    let file = fs::File::create(diff).with_context(|| format!("create {}", diff.display()))?;
    repository.diff_to(base, head, &file)?;
    drop(file);
    let (longest, last) = backtick_run_and_last_byte(diff)?;
    let fence = "`".repeat(longest.max(2) + 1);
    let mut out = BufWriter::new(
        fs::File::create(temporary).with_context(|| format!("create {}", temporary.display()))?,
    );
    writeln!(out, "{text}{fence}diff")?;
    io::copy(
        &mut fs::File::open(diff).with_context(|| format!("open {}", diff.display()))?,
        &mut out,
    )?;
    if last.is_some_and(|byte| byte != b'\n') {
        out.write_all(b"\n")?;
    }
    writeln!(out, "{fence}")?;
    out.into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()
        .with_context(|| format!("write {}", temporary.display()))
}

/// The longest run of backticks in the file and its last byte, read in chunks.
fn backtick_run_and_last_byte(path: &Path) -> Result<(usize, Option<u8>)> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let (mut longest, mut run, mut last) = (0, 0, None);
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok((longest, last));
        }
        for &byte in &buffer[..read] {
            run = if byte == b'`' { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        last = Some(buffer[read - 1]);
    }
}

/// A fenced block whose fence is longer than any backtick run in `text`,
/// so a diff of Markdown cannot close it early.
fn fenced(info: &str, text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let body = text.trim_end_matches('\n');
    if body.is_empty() {
        format!("{fence}{info}\n{fence}\n")
    } else {
        format!("{fence}{info}\n{body}\n{fence}\n")
    }
}

/// `text`, or `(none)` when it is blank.
fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text.trim_end()
    }
}

#[allow(clippy::too_many_arguments)]
fn review_markdown(
    task: &Task,
    run: &TaskRun,
    goal: Option<&Goal>,
    receipt: &Receipt,
    base: &str,
    head: &str,
    log: &str,
    stat: &str,
) -> String {
    let mut out = format!(
        "# Review of task {id}: {title}\n\n\
         - run: {run_id} ({status})\n\
         - base: {base} (run base {run_base})\n\
         - head: {head}\n\
         - branch: {branch}\n\
         - worktree: {worktree}\n\
         - verification logs: {run_dir}/integrate-verify-N.log (written when integrate runs the verification commands after its rebase)\n\n\
         ## Task\n\n\
         ### Description\n\n{description}\n\n\
         ### Acceptance\n\n{acceptance}\n\n\
         ### Verification commands\n\n{verify}\n",
        id = task.id,
        title = task.title,
        run_id = run.id,
        status = run.status.as_str(),
        run_base = run.base_commit,
        branch = run.branch.as_deref().unwrap_or("(none)"),
        worktree = run.worktree_path.as_deref().unwrap_or("(none)"),
        run_dir = run.run_dir.as_deref().unwrap_or("(none)"),
        description = or_none(&task.description),
        acceptance = or_none(&task.acceptance),
        verify = fenced("sh", &task.verification_commands.join("\n")),
    );
    if let Some(goal) = goal {
        out.push_str(&format!(
            "\n## Goal {id}: {title}\n\n\
             ### Goal acceptance\n\n{acceptance}\n\n\
             ### Goal constraints\n\n{constraints}\n",
            id = goal.id,
            title = goal.title,
            acceptance = or_none(&goal.acceptance),
            constraints = or_none(&goal.constraints),
        ));
    }
    out.push_str(&format!(
        "\n## Receipt\n\n### Summary\n\n{summary}\n",
        summary = or_none(&receipt.summary)
    ));
    for (name, check) in [
        ("Tests", &receipt.tests),
        ("E2E", &receipt.e2e),
        ("Subagent review", &receipt.subagent_review),
    ] {
        out.push_str(&format!(
            "\n### {name}: {status}\n\n{evidence}\n",
            status = check.status.as_str(),
            evidence = or_none(&check.evidence_or_reason),
        ));
    }
    let follow_ups = match &receipt.follow_ups {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| {
                format!(
                    "- {}: {}",
                    item["title"].as_str().unwrap_or("(untitled)"),
                    item["description"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "(none)".to_owned(),
    };
    out.push_str(&format!("\n### Follow-ups\n\n{follow_ups}\n"));
    out.push_str(&format!(
        "\n## Commits\n\n`git log --oneline {base}..{head}`\n\n{log}\n\
         ## Diffstat\n\n`git diff --stat {base}...{head}`\n\n{stat}\n\
         ## Diff\n\n`git diff {base}...{head}`\n\n",
        log = fenced("", log),
        stat = fenced("", stat),
    ));
    out
}

fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// What the prompt says about one direct predecessor: the task, the squash
/// commit `integrate` put on `main` for it, and the summary its agent wrote.
#[derive(Debug, Clone, Serialize)]
pub struct PredecessorSummary {
    pub task_id: i64,
    pub title: String,
    /// `result_commit` of the integrated run; `(not landed)` without one.
    pub result_commit: String,
    /// `summary` of the integrated run's receipt, whitespace collapsed;
    /// `(receipt unavailable)` when the receipt cannot be read or parsed.
    pub summary: String,
}

impl PredecessorSummary {
    /// The receipt is read where the run left it after landing (its planned
    /// `receipt_path`, else `<run_dir>/receipt.json`); a missing or
    /// unreadable one is described, never an error, so the successor still starts.
    pub fn from_predecessor(predecessor: &Predecessor) -> Self {
        let run = predecessor.integrated_run.as_ref();
        let summary = run
            .and_then(|run| {
                run.receipt_path.as_deref().map(PathBuf::from).or_else(|| {
                    run.run_dir
                        .as_deref()
                        .map(|dir| Path::new(dir).join("receipt.json"))
                })
            })
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| {
                receipt
                    .summary
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .map(|summary| {
                if summary.is_empty() {
                    "(no summary)".to_owned()
                } else {
                    summary
                }
            })
            .unwrap_or_else(|| "(receipt unavailable)".to_owned());
        Self {
            task_id: predecessor.task.id,
            title: predecessor.task.title.clone(),
            result_commit: run
                .and_then(|run| run.result_commit.clone())
                .unwrap_or_else(|| "(not landed)".to_owned()),
            summary,
        }
    }
}

/// The other tasks a worker is told are executing alongside it: of the
/// `in_progress` tasks (ID order), those sharing the task's goal, or all of
/// them when the task has no goal; the task itself is never listed.
pub fn siblings_in_progress(task: &Task, in_progress: Vec<Task>) -> Vec<Task> {
    in_progress
        .into_iter()
        .filter(|other| other.id != task.id)
        .filter(|other| task.goal_id.is_none() || other.goal_id == task.goal_id)
        .collect()
}

/// The line in the worker prompt and the resume request that asks the
/// session to stop its own background work before the receipt: a leftover
/// background shell makes Claude Code answer the supervisor's `/exit` with a
/// confirmation screen, and the exit request times out.
pub const STOP_BACKGROUND: &str = "Before writing the receipt, stop every background process you started (run_in_background shells, wait loops, watches); if any is left, /exit stops at a confirmation screen.";

/// What a worker reads before it starts, and nothing more: everything else
/// about its run is in the prompt, and reading the queue or the whole docs
/// tree only delays the first commit (goal 11, decision 4).
pub const WORKER_READING: &str = "Read first, and only: the worker section of the repository instructions (AGENTS.md), the task context below and the documents it names, the goal doc if there is one, and the predecessor summaries below. \
Do not run `dagq list` or `dagq show`, and skip the rest of the docs tree; open other files only when the task needs them.\n";

/// Text of `prompt.txt`. `goal` is the task's goal as it reads at claim
/// time, `predecessors` the task's direct dependencies and `siblings` the
/// other tasks executing at claim time (`siblings_in_progress`). The Goal,
/// Context, Predecessor and Sibling sections are always present, `none`
/// when empty, so the prompt keeps one shape whether or not a task has a
/// goal, a context, dependencies or company.
pub fn prompt(
    task: &Task,
    run: &TaskRun,
    goal: Option<&Goal>,
    predecessors: &[PredecessorSummary],
    siblings: &[Task],
) -> Result<String> {
    let receipt = run.receipt_path.as_ref().context("missing receipt path")?;
    let goal = match goal {
        None => "Goal: none, this task stands alone\n".to_owned(),
        Some(goal) => format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {id}\nGoal title: {title}\nGoal description:\n{description}\n\
             Goal acceptance:\n{acceptance}\nGoal constraints:\n{constraints}\n\
             Goal doc: {doc}\n",
            id = goal.id,
            title = goal.title,
            description = goal.description,
            acceptance = goal.acceptance,
            constraints = goal.constraints,
            doc = goal
                .doc
                .as_deref()
                .map(|doc| format!(
                    "{doc} (a path in the repository; read it for the full picture)"
                ))
                .unwrap_or_else(|| "none".to_owned()),
        ),
    };
    let context = if task.context.trim().is_empty() {
        "Context: none\n".to_owned()
    } else {
        format!(
            "Context (why this task exists and what to read first):\n{}\n",
            task.context
        )
    };
    let predecessors = if predecessors.is_empty() {
        "Predecessor tasks: none\n".to_owned()
    } else {
        let mut text =
            "Predecessor tasks (their changes are already in your base commit):\n".to_owned();
        for predecessor in predecessors {
            text.push_str(&format!(
                "- task {}: {}; result commit {}; summary: {}\n",
                predecessor.task_id,
                predecessor.title,
                predecessor.result_commit,
                predecessor.summary
            ));
        }
        text
    };
    let siblings = if siblings.is_empty() {
        "Sibling tasks in progress: none\n".to_owned()
    } else {
        let mut text =
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n"
                .to_owned();
        for other in siblings {
            text.push_str(&format!("- task {}: {}\n", other.id, other.title));
        }
        text
    };
    // Known up front, so the receipt carries it (ADR-0019 decision 5).
    let evidence = if task.required_evidence.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = task.required_evidence.iter().map(|c| c.as_str()).collect();
        format!(
            "Required evidence: {} (each must be passed with evidence in the receipt, or the run waits for a session to add it)\n",
            names.join(", ")
        )
    };
    Ok(format!(
        "You are executing dagq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree.\n\
         {reading}\
         Implement the task, run the required verification commands, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         Perform applicable unit tests, E2E, and subagent review. Record evidence or an explicit reason when not applicable.\n\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (run in the worktree):\n{verification}\n\
         {evidence}{goal}{context}{predecessors}{siblings}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\"}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title and a description, for the maintainer to register; omit it when there is none.\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and integrate reruns the verification commands itself after rebasing onto main.\n\
         When you need a decision you cannot make from the task and the repository, do not write the question to the terminal and wait: run `dagq ask --run {run_id} --kind worker_question --question '...'` in the worktree (one ask at a time, with everything you need decided in its question), report briefly that you asked, and stop. The answer arrives in this terminal as `answer to ask <id>: ...`; continue from it.\n\
         {stop_background}\n\
         After submitting, report the outcome briefly and stop; do not run /exit yourself. Once you are idle the supervisor ends the session, and the maintainer can still send /exit. A receipt does not itself end the session.\n",
        task_id = task.id,
        run_id = run.id,
        reading = WORKER_READING,
        stop_background = STOP_BACKGROUND,
        title = task.title,
        description = task.description,
        acceptance = task.acceptance,
        verification = serde_json::to_string_pretty(&task.verification_commands)?,
    ))
}

/// The initial prompt of the maintainer session that `up` opens in the
/// `[<repo>]maintainer` workspace (ADR-0016). It names the queue and the
/// supervisor's logs and points at `status`, the background `watch` and the
/// plugin's `dagq-maintain` skill, which holds the procedure; waking up again
/// after compaction or `/clear` is the plugin's SessionStart hook's job.
pub fn maintainer_prompt(db: &Path, log_dir: &Path) -> Result<String> {
    Ok(format!(
        "You are the maintainer of the dagq queue at {db}; the supervisor logs to {log_dir}.\n\
         Start with `dagq status`, then follow the dagq-maintain skill of the dagq plugin: run `dagq watch --after <cursor>` in the background and wake when it returns.\n\
         Land a run when its subagent review passes; ask the user only on doubt (acceptance mismatch, changes outside the task, review findings). Never integrate because a watch returned.\n\
         Never open the queue database directly; use the dagq CLI only. If the dagq-maintain skill is missing, say so and wait.\n",
        db = path_text(db)?,
        log_dir = path_text(log_dir)?,
    ))
}

/// The initial prompt of the inbox session that `up` opens in the
/// `[<repo>]inbox` workspace (ADR-0022): it relays each open ask to a person
/// and writes the person's answer back, deciding nothing itself.
pub fn inbox_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are the inbox of the dagq queue at {db}: you relay its asks to a person and never decide anything yourself.\n\
         Start with `dagq status --role inbox`, then run `dagq watch --role inbox --after <cursor>` in the background, wake when it returns and watch again from the cursor it returns.\n\
         On ask_opened, read the ask with `dagq asks --open --role inbox`, show the person its question and options (use AskUserQuestion when it is available), then write the person's answer with `dagq answer ID --text '<answer>'`.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = path_text(db)?,
    ))
}

/// The initial prompt of the planner session that `up` opens in the
/// `[<repo>]planner` workspace (ADR-0022): it turns a person's problems into
/// goals and tasks and closes a goal once its tasks meet the acceptance.
pub fn planner_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are the planner of the dagq queue at {db}: listen to the person's problems and turn them into goals and tasks.\n\
         Register them with the dagq skill of the dagq plugin and make the tasks ready.\n\
         When every task of a goal is completed, check their receipts against the goal's acceptance and close the goal (`dagq goal close ID --verdict achieved`).\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = path_text(db)?,
    ))
}

/// Health of one run's lease as `status` and `doctor` report it.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseHealth {
    pub pid: u32,
    pub alive: bool,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
}

/// A process that owns runs, as `status` and `doctor` report it: a resident
/// `supervise` through its registration (`registered`, with `parallel` and
/// `started_at`), or an `integrate` process through the lease it holds
/// (`registered: false`). `run_ids` are the leases carrying its token, and
/// they share its heartbeat. `stale` is a registration or lease that no
/// working process stands behind: a dead pid or a heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS`. `mode` is how `up` started it (`launchd`, or
/// `in_cmux` with the `workspace_id` it runs in); a supervisor started by
/// hand and an `integrate` process have none. Nothing here is deleted
/// automatically.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorHealth {
    pub pid: u32,
    pub alive: bool,
    pub registered: bool,
    pub mode: Option<SupervisorMode>,
    pub workspace_id: Option<String>,
    /// The `dagq` version the registered process runs; `None` for a
    /// lease holder without a registration, or a registration older than
    /// the column (ADR-0014).
    pub binary_version: Option<String>,
    pub parallel: Option<u32>,
    pub started_at: Option<i64>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
    pub run_ids: Vec<String>,
}

/// Health of one registered wrapper/agent process. `alive` is only checked
/// while the wrapper has not reported an exit, because a dead PID may be reused.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessHealth {
    pub role: String,
    pub pid: u32,
    pub alive: Option<bool>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub heartbeat_stale: bool,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// One unfinished run. `blockers` lists why `recover` would refuse it; an
/// empty list means it is recoverable now. Only this run's own lease and
/// processes count; other runs never block it.
#[derive(Debug, Clone, Serialize)]
pub struct RunHealth {
    pub run_id: String,
    pub task_id: i64,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_exists: Option<bool>,
    pub run_dir: Option<String>,
    pub run_dir_exists: Option<bool>,
    pub receipt_exists: Option<bool>,
    pub last_error: Option<String>,
    pub lease: Option<LeaseHealth>,
    pub processes: Vec<ProcessHealth>,
    pub blockers: Vec<String>,
    pub recoverable: bool,
}

impl RunHealth {
    /// The run in `doctor`'s default output: whether it can be recovered and
    /// where it is, with `blockers` counted (`blocker_count`) and the lease
    /// reduced to `lease_stale` (null without a lease).
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "task_id": self.task_id,
            "status": self.status,
            "lease_stale": self.lease.as_ref().map(|lease| lease.stale),
            "recoverable": self.recoverable,
            "blocker_count": self.blockers.len(),
            "workspace_id": self.workspace_id,
            "worktree_path": self.worktree_path,
        })
    }
}

impl SupervisorHealth {
    /// The supervisor in `doctor`'s default output, one line's worth.
    pub fn summary(&self) -> Value {
        json!({
            "pid": self.pid,
            "alive": self.alive,
            "registered": self.registered,
            "mode": self.mode,
            "workspace_id": self.workspace_id,
            "binary_version": self.binary_version,
            "heartbeat_age_secs": self.heartbeat_age_secs,
            "stale": self.stale,
            "run_ids": self.run_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisors: Vec<SupervisorHealth>,
    pub runs: Vec<RunHealth>,
}

/// Registered supervisors, lease holders and the unfinished runs with their
/// leases, without inspecting the runs' processes, plus what waits for the
/// maintainer (`attention`) and the newest event id (`cursor`) to `watch`
/// from (ADR-0016).
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// Characters of an ask's question `status` keeps before `…`.
const ASK_QUESTION_CHARS: usize = 200;

/// `status --role`: the attention narrowed to what `role` acts on
/// (ADR-0022; `None` is all of it), and every open ask with its question
/// cut to 200 characters.
pub fn status_for(db: &Path, role: Option<crate::domain::SessionRole>) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    // Read before the state it describes, so a transition in between is
    // seen again by `watch --after cursor` rather than missed.
    let cursor = queue.latest_event_id()?;
    let now = unix_time();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let lease = leases
                .iter()
                .find(|l| l.run_id == run.id)
                .map(|l| lease_health(l, now));
            json!({
                "run_id": run.id,
                "task_id": run.task_id,
                "status": run.status,
                "workspace_id": run.workspace_id,
                "worktree_path": run.worktree_path,
                "lease": lease,
            })
        })
        .collect::<Vec<_>>();
    let asks = queue
        .asks(crate::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })?
        .into_iter()
        .map(|ask| {
            json!({
                "id": ask.id,
                "kind": ask.kind,
                "question": crate::view::truncate(&ask.question, ASK_QUESTION_CHARS)
                    .unwrap_or(ask.question),
                "task_id": ask.task_id,
                "run_id": ask.run_id,
                "asked_by": ask.asked_by,
                "age_secs": now - ask.created_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors(&registrations, &leases, now),
        "runs": runs,
        "attention": crate::watch::attention(&queue, &registrations, now)?
            .into_iter()
            .filter(|a| crate::watch::for_role(&a.kind, role))
            .collect::<Vec<_>>(),
        "asks": asks,
        "cursor": cursor,
    }))
}

/// `stats` (ADR-0023 decision 5): per-run and per-goal times and the
/// thresholds crossed, derived from `run_events` by
/// [`crate::domain::stats::stats`]. The idle alert looks at the live
/// supervisors' slots now. Reads only.
pub fn stats(db: &Path, query: &crate::domain::stats::StatsQuery) -> Result<Value> {
    use crate::domain::stats::{SlotSnapshot, stats};
    let queue = SqliteQueue::open(db)?;
    let now = unix_time();
    let events = queue.all_events()?;
    let goals = queue.task_goals()?;
    let registrations = queue.supervisors()?;
    let slots: i64 = crate::watch::pulses(&registrations, now)
        .iter()
        .zip(&registrations)
        .filter(|(pulse, _)| !pulse.stale)
        .map(|(_, registration)| i64::from(registration.parallel))
        .sum();
    let executing = queue
        .active_runs()?
        .iter()
        .filter(|run| run.status != RunStatus::Integrating)
        .count();
    let ready = queue
        .list(&crate::application::TaskQuery {
            status: crate::application::StatusFilter::Only(vec![crate::domain::TaskStatus::Ready]),
            limit: 1,
            ..Default::default()
        })?
        .total;
    // A draft goal's ready tasks wait for `goal ready`, not for a
    // predecessor, so they do not make free slots an alert.
    let ready_in_draft_goals: usize = queue
        .list_goals()?
        .iter()
        .filter(|goal| goal.status == crate::domain::GoalStatus::Draft)
        .map(|goal| goal.tasks.ready)
        .sum();
    let ready = ready.saturating_sub(ready_in_draft_goals);
    let snapshot = SlotSnapshot {
        free_slots: slots - i64::try_from(executing)?,
        candidates: queue.candidates()?.len(),
        ready,
    };
    Ok(serde_json::to_value(stats(
        &events, &goals, now, snapshot, query,
    ))?)
}

/// Inspect every registered supervisor and every unfinished run with its
/// lease, processes and paths. Reads only.
/// `doctor`: with `full`, every unfinished run with its lease, processes
/// and paths; without it, one line's worth per run and per supervisor
/// ([`RunHealth::summary`], [`SupervisorHealth::summary`]).
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = unix_time();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let processes = queue.processes(&run.id)?;
            let lease = leases
                .iter()
                .find(|l| l.run_id == run.id)
                .map(|l| lease_health(l, now));
            Ok(run_health(&run, &processes, lease, now))
        })
        .collect::<Result<Vec<_>>>()?;
    let supervisors = supervisors(&registrations, &leases, now);
    if !full {
        return Ok(json!({
            "checked_at": now,
            "supervisors": supervisors.iter().map(SupervisorHealth::summary).collect::<Vec<_>>(),
            "runs": runs.iter().map(RunHealth::summary).collect::<Vec<_>>(),
        }));
    }
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisors,
        runs,
    })?)
}

/// Mark an orphaned run `interrupted` (or a run whose `integrate` process
/// died `awaiting_integration` again) and drop its lease, after checking that
/// nothing registered for it is still alive. Never reruns, never deletes the
/// worktree or workspace, leaves the task `in_progress`, and does not touch
/// any other run.
pub fn recover(db: &Path, id: &str) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let run = queue.run(id)?;
    ensure!(
        matches!(
            run.status,
            RunStatus::Claimed
                | RunStatus::Starting
                | RunStatus::Running
                | RunStatus::Validating
                | RunStatus::Integrating
        ),
        "run {id} is {}; only unfinished runs can be recovered",
        run.status.as_str()
    );
    let now = unix_time();
    let lease = queue.run_lease(id)?.map(|l| lease_health(&l, now));
    let processes = queue.processes(&run.id)?;
    let health = run_health(&run, &processes, lease, now);
    ensure!(
        health.recoverable,
        "refusing to recover run {id}: {}",
        health.blockers.join("; ")
    );
    let report = json!({"run": health});
    let run = queue.recover_run(&run.id, processes.len(), report)?;
    Ok(json!({"outcome": "recovered", "run": run}))
}

/// `rebind`: bind the queue at `db` to the repository containing `repo`
/// after the repository moved, the only way the binding changes (ADR-0020).
/// Refused while a registered supervisor or an `integrate` still lives,
/// since both hold paths of the old repository. The change is appended to
/// `logs/rebind.jsonl`, the queue directory's `repository` file (if any)
/// is rewritten, and every run worktree still on disk gets its Git link
/// repaired from the new repository. Reports where a repository-resolved
/// queue now lives, which differs from the queue's own directory until it
/// is moved there.
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let repository = GitRepository::inspect(repo)?;
    let common_dir = path_text(&repository.common_dir)?;
    let live = queue
        .supervisors()?
        .into_iter()
        .filter(|registration| process_alive(registration.pid))
        .map(|registration| registration.pid)
        .collect::<Vec<_>>();
    ensure!(
        live.is_empty(),
        "refusing to rebind while a supervisor is running (pid {live:?}); stop it with `down --wait` first"
    );
    let leases = queue.run_leases()?;
    if let Some(run) = queue
        .runs_with_status(RunStatus::Integrating)?
        .into_iter()
        .find(|run| {
            leases
                .iter()
                .any(|lease| lease.run_id == run.id && process_alive(lease.pid))
        })
    {
        bail!(
            "refusing to rebind while run {} of task {} is integrating",
            run.id,
            run.task_id
        );
    }
    let previous = queue.rebind_repository(&common_dir)?;
    let changed = previous.as_deref() != Some(common_dir.as_str());
    let location = crate::infrastructure::location::QueueLocation::explicit(&db);
    if changed {
        fs::create_dir_all(&location.log_dir)?;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(location.log_dir.join(REBIND_LOG))?;
        writeln!(
            log,
            "{}",
            json!({
                "at": unix_time(),
                "previous_git_common_dir": previous,
                "git_common_dir": common_dir,
                "binary_version": crate::VERSION,
            })
        )?;
        let pointer = location
            .queue_dir
            .join(crate::infrastructure::location::REPOSITORY_FILE_NAME);
        if pointer.is_file() {
            fs::write(&pointer, format!("{common_dir}\n"))?;
        }
    }
    let worktrees = queue
        .all_runs()?
        .into_iter()
        .filter_map(|run| run.worktree_path.map(|path| (run.id, PathBuf::from(path))))
        .filter(|(_, path)| path.is_dir())
        .map(|(run_id, path)| {
            let error = repository.repair_worktree(&path).err();
            json!({
                "run_id": run_id,
                "worktree_path": path,
                "repaired": error.is_none(),
                "error": error.map(|e| format!("{e:#}")),
            })
        })
        .collect::<Vec<_>>();
    let resolved = crate::infrastructure::location::data_home()
        .ok()
        .map(|home| {
            crate::infrastructure::location::QueueLocation::for_repository(
                &repository.common_dir,
                &home,
            )
            .queue_dir
        });
    let move_to = resolved
        .clone()
        .filter(|dir| dir.canonicalize().ok().as_deref() != Some(location.queue_dir.as_path()));
    Ok(json!({
        "outcome": if changed { "rebound" } else { "unchanged" },
        "db": db,
        "previous_git_common_dir": previous,
        "git_common_dir": common_dir,
        "queue_dir": location.queue_dir,
        "repository_queue_dir": resolved,
        "move_to": move_to,
        "worktrees": worktrees,
    }))
}

/// Append-only record of `rebind` under the queue's `logs/`, one JSON
/// object per changed binding; the schema has no queue-level event.
pub const REBIND_LOG: &str = "rebind.jsonl";

fn lease_health(lease: &RunLease, now: i64) -> LeaseHealth {
    let age = now - lease.heartbeat_at;
    LeaseHealth {
        pid: lease.pid,
        alive: process_alive(lease.pid),
        heartbeat_at: lease.heartbeat_at,
        heartbeat_age_secs: age,
        stale: age > HEARTBEAT_TIMEOUT_SECS,
    }
}

/// Registered supervisors in registration order, then any other lease
/// holder (an `integrate` process) in lease order; leases join by token.
pub(crate) fn supervisors(
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
) -> Vec<SupervisorHealth> {
    let health = |pid: u32, heartbeat_at: i64, registration: Option<&SupervisorRegistration>| {
        let alive = process_alive(pid);
        let age = now - heartbeat_at;
        SupervisorHealth {
            pid,
            alive,
            registered: registration.is_some(),
            mode: registration.and_then(|r| r.mode),
            workspace_id: registration.and_then(|r| r.workspace_id.clone()),
            binary_version: registration.and_then(|r| r.binary_version.clone()),
            parallel: registration.map(|r| r.parallel),
            started_at: registration.map(|r| r.started_at),
            heartbeat_at,
            heartbeat_age_secs: age,
            stale: heartbeat_stale(alive, age),
            run_ids: Vec::new(),
        }
    };
    let mut entries: Vec<(&str, SupervisorHealth)> = registrations
        .iter()
        .map(|r| (r.token.as_str(), health(r.pid, r.heartbeat_at, Some(r))))
        .collect();
    for lease in leases {
        let index = match entries.iter().position(|(token, _)| *token == lease.token) {
            Some(index) => index,
            None => {
                entries.push((
                    lease.token.as_str(),
                    health(lease.pid, lease.heartbeat_at, None),
                ));
                entries.len() - 1
            }
        };
        let entry = &mut entries[index].1;
        entry.run_ids.push(lease.run_id.clone());
        if !entry.registered {
            // Every lease of one process carries the same heartbeat; the
            // freshest one stands for the process.
            let age = now - lease.heartbeat_at;
            if age < entry.heartbeat_age_secs {
                entry.heartbeat_at = lease.heartbeat_at;
                entry.heartbeat_age_secs = age;
                entry.stale = heartbeat_stale(entry.alive, age);
            }
        }
    }
    entries.into_iter().map(|(_, health)| health).collect()
}

fn run_health(
    run: &TaskRun,
    processes: &[RunProcess],
    lease: Option<LeaseHealth>,
    now: i64,
) -> RunHealth {
    let mut blockers = Vec::new();
    let processes: Vec<ProcessHealth> = processes
        .iter()
        .map(|process| {
            let age = now - process.heartbeat_at;
            let alive = process
                .exited_at
                .is_none()
                .then(|| process_alive(process.pid));
            if alive == Some(true) {
                blockers.push(format!("{} pid {} is alive", process.role, process.pid));
            }
            ProcessHealth {
                role: process.role.clone(),
                pid: process.pid,
                alive,
                heartbeat_at: process.heartbeat_at,
                heartbeat_age_secs: age,
                heartbeat_stale: process.exited_at.is_none() && age > HEARTBEAT_TIMEOUT_SECS,
                exited_at: process.exited_at,
                exit_code: process.exit_code,
            }
        })
        .collect();
    if let Some(lease) = &lease {
        if !lease.stale {
            blockers.push(format!(
                "lease heartbeat is {}s old (limit {HEARTBEAT_TIMEOUT_SECS}s)",
                lease.heartbeat_age_secs
            ));
        }
        if lease.alive {
            blockers.push(format!("supervisor pid {} is alive", lease.pid));
        }
    }
    let exists = |path: &Option<String>| path.as_deref().map(|p| Path::new(p).exists());
    RunHealth {
        run_id: run.id.clone(),
        task_id: run.task_id,
        status: run.status,
        workspace_id: run.workspace_id.clone(),
        worktree_path: run.worktree_path.clone(),
        worktree_exists: exists(&run.worktree_path),
        run_dir: run.run_dir.clone(),
        run_dir_exists: exists(&run.run_dir),
        receipt_exists: exists(&run.receipt_path),
        last_error: run.last_error.clone(),
        lease,
        processes,
        recoverable: blockers.is_empty(),
        blockers,
    }
}

/// Run from cmux, not from a pipe; stdout must remain a terminal for Claude.
/// `resume` reopens the session of a `needs_session` run the supervisor is
/// resuming (ADR-0019) instead of starting the worker.
pub fn session(db: &Path, id: &str, token: &str, claude: &Path, resume: bool) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    run_session(db, id, token, &provider, resume)
}

pub fn session_with_provider(
    db: &Path,
    id: &str,
    token: &str,
    provider: &dyn AgentProvider,
) -> Result<Value> {
    run_session(db, id, token, provider, false)
}

/// The wrapper of a resumed session: `session --resume`.
pub fn resume_session_with_provider(
    db: &Path,
    id: &str,
    token: &str,
    provider: &dyn AgentProvider,
) -> Result<Value> {
    run_session(db, id, token, provider, true)
}

fn run_session(
    db: &Path,
    id: &str,
    token: &str,
    provider: &dyn AgentProvider,
    resume: bool,
) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let started = Instant::now();
    // cmux may start this command before its create response reaches
    // supervisor. A resumed run keeps the workspace of its first session.
    let run = loop {
        let run = queue.run(id)?;
        if run.workspace_id.is_some() {
            break run;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(45),
            "workspace registration timed out"
        );
        thread::sleep(Duration::from_millis(100));
    };
    let pid = std::process::id();
    if resume {
        queue.register_resume_wrapper(id, token, pid)?;
    } else {
        queue.register_wrapper(id, token, pid)?;
    }
    let mut child_may_be_alive = false;
    let result = drive_agent(
        &mut queue,
        &run,
        provider,
        pid,
        resume,
        &mut child_may_be_alive,
    );
    match result {
        Ok(code) => {
            queue.wrapper_exited(id, pid, code)?;
            Ok(json!({"run_id": id, "exit_code": code}))
        }
        Err(error) => {
            let _ = queue.record_runtime_error(id, &format!("{error:#}"));
            if !child_may_be_alive {
                let _ = queue.wrapper_exited(id, pid, 127);
            }
            Err(error)
        }
    }
}

fn drive_agent(
    queue: &mut SqliteQueue,
    run: &TaskRun,
    provider: &dyn AgentProvider,
    pid: u32,
    resume: bool,
    child_may_be_alive: &mut bool,
) -> Result<i32> {
    let mut command = if resume {
        provider.resume_command(run)?
    } else {
        let prompt_path =
            Path::new(run.run_dir.as_ref().context("missing run directory")?).join("prompt.txt");
        provider.command(run, &fs::read_to_string(prompt_path)?)?
    };
    let mut child = command.spawn().context("launch agent")?;
    *child_may_be_alive = true;
    let registered = if resume {
        queue.register_resume_agent(&run.id, pid, child.id())
    } else {
        queue.register_agent(&run.id, pid, child.id())
    };
    if let Err(error) = registered {
        let _ = child.kill();
        if child.wait().is_ok() {
            *child_may_be_alive = false;
        }
        return Err(error);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            *child_may_be_alive = false;
            return Ok(status.code().unwrap_or(128));
        }
        if let Err(error) = queue.heartbeat_wrapper(&run.id, pid) {
            // Keep owning/waiting on the existing child even during a DB outage.
            eprintln!("wrapper heartbeat failed: {error:#}");
        }
        thread::sleep(Duration::from_secs(1));
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
