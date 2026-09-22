//! Execute one reserved task, validate its receipt, close the workspace of an
//! accepted run, and recover orphaned runs.
use crate::{
    application::{AgentProvider, TaskQueue, WorkspaceBackend},
    domain::{ClaimOutcome, Receipt, RunProcess, RunStatus, SupervisorLease, Task, TaskRun},
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, path_text, process_alive, run_shell_to_log, shell_join,
        },
        runtime_store::{HEARTBEAT_TIMEOUT_SECS, RunPlan, Validation},
        sqlite::SqliteQueue,
    },
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
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
                    queue.heartbeat_supervisor(&token)?;
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
            "supervisor heartbeat failed; preserving run for inspection"
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

pub fn supervise(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    runner: &Path,
) -> Result<Value> {
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
    let token = Uuid::new_v4().to_string();
    queue.acquire_supervisor(&token, &path_text(&repository.common_dir)?)?;
    let heartbeat = Heartbeat::start(db.clone(), token.clone());
    let claim = queue.claim(&repository.base_commit);
    let run = match claim {
        Ok(ClaimOutcome::Claimed { run }) => run,
        other => {
            drop(heartbeat);
            queue.release_supervisor(&token)?;
            return Ok(serde_json::to_value(other?)?);
        }
    };
    let result = provision_and_monitor(
        &mut queue,
        &db,
        &repository,
        cmux,
        claude,
        runner,
        &token,
        &run,
        &heartbeat,
    );
    let result = result.and_then(|run| {
        if run.status == RunStatus::Validating {
            validate(&mut queue, &repository, &token, &run, &heartbeat)
        } else {
            Ok(run)
        }
    });
    // Only an accepted run gives up its workspace; failures keep it for inspection.
    let result = result.and_then(|run| {
        if run.status == RunStatus::AwaitingIntegration {
            close_workspace(&mut queue, cmux, &token, &run, &heartbeat)
        } else {
            Ok(run)
        }
    });
    drop(heartbeat);
    match result {
        Ok(run) => {
            queue.release_supervisor(&token)?;
            Ok(json!({"outcome": "finished", "run": run}))
        }
        Err(error) => {
            // Creation/communication failures can be ambiguous. Never free the
            // execution slot or delete resources when a session might be alive.
            let _ = queue.record_runtime_error(&run.id, &format!("{error:#}"));
            Err(error.context(format!(
                "run {} retained; inspect show {} and status before recovery",
                run.id, run.task_id
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn provision_and_monitor(
    queue: &mut SqliteQueue,
    db: &Path,
    repository: &GitRepository,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    runner: &Path,
    token: &str,
    claimed: &TaskRun,
    heartbeat: &Heartbeat,
) -> Result<TaskRun> {
    let parent = db.parent().context("database has no parent")?;
    let state_dir = parent.join(format!(
        "{}.runs",
        db.file_name()
            .context("database has no filename")?
            .to_string_lossy()
    ));
    let run_dir = state_dir.join(&claimed.id);
    let plan = RunPlan {
        repo_path: path_text(&repository.root)?,
        run_dir: path_text(&run_dir)?,
        branch: format!("taskq/{}", claimed.id),
        worktree_path: path_text(&run_dir.join("worktree"))?,
        receipt_path: path_text(&run_dir.join("receipt.json"))?,
        log_path: path_text(&run_dir.join("claude.debug.log"))?,
    };
    // Save intended paths before any external resource is created.
    queue.plan_run(&claimed.id, token, &plan)?;
    fs::create_dir_all(&state_dir)?;
    fs::create_dir(&run_dir).context("run directory must be new")?;
    let run = queue.run(&claimed.id)?;
    let task = queue.show(run.task_id)?.task;
    fs::write(run_dir.join("prompt.txt"), prompt(&task, &run)?)?;
    // A running wrapper must not change when the development binary is rebuilt.
    fs::copy(runner, run_dir.join("runner")).context("snapshot runtime binary")?;
    let git_output = repository.create_worktree(&run)?;
    fs::write(run_dir.join("worktree-create.txt"), git_output)?;
    queue.record_runtime_event(
        &run.id,
        "worktree_created",
        json!({"path": plan.worktree_path, "branch": plan.branch}),
    )?;
    heartbeat.check()?;
    let command = shell_join(&[
        path_text(&run_dir.join("runner"))?,
        "--db".into(),
        path_text(db)?,
        "session".into(),
        "--run".into(),
        run.id.clone(),
        "--lease".into(),
        token.into(),
        "--claude".into(),
        path_text(claude)?,
    ]);
    let workspace = cmux.create(&run, &command)?;
    queue.workspace_created(&run.id, token, &workspace)?;
    eprintln!(
        "task {} running in workspace {}; run {}",
        run.task_id, workspace, run.id
    );
    let startup = Instant::now();
    let mut receipt_seen = false;
    loop {
        heartbeat.check()?;
        let processes = queue.processes(&run.id)?;
        if !receipt_seen && Path::new(&plan.receipt_path).is_file() {
            receipt_seen = true;
            queue.record_runtime_event(
                &run.id,
                "receipt_observed",
                json!({"path": plan.receipt_path, "validated": false}),
            )?;
            eprintln!(
                "receipt received for {}; waiting for session exit (operator /exit)",
                run.id
            );
        }
        if let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") {
            if wrapper.exited_at.is_some() {
                match cmux.capture(&workspace) {
                    Ok(screen) => fs::write(run_dir.join("terminal-final.txt"), screen)?,
                    Err(error) => queue.record_runtime_event(
                        &run.id,
                        "screen_capture_failed",
                        json!({"error": format!("{error:#}")}),
                    )?,
                }
                return queue.finish_supervision(&run.id, token);
            }
            ensure!(
                unix_time() - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS,
                "wrapper heartbeat expired; session may still be alive"
            );
        } else {
            ensure!(
                startup.elapsed() < Duration::from_secs(45),
                "wrapper did not register within 45 seconds"
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Cross-check the agent's receipt against Git and rerun the task's verification
/// commands. Rejections become `failed`; only errors in the checks themselves
/// propagate, leaving the run in `validating`.
fn validate(
    queue: &mut SqliteQueue,
    repository: &GitRepository,
    token: &str,
    run: &TaskRun,
    heartbeat: &Heartbeat,
) -> Result<TaskRun> {
    let task = queue.show(run.task_id)?.task;
    let checked = check_receipt(queue, repository, &task, run, heartbeat)?;
    let validation = match checked {
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
    };
    queue.finish_validation(&run.id, token, &validation)
}

/// Close the cmux workspace of an accepted run. The worktree and branch stay
/// until integration. A close failure is recorded but does not change the run
/// status; `workspace_closed_at` stays null so nothing treats it as cleaned.
fn close_workspace(
    queue: &mut SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    token: &str,
    run: &TaskRun,
    heartbeat: &Heartbeat,
) -> Result<TaskRun> {
    heartbeat.check()?;
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
    heartbeat: &Heartbeat,
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
        heartbeat.check()?;
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
         After submitting, report the outcome and wait for the operator to exit with /exit. A receipt does not itself end the session.\n",
        task_id = task.id,
        run_id = run.id,
        title = task.title,
        description = task.description,
        acceptance = task.acceptance,
        verification = serde_json::to_string_pretty(&task.verification_commands)?,
    ))
}

/// Health of the supervisor lease as `doctor` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseHealth {
    pub pid: u32,
    pub alive: bool,
    pub heartbeat_at: i64,
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

/// One run holding the execution slot. `blockers` lists why `recover` would
/// refuse it; an empty list means it is recoverable now.
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
    pub processes: Vec<ProcessHealth>,
    pub blockers: Vec<String>,
    pub recoverable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisor: Option<LeaseHealth>,
    pub runs: Vec<RunHealth>,
}

/// Inspect the lease, unfinished runs, their processes and paths. Reads only.
pub fn doctor(db: &Path) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = unix_time();
    let supervisor = queue
        .supervisor_lease()?
        .map(|lease| lease_health(&lease, now));
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let processes = queue.processes(&run.id)?;
            Ok(run_health(&run, &processes, supervisor.as_ref(), now))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisor,
        runs,
    })?)
}

/// Mark an orphaned run `interrupted` and drop the stale lease, after checking
/// that nothing registered for it is still alive. Never reruns, never deletes
/// the worktree or workspace, and leaves the task `in_progress`.
pub fn recover(db: &Path, id: &str) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let run = queue.run(id)?;
    ensure!(
        matches!(
            run.status,
            RunStatus::Claimed | RunStatus::Starting | RunStatus::Running | RunStatus::Validating
        ),
        "run {id} is {}; only unfinished runs can be recovered",
        run.status.as_str()
    );
    let now = unix_time();
    let supervisor = queue
        .supervisor_lease()?
        .map(|lease| lease_health(&lease, now));
    let processes = queue.processes(&run.id)?;
    let health = run_health(&run, &processes, supervisor.as_ref(), now);
    ensure!(
        health.recoverable,
        "refusing to recover run {id}: {}",
        health.blockers.join("; ")
    );
    let report = json!({"supervisor": supervisor, "run": health});
    let run = queue.recover_run(&run.id, processes.len(), report)?;
    Ok(json!({"outcome": "recovered", "run": run}))
}

fn lease_health(lease: &SupervisorLease, now: i64) -> LeaseHealth {
    let age = now - lease.heartbeat_at;
    LeaseHealth {
        pid: lease.pid,
        alive: process_alive(lease.pid),
        heartbeat_at: lease.heartbeat_at,
        heartbeat_age_secs: age,
        stale: age > HEARTBEAT_TIMEOUT_SECS,
    }
}

fn run_health(
    run: &TaskRun,
    processes: &[RunProcess],
    supervisor: Option<&LeaseHealth>,
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
    if let Some(lease) = supervisor {
        if !lease.stale {
            blockers.push(format!(
                "supervisor heartbeat is {}s old (limit {HEARTBEAT_TIMEOUT_SECS}s)",
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
