//! Execute claimed tasks in parallel, validate their receipts, close the
//! workspaces of accepted runs, land them on main one at a time, and recover
//! orphaned runs. One run's state machine is unchanged from the single-run
//! supervisor; the loop multiplexes independent slots and isolates failures.
//! A run whose supervisor died while its session lives on is adopted by a
//! supervisor with a free slot instead of being rerun (ADR-0012).
use crate::{
    application::{AgentProvider, TaskStore, WorkspaceBackend},
    domain::{
        ClaimOutcome, Goal, IntegrationOutcome, Predecessor, Receipt, ReceiptResult, RunLease,
        RunPaths, RunProcess, RunStatus, SupervisorMode, SupervisorRegistration, Task, TaskRun,
        heartbeat_stale,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, path_text, process_alive, run_shell_to_log, shell_join,
        },
        location::runs_dir,
        runtime_store::{
            HEARTBEAT_TIMEOUT_SECS, Landing, LeasedRun, RunPlan, Validation, lease_is_stale,
        },
        sqlite::SqliteQueue,
    },
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{IsTerminal, Write},
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
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            log_dir: None,
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
    let mut supervisor = Supervisor {
        queue,
        db,
        repository,
        cmux,
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
            if self.slots.is_empty() {
                if options.once || stopping || !self.claiming {
                    break;
                }
                thread::sleep(IDLE_POLL);
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
        while self.slots.len() < parallel {
            if self.queue.candidates()?.is_empty() {
                break;
            }
            let base = self.repository.main_head()?;
            let run = match self.queue.claim_for_supervisor(&base, &self.token)? {
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

    fn step(&mut self, slot: &mut Slot) -> Result<Step> {
        if !self.queue.holds_lease(&slot.run.id, &self.token)? {
            return Ok(Step::Disowned);
        }
        match &mut slot.phase {
            Phase::Session(watch) => {
                let Some(run) = watch.poll(
                    &mut self.queue,
                    self.cmux,
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
                // Only an accepted run gives up its workspace; failures keep it for inspection.
                let run = if run.status == RunStatus::AwaitingIntegration {
                    close_workspace(&mut self.queue, self.cmux, &self.token, &run, &self.log)?
                } else {
                    run
                };
                self.queue.release_lease(&run.id, &self.token)?;
                Ok(Step::Done(Box::new(run)))
            }
        }
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
                })
            }
        })
    }

    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
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
        let workspace = self.cmux.create(&task, &run, &command)?;
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
}

