//! Execute one reserved task. Result validation and cleanup are a separate stage.
use crate::{
    application::{AgentProvider, TaskQueue, WorkspaceBackend},
    domain::{ClaimOutcome, Task, TaskRun},
    infrastructure::{
        adapters::{ClaudeCode, GitRepository, path_text, shell_join},
        runtime_store::{HEARTBEAT_TIMEOUT_SECS, RunPlan},
        sqlite::SqliteQueue,
    },
};
use anyhow::{Context, Result, ensure};
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
    drop(heartbeat);
    match result {
        Ok(run) => {
            queue.release_supervisor(&token)?;
            Ok(json!({"outcome": "session_exited", "run": run}))
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
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA\",\"tests\":[{{\"command\":\"...\",\"exit_code\":0,\"evidence\":\"...\"}}],\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\"}}\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         After submitting, report the outcome and wait for the operator to exit with /exit. A receipt does not itself end the session.\n",
        task_id = task.id,
        run_id = run.id,
        title = task.title,
        description = task.description,
        acceptance = task.acceptance,
        verification = serde_json::to_string_pretty(&task.verification_commands)?,
    ))
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
