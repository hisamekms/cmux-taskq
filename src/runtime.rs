//! Execute claimed tasks in parallel, validate their receipts, close the
//! workspaces of accepted runs, land them on main one at a time, and recover
//! orphaned runs. One run's state machine is unchanged from the single-run
//! supervisor; the loop multiplexes independent slots and isolates failures.
use crate::{
    application::{AgentProvider, TaskQueue, WorkspaceBackend},
    domain::{
        ClaimOutcome, IntegrationOutcome, Receipt, ReceiptResult, RunLease, RunProcess, RunStatus,
        Task, TaskRun,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, path_text, process_alive, run_shell_to_log, shell_join,
        },
        location::runs_dir,
        runtime_store::{HEARTBEAT_TIMEOUT_SECS, Landing, RunPlan, Validation},
        sqlite::SqliteQueue,
    },
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc,
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
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel,
            once,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// One supervisor process heartbeats every lease it holds with a single token.
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
                let queue = SqliteQueue::open(db)?;
                loop {
                    queue.heartbeat_leases(&token)?;
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
        slots: Vec::new(),
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
    };
    supervisor.run_loop(options)
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
}

impl Supervisor<'_> {
    fn run_loop(&mut self, options: &SuperviseOptions) -> Result<Value> {
        loop {
            if let Err(error) = self.heartbeat.check() {
                // Supervisor-level failure: note it on every run and keep the
                // leases; they go stale once this process is gone.
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

    /// Claim and provision candidates until every slot is taken or nothing is
    /// claimable. `main` is reread per claim so a task released by `integrate`
    /// starts from the main that contains its predecessor.
    fn fill_slots(&mut self, parallel: usize) -> Result<()> {
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
                    eprintln!("{message}; no further tasks will be claimed");
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
                    eprintln!("run {} is {}", run.id, run.status.as_str());
                    self.finished.push(*run);
                }
                Err(error) => {
                    // Creation/communication failures can be ambiguous: the
                    // session may be alive. Disown the run, delete nothing,
                    // and keep serving the other slots.
                    let message = format!("{error:#}");
                    eprintln!(
                        "run {} retained for inspection: {message}; see show {} and doctor",
                        slot.run.id, slot.run.task_id
                    );
                    self.abandon(&slot.run, message);
                }
            }
        }
    }

    fn abandon(&mut self, run: &TaskRun, message: String) {
        if let Err(error) = self.queue.abandon_run(&run.id, &self.token, &message) {
            eprintln!("run {}: could not record the error: {error:#}", run.id);
        }
        self.errors.push(RunError {
            run_id: run.id.clone(),
            task_id: run.task_id,
            message,
        });
    }

    fn step(&mut self, slot: &mut Slot) -> Result<Step> {
        match &mut slot.phase {
            Phase::Session(watch) => {
                let Some(run) = watch.poll(&mut self.queue, self.cmux, &self.token, &slot.run)?
                else {
                    return Ok(Step::Continue);
                };
                if run.status != RunStatus::Validating {
                    self.queue.release_lease(&run.id, &self.token)?;
                    return Ok(Step::Done(Box::new(run)));
                }
                let handle =
                    spawn_validation(self.db.clone(), self.repository.clone(), run.clone());
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
                    close_workspace(&mut self.queue, self.cmux, &self.token, &run)?
                } else {
                    run
                };
                self.queue.release_lease(&run.id, &self.token)?;
                Ok(Step::Done(Box::new(run)))
            }
        }
    }

    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
    fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = runs_dir(&self.db);
        let run_dir = state_dir.join(&claimed.id);
        let plan = RunPlan {
            repo_path: path_text(&self.repository.root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("taskq/{}", claimed.id),
            worktree_path: path_text(&run_dir.join("worktree"))?,
            receipt_path: path_text(&run_dir.join("receipt.json"))?,
            log_path: path_text(&run_dir.join("claude.debug.log"))?,
        };
        // Save intended paths before any external resource is created.
        self.queue.plan_run(&claimed.id, &self.token, &plan)?;
        fs::create_dir_all(&state_dir)?;
        fs::create_dir(&run_dir).context("run directory must be new")?;
        let run = self.queue.run(&claimed.id)?;
        let task = self.queue.show(run.task_id)?.task;
        fs::write(run_dir.join("prompt.txt"), prompt(&task, &run)?)?;
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
        let workspace = self.cmux.create(&run, &command)?;
        self.queue
            .workspace_created(&run.id, &self.token, &workspace)?;
        eprintln!(
            "task {} running in workspace {}; run {}",
            run.task_id, workspace, run.id
        );
        Ok(SessionWatch {
            workspace,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            exit_requested: None,
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
    ) -> Result<Option<TaskRun>> {
        let processes = queue.processes(&run.id)?;
        if !self.receipt_seen && self.receipt_path.is_file() {
            self.receipt_seen = true;
            queue.record_runtime_event(
                &run.id,
                "receipt_observed",
                json!({"path": path_text(&self.receipt_path)?, "validated": false}),
            )?;
            eprintln!(
                "receipt received for {}; waiting for the session to go idle (or an operator /exit)",
                run.id
            );
        }
        if self.receipt_seen
            && self.exit_requested.is_none()
            && let Some(evidence) = idle_after_receipt(&self.receipt_path, &self.idle_marker)?
        {
            queue.record_runtime_event(&run.id, "session_idle_observed", evidence)?;
            // Ask once, the way an operator would; never kill the session.
            cmux.send_exit(&self.workspace)?;
            let timeout = cmux.exit_timeout();
            queue.record_runtime_event(
                &run.id,
                "exit_requested",
                json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
            )?;
            eprintln!("exit requested for {}; waiting for session exit", run.id);
            self.exit_requested = Some(Instant::now());
        }
        if let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") {
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
        if let Some(requested) = self.exit_requested {
            let timeout = cmux.exit_timeout();
            if requested.elapsed() >= timeout {
                // The session is still alive; leave it to a human instead of forcing it.
                queue.record_runtime_event(
                    &run.id,
                    "exit_request_timed_out",
                    json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                bail!(
                    "session did not exit within {}s of the exit request; send /exit in workspace {} or recover the run",
                    timeout.as_secs(),
                    self.workspace
                );
            }
        }
        Ok(None)
    }
}

/// Evidence that the agent finished a response after publishing the receipt: an
/// idle marker written by the provider's stop hook no older than the receipt.
/// Markers from earlier turns (for example a question to the operator) do not count.
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
                eprintln!("run {} rejected: {}", run.id, rejection.reason);
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
) -> Result<TaskRun> {
    let workspace = run.workspace_id.as_ref().context("missing workspace")?;
    match cmux.close(workspace) {
        Ok(()) => queue.workspace_closed(&run.id, token),
        Err(error) => {
            let message = format!("workspace {workspace} could not be closed: {error:#}");
            eprintln!("run {}: {message}", run.id);
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
/// the tree into one commit with `Taskq-Task` / `Taskq-Run` trailers and
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
        Verdict::ReceiptFailed(reason) => {
            eprintln!("run {} failed: {reason}", run.id);
            let run = queue.fail_integration(&run.id, &token, &reason)?;
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
    /// The session's rewritten receipt reports `failed`.
    ReceiptFailed(String),
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
        return Ok(Verdict::ReceiptFailed(format!(
            "session reported the run as failed: {}",
            receipt.summary
        )));
    }
    if let Err(error) = receipt.check(&run.id) {
        return defer(format!("{error:#}"), json!({}));
    }
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
    // The task's verification commands run again on the rebased tree.
    for (index, command) in task.verification_commands.iter().enumerate() {
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
    // reachable under refs/taskq/runs/<run-id>.
    let paragraphs = commit_message(task, run, &receipt);
    let tree = repository.tree_of(&rebased)?;
    let commit = repository.commit_tree(&tree, main, &paragraphs)?;
    let history_ref = format!("refs/taskq/runs/{}", run.id);
    repository.update_ref(&history_ref, &rebased)?;
    repository.advance_main(main, &commit)?;
    Ok(Verdict::Landed(Landing {
        commit,
        source_commit: rebased,
        main_before: main.to_owned(),
        history_ref,
        message: paragraphs.join("\n\n"),
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
    paragraphs.push(format!("Taskq-Task: {}\nTaskq-Run: {}", task.id, run.id));
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

pub fn prompt(task: &Task, run: &TaskRun) -> Result<String> {
    let receipt = run.receipt_path.as_ref().context("missing receipt path")?;
    Ok(format!(
        "You are executing cmux-taskq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree. Read its repository instructions.\n\
         Implement the task, run the required verification commands, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         Perform applicable unit tests, E2E, and subagent review. Record evidence or an explicit reason when not applicable.\n\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (run in the worktree):\n{verification}\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\"}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and it reruns the verification commands itself.\n\
         After submitting, report the outcome briefly and stop; do not run /exit yourself. Once you are idle the supervisor ends the session, and an operator can still send /exit. A receipt does not itself end the session.\n",
        task_id = task.id,
        run_id = run.id,
        title = task.title,
        description = task.description,
        acceptance = task.acceptance,
        verification = serde_json::to_string_pretty(&task.verification_commands)?,
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

/// A supervisor process as seen through the leases it holds; every lease of
/// one process carries the same heartbeat.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorHealth {
    pub pid: u32,
    pub alive: bool,
    pub run_ids: Vec<String>,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
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

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisors: Vec<SupervisorHealth>,
    pub runs: Vec<RunHealth>,
}

/// Live supervisors and the unfinished runs with their leases, without
/// inspecting the runs' processes.
pub fn status(db: &Path) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = unix_time();
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
                "lease": lease,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors(&leases, now),
        "runs": runs,
    }))
}

/// Inspect every unfinished run, its lease, processes and paths. Reads only.
pub fn doctor(db: &Path) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = unix_time();
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
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisors: supervisors(&leases, now),
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

fn supervisors(leases: &[RunLease], now: i64) -> Vec<SupervisorHealth> {
    let mut by_pid: BTreeMap<u32, SupervisorHealth> = BTreeMap::new();
    for lease in leases {
        let age = now - lease.heartbeat_at;
        let entry = by_pid.entry(lease.pid).or_insert_with(|| SupervisorHealth {
            pid: lease.pid,
            alive: process_alive(lease.pid),
            run_ids: Vec::new(),
            heartbeat_age_secs: age,
            stale: age > HEARTBEAT_TIMEOUT_SECS,
        });
        entry.run_ids.push(lease.run_id.clone());
        entry.heartbeat_age_secs = entry.heartbeat_age_secs.min(age);
        entry.stale = entry.heartbeat_age_secs > HEARTBEAT_TIMEOUT_SECS;
    }
    by_pid.into_values().collect()
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