impl SessionWatch {
    /// One observation. `Some` once the wrapper exited and supervision finished
    /// (`validating` or `failed`); an error means the run must be retained.
    fn poll(
        &mut self,
        queue: &mut SqliteQueue,
        cmux: &dyn WorkspaceBackend,
        token: &str,
        run: &TaskRun,
        log: &SupervisorLog,
    ) -> Result<Option<TaskRun>> {
        let processes = queue.processes(&run.id)?;
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
            // Ask once, the way the maintainer would; never kill the session.
            cmux.send_exit(&self.workspace)?;
            let timeout = cmux.exit_timeout();
            queue.record_runtime_event(
                &run.id,
                "exit_requested",
                json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
            )?;
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
        } else {
            ensure!(
                self.startup.elapsed() < Duration::from_secs(45),
                "wrapper did not register within 45 seconds"
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

/// Cross-check the agent's receipt against Git and rerun the task's verification
/// commands on a thread with its own connection. Rejections become a
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
        let checked = check_receipt(&queue, &repository, &task, &run)?;
        Ok(match checked {
            Ok((receipt, commit)) => Validation {
                accepted: true,
                result_commit: Some(commit),
                reason: None,
                receipt: serde_json::to_value(receipt)?,
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
}

fn check_receipt(
    queue: &SqliteQueue,
    repository: &GitRepository,
    task: &Task,
    run: &TaskRun,
) -> Result<std::result::Result<(Receipt, String), Rejection>> {
    let reject = |reason: String, commit: Option<String>, receipt: Option<Receipt>| {
        Ok(Err(Rejection {
            reason,
            commit,
            receipt,
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
    if let Err(error) = receipt.check(&run.id) {
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
    // Rerun the task's own verification commands; the receipt's claims are not enough.
    let run_dir = Path::new(run.run_dir.as_ref().context("missing run directory")?);
    for (index, command) in task.verification_commands.iter().enumerate() {
        let log = run_dir.join(format!("verify-{}.log", index + 1));
        let status = run_shell_to_log(command, worktree, &log)?;
        let exit_code = status.code().unwrap_or(128);
        let output = fs::read_to_string(&log).unwrap_or_default();
        queue.record_runtime_event(
            &run.id,
            "verification_command",
            json!({
                "index": index + 1,
                "command": command,
                "exit_code": exit_code,
                "log_path": path_text(&log)?,
                "output_tail": tail(&output, 2000),
            }),
        )?;
        if exit_code != 0 {
            return reject(
                format!(
                    "verification command {command:?} exited with {exit_code}; see {}",
                    log.display()
                ),
                Some(commit),
                Some(receipt),
            );
        }
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
/// (receipt, descent from main, clean tree, verification commands), squash
/// the tree into one commit with `Dagq-Task` / `Dagq-Run` trailers and
/// fast-forward `main` to it. Never a merge commit, never a fast-forward of
/// the run branch itself. A conflict or a failed re-validation parks the run
/// as `needs_session` for a resumed session to fix; a rewritten receipt that
/// reports `failed` ends the run. `repo` is any checkout of the repository
/// the queue is bound to.
pub fn integrate(db: &Path, target: IntegrateTarget, repo: &Path) -> Result<Value> {
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
    let previous = run.status;
    let token = Uuid::new_v4().to_string();
    let main = repository.main_head()?;
    let run = queue.begin_integration(&run.id, &token, &main)?;
    let heartbeat = Heartbeat::start(db.clone(), token.clone());
    let task = queue.show(run.task_id)?.task;
    eprintln!(
        "run {} integrating task {} onto main {main}",
        run.id, run.task_id
    );
    let verdict = match land(&mut queue, &repository, &task, &run, &main) {
        Ok(verdict) => verdict,
        Err(error) => {
            // Nothing reached main: give the slot back and keep the run where it was.
            let message = format!("integration stopped before main moved: {error:#}");
            if let Err(record) =
                queue.abort_integration(&run.id, &token, previous.as_str(), &message)
            {
                eprintln!("run {}: could not record the error: {record:#}", run.id);
            }
            return Err(error.context(format!("run {} returned to {}", run.id, previous.as_str())));
        }
    };
    let outcome = match verdict {
        Verdict::Landed(landing) => {
            let verification_skipped = landing.verification_skipped;
            let (task, run) = queue
                .finish_integration(&run.id, &token, &landing, &common_dir)
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
            remove_landed_worktree(&mut queue, &repository, &run);
            IntegrationOutcome::Integrated {
                task,
                run: Box::new(run),
                verification_skipped,
            }
        }
        Verdict::Deferred { reason, detail } => {
            eprintln!("run {} needs a session: {reason}", run.id);
            let run = queue.defer_integration(&run.id, &token, &reason, detail)?;
            IntegrationOutcome::NeedsSession {
                run: Box::new(run),
                main,
                reason,
            }
        }
        Verdict::ReceiptFailed { reason, receipt } => {
            eprintln!("run {} failed: {reason}", run.id);
            let run = queue.fail_integration(&run.id, &token, &reason, receipt)?;
            IntegrationOutcome::Failed {
                run: Box::new(run),
                reason,
            }
        }
    };
    drop(heartbeat); // Stops the lease heartbeat before this process reports.
    Ok(serde_json::to_value(outcome)?)
}

enum Verdict {
    Landed(Landing),
    /// Re-validation did not pass; the worktree is left for a session.
    Deferred {
        reason: String,
        detail: Value,
    },
    /// The session's rewritten receipt reports `failed`; `receipt` is its
    /// JSON, kept with the `integration_failed` event.
    ReceiptFailed {
        reason: String,
        receipt: Value,
    },
}

/// Rebase, re-validate and land one run. `Ok(Deferred)` and
/// `Ok(ReceiptFailed)` are verdicts on the run; `Err` is a failure of the
/// landing itself (Git, files) before `main` moved.
fn land(
    queue: &mut SqliteQueue,
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
    if let Err(error) = receipt.check(&run.id) {
        return defer(format!("{error:#}"), json!({}));
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
    // The task's verification commands run again on the rebased tree, unless
    // the rebase was a no-op on the head validation itself verified: the
    // supervisor ran these very commands on this commit and tree, so a second
    // run can only repeat its result. A head a session wrote after
    // `needs_session` is not that head, even when the session rebased it onto
    // main itself, so it is verified here.
    let verification_skipped = rebased == head && run.result_commit.as_deref() == Some(&*head);
    if verification_skipped {
        queue.record_runtime_event(
            &run.id,
            "integration_verification_skipped",
            json!({
                "main": main,
                "head": rebased,
                "reason": "rebase was a no-op; validation already verified this head",
            }),
        )?;
    }
    let commands: &[String] = if verification_skipped {
        &[]
    } else {
        &task.verification_commands
    };
    for (index, command) in commands.iter().enumerate() {
        let log = run_dir.join(format!("integrate-verify-{}.log", index + 1));
        let status = run_shell_to_log(command, worktree, &log)?;
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
    Ok(Verdict::Landed(Landing {
        commit,
        source_commit: rebased,
        main_before: main.to_owned(),
        history_ref,
        message: paragraphs.join("\n\n"),
        verification_skipped,
    }))
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
    let diff = repository.diff(&base, &head)?;
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
        &diff,
    );
    let path = run_dir.join("review.md");
    let temporary = run_dir.join(format!(".review.md.{}.tmp", std::process::id()));
    fs::write(&temporary, text).with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("write {}", path.display()))?;
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
    diff: &str,
) -> String {
    let mut out = format!(
        "# Review of task {id}: {title}\n\n\
         - run: {run_id} ({status})\n\
         - base: {base} (run base {run_base})\n\
         - head: {head}\n\
         - branch: {branch}\n\
         - worktree: {worktree}\n\
         - verification logs: {run_dir}/verify-N.log\n\n\
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
         ## Diff\n\n`git diff {base}...{head}`\n\n{diff}",
        log = fenced("", log),
        stat = fenced("", stat),
        diff = fenced("diff", diff),
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
    Ok(format!(
        "You are executing dagq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree. Read its repository instructions.\n\
         Implement the task, run the required verification commands, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         Perform applicable unit tests, E2E, and subagent review. Record evidence or an explicit reason when not applicable.\n\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (run in the worktree):\n{verification}\n\
         {goal}{context}{predecessors}{siblings}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\"}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title and a description, for the maintainer to register; omit it when there is none.\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and it reruns the verification commands itself.\n\
         After submitting, report the outcome briefly and stop; do not run /exit yourself. Once you are idle the supervisor ends the session, and the maintainer can still send /exit. A receipt does not itself end the session.\n",
        task_id = task.id,
        run_id = run.id,
        title = task.title,
        description = task.description,
        acceptance = task.acceptance,
        verification = serde_json::to_string_pretty(&task.verification_commands)?,
    ))
}

/// The initial prompt of the maintainer session that `up` opens in the
/// `dagq <repo> maintainer` workspace. It names the queue and the roles,
/// points at the supervisor's logs, and asks for a first report through the
/// plugin's `dagq-maintain` skill; the CLI itself is documented there, not
/// here, so the prompt stays stable across skill revisions.
pub fn maintainer_prompt(db: &Path, log_dir: &Path) -> Result<String> {
    Ok(format!(
        "You are the maintainer session of the dagq queue at {db}.\n\
         Roles: supervisor is the resident `dagq supervise` process that runs tasks; maintainer is this session, which registers, watches, reviews and lands them; worker is the Claude session of one run.\n\
         The supervisor writes its logs to {log_dir} (one supervisor-<started_at>-<pid>.log per start, launchd output in launchd.log).\n\
         Start by using the dagq-maintain skill of the dagq plugin to run status and doctor. Report stale supervisors, unfinished runs, runs awaiting_integration and runs in needs_session, then wait for the user's instructions.\n\
         If the dagq-maintain skill is not available in this session, say so and wait.\n\
         Never open or edit the queue database directly; go through the dagq CLI only.\n",
        db = path_text(db)?,
        log_dir = path_text(log_dir)?,
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
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors(&registrations, &leases, now),
        "runs": runs,
        "attention": crate::watch::attention(&queue, &registrations, now)?,
        "cursor": cursor,
    }))
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
pub fn session(db: &Path, id: &str, token: &str, claude: &Path) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    session_with_provider(db, id, token, &provider)
}

pub fn session_with_provider(
    db: &Path,
    id: &str,
    token: &str,
    provider: &dyn AgentProvider,
) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let started = Instant::now();
    // cmux may start this command before its create response reaches supervisor.
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
    queue.register_wrapper(id, token, pid)?;
    let mut child_may_be_alive = false;
    let result = drive_agent(&mut queue, &run, provider, pid, &mut child_may_be_alive);
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
    child_may_be_alive: &mut bool,
) -> Result<i32> {
    let prompt_path =
        Path::new(run.run_dir.as_ref().context("missing run directory")?).join("prompt.txt");
    let text = fs::read_to_string(prompt_path)?;
    let mut child = provider
        .command(run, &text)?
        .spawn()
        .context("launch agent")?;
    *child_may_be_alive = true;
    if let Err(error) = queue.register_agent(&run.id, pid, child.id()) {
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
