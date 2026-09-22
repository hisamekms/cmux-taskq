use anyhow::{Result, bail};
use cmux_taskq::{
    application::{AgentProvider, TaskQueue, WorkspaceBackend},
    domain::{NewTask, RunStatus, TaskAction, TaskRun, TaskStatus},
    infrastructure::{
        adapters::{shell_join, workspace_handle},
        sqlite::SqliteQueue,
    },
    runtime::{self, IntegrateTarget, SuperviseOptions},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo's directory");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "seed"]);
    let db = dir.path().join("queue's data.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
    add_ready_task(&mut queue, "test task", &[]);
    (dir, repo, db)
}

fn add_ready_task(queue: &mut SqliteQueue, title: &str, dependencies: &[i64]) -> i64 {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            dependencies: dependencies.to_vec(),
        })
        .unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
    task.id
}

/// Shell prelude for the fake agent: `receipt COMMIT [RUN_ID]` writes an
/// atomically renamed receipt claiming success with evidence on every check,
/// `idle` mimics Claude's Stop hook, and `await_exit` blocks until the test
/// workspace delivers the supervisor's exit request.
const AGENT_PRELUDE: &str = r#"
test -f seed.txt || exit 99
printf 'fixture log\n' > "$LOG"
receipt() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "${2:-$RUN_ID}" "$1" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
idle() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
await_exit() { while [ ! -f "$EXIT" ]; do sleep 0.2; done; }
commit() { printf 'change by %s\n' "$RUN_ID" > change.txt && git add change.txt && git commit -q -m "$1"; }
sleep 2
"#;
const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";
/// The first workspace the test backend hands out; see `workspace_id`.
const WORKSPACE_ID: &str = "01234567-89ab-4def-8123-000000000000";

fn workspace_id(n: usize) -> String {
    format!("01234567-89ab-4def-8123-{n:012x}")
}

struct TestProvider {
    script: String,
}
impl AgentProvider for TestProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, run: &TaskRun, prompt: &str) -> Result<Command> {
        assert!(prompt.contains("Acceptance criteria:"));
        assert!(prompt.contains("Verification commands (run in the worktree):"));
        let mut command = Command::new("/bin/sh");
        command
            .current_dir(run.worktree_path.as_ref().unwrap())
            .env("RUN_ID", &run.id)
            .env("RECEIPT", run.receipt_path.as_ref().unwrap())
            .env("LOG", run.log_path.as_ref().unwrap())
            .env("BASE", &run.base_commit)
            .env("IDLE", run.idle_marker_path().unwrap())
            .env("EXIT", exit_request_path(run.run_dir.as_ref().unwrap()))
            .arg("-c")
            .arg(format!("{AGENT_PRELUDE}\n{}", self.script));
        Ok(command)
    }
}

/// The test backend delivers an exit request as a file the fake agent polls for.
fn exit_request_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("exit-requested")
}

/// One session the test backend started, keyed by its workspace id.
struct TestSession {
    run_id: String,
    run_dir: String,
    worker: Option<thread::JoinHandle<Result<Value>>>,
}

/// Starts the session wrapper on a thread per workspace, with the agent
/// script chosen per task, and records exits and closes per workspace.
struct TestWorkspace {
    db: PathBuf,
    fail: bool,
    close_fail: bool,
    script: String,
    scripts: Mutex<HashMap<i64, String>>,
    exit_timeout: Duration,
    exits_sent: AtomicUsize,
    sessions: Mutex<Vec<(String, TestSession)>>,
    closed: Mutex<Vec<String>>,
}
impl TestWorkspace {
    fn new(db: &Path, fail: bool, script: &str) -> Self {
        Self {
            db: db.into(),
            fail,
            close_fail: false,
            script: script.into(),
            scripts: Mutex::new(HashMap::new()),
            exit_timeout: Duration::from_secs(120),
            exits_sent: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        }
    }
    /// Agent script for one task; other tasks use the default script.
    fn script_for(&self, task_id: i64, script: &str) {
        self.scripts.lock().unwrap().insert(task_id, script.into());
    }
    fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
    /// Wait for every session wrapper started so far to return successfully.
    fn join(&self) {
        let workers: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|(_, s)| s.worker.take())
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
    }
    fn session_run_dir(&self, workspace_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_dir.clone())
            .expect("workspace was created")
    }
}
impl WorkspaceBackend for TestWorkspace {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn create(&self, run: &TaskRun, command: &str) -> Result<String> {
        assert!(
            Path::new(run.worktree_path.as_ref().unwrap())
                .join("seed.txt")
                .exists()
        );
        assert!(command.contains("'\"'\"'")); // Database path contains an apostrophe.
        if self.fail {
            bail!("injected workspace creation failure");
        }
        let token: String = Connection::open(&self.db)?.query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [&run.id],
            |r| r.get(0),
        )?;
        let db = self.db.clone();
        let id = run.id.clone();
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&run.task_id)
            .cloned()
            .unwrap_or_else(|| self.script.clone());
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = workspace_id(sessions.len());
        let worker = thread::spawn(move || {
            runtime::session_with_provider(&db, &id, &token, &TestProvider { script })
        });
        sessions.push((
            workspace.clone(),
            TestSession {
                run_id: run.id.clone(),
                run_dir: run.run_dir.clone().unwrap(),
                worker: Some(worker),
            },
        ));
        Ok(workspace)
    }
    fn capture(&self, _: &str) -> Result<String> {
        Ok("fixture terminal screen".into())
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        // The session must have exited before the supervisor gives up the workspace.
        let run_id = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_id.clone())
            .expect("workspace was created");
        let exited: bool = Connection::open(&self.db)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
            [&run_id],
            |r| r.get(0),
        )?;
        assert!(exited);
        if self.close_fail {
            bail!("injected workspace close failure");
        }
        self.closed.lock().unwrap().push(workspace_id.into());
        Ok(())
    }

    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        self.exits_sent.fetch_add(1, Ordering::SeqCst);
        let run_dir = self.session_run_dir(workspace_id);
        fs::write(exit_request_path(&run_dir), "")?;
        Ok(())
    }
    fn exit_timeout(&self) -> Duration {
        self.exit_timeout
    }
}

/// /bin/sh --version is not portable; a tiny standalone provider preflight stub.
fn claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-stub");
    fs::write(&stub, "#!/bin/sh\nprintf 'test provider\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// One pass of the parallel supervisor: claim whatever is ready, finish it, exit.
fn supervise(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    supervise_with(db, repo, backend, &SuperviseOptions::new(4, true))
}

fn supervise_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> Result<Value> {
    runtime::supervise(
        db,
        repo,
        backend,
        &claude_stub(db),
        Path::new(env!("CARGO_BIN_EXE_cmux-taskq")),
        options,
    )
}

/// Poll the queue until `condition` holds or the deadline passes.
fn wait_until(db: &Path, timeout: Duration, mut condition: impl FnMut(&mut SqliteQueue) -> bool) {
    let started = Instant::now();
    let mut queue = SqliteQueue::open(db).unwrap();
    while !condition(&mut queue) {
        assert!(
            started.elapsed() < timeout,
            "condition not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Run one fake agent script through supervise and return the task detail.
fn run_agent(script: &str) -> (TempDir, PathBuf, cmux_taskq::domain::TaskDetail) {
    run_agent_with(script, false)
}

fn run_agent_with(
    script: &str,
    close_fail: bool,
) -> (TempDir, PathBuf, cmux_taskq::domain::TaskDetail) {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, script);
    backend.close_fail = close_fail;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    // These scripts exit on their own, like an operator's /exit; nothing was requested.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    let run = &detail.runs[0];
    assert_eq!(outcome["runs"][0]["id"], json!(run.id));
    // Runs live in `runs/` next to the (canonicalized) database, worktree inside.
    let run_dir = db
        .canonicalize()
        .unwrap()
        .with_file_name("runs")
        .join(&run.id);
    assert_eq!(Path::new(run.run_dir.as_ref().unwrap()), run_dir);
    assert_eq!(
        Path::new(run.worktree_path.as_ref().unwrap()),
        run_dir.join("worktree")
    );
    // Every outcome keeps the worktree; only an accepted run closes its workspace.
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
    assert_eq!(run.workspace_id.as_deref(), Some(WORKSPACE_ID));
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    if run.status == RunStatus::AwaitingIntegration && !close_fail {
        assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
        assert!(run.workspace_closed_at.is_some());
        assert!(kinds.contains(&"workspace_closed"));
    } else {
        assert!(backend.closed().is_empty());
        assert!(run.workspace_closed_at.is_none());
        assert!(!kinds.contains(&"workspace_closed"));
    }
    assert_eq!(kinds.contains(&"cleanup_failed"), close_fail);
    // The run came to rest: its lease is gone, and the task still owns it.
    assert!(kinds.contains(&"lease_acquired"));
    assert!(kinds.contains(&"lease_released"));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    (dir, db, detail)
}
fn rejection_reason(detail: &cmux_taskq::domain::TaskDetail) -> String {
    let run = &detail.runs[0];
    assert_eq!(run.status, RunStatus::Failed);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(event.payload["status"], "failed");
    assert_eq!(event.payload["accepted"], false);
    let reason = event.payload["reason"].as_str().unwrap().to_owned();
    assert_eq!(run.last_error.as_deref(), Some(reason.as_str()));
    reason
}

#[test]
fn valid_receipt_is_verified_and_awaits_integration() {
    let (_dir, db, detail) = run_agent(VALID_AGENT);
    let run = &detail.runs[0];
    assert_eq!(run.status, RunStatus::AwaitingIntegration);
    assert!(run.last_error.is_none());
    let commit = run.result_commit.as_ref().unwrap();
    assert_ne!(commit, &run.base_commit);
    let head = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path.as_ref().unwrap())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), commit);
    assert_eq!(
        fs::read_to_string(run.log_path.as_ref().unwrap()).unwrap(),
        "fixture log\n"
    );
    assert!(
        Path::new(run.run_dir.as_ref().unwrap())
            .join("runner")
            .exists()
    );
    assert_eq!(detail.processes.len(), 2);
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent_started"));
    assert!(kinds.contains(&"receipt_observed"));
    let verification = detail
        .events
        .iter()
        .find(|e| e.kind == "verification_command")
        .unwrap();
    assert_eq!(verification.payload["command"], "test -f seed.txt");
    assert_eq!(verification.payload["exit_code"], 0);
    assert!(Path::new(verification.payload["log_path"].as_str().unwrap()).exists());
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["status"], "awaiting_integration");
    assert_eq!(finished.payload["result_commit"], json!(commit));
    assert_eq!(
        finished.payload["receipt"]["e2e"]["status"],
        "not_applicable"
    );
    // The workspace is closed only after validation succeeded; the branch stays.
    let closed = detail
        .events
        .iter()
        .find(|e| e.kind == "workspace_closed")
        .unwrap();
    assert!(closed.id > finished.id);
    assert_eq!(closed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(
        closed.payload["closed_at"],
        json!(run.workspace_closed_at.unwrap())
    );
    let branch = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path.as_ref().unwrap())
        .args(["symbolic-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(branch.stdout).unwrap().trim(),
        format!("refs/heads/{}", run.branch.as_ref().unwrap())
    );
    // The task still owns its slot until integration; no second run starts.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
    assert!(queue.candidates().unwrap().is_empty());
}

#[test]
fn failed_workspace_close_is_recorded_without_changing_run_status() {
    let (_dir, _db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    assert_eq!(run.status, RunStatus::AwaitingIntegration);
    assert!(run.result_commit.is_some());
    let error = run.last_error.as_ref().unwrap();
    assert!(
        error.contains("injected workspace close failure"),
        "{error}"
    );
    assert!(error.contains(WORKSPACE_ID));
    let failed = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(failed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(failed.payload["message"], json!(error));
    // Validation itself was accepted; the failure is confined to cleanup.
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["accepted"], true);
    assert!(failed.id > finished.id);
}

#[test]
fn missing_receipt_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work");
    assert!(rejection_reason(&detail).contains("receipt was not submitted"));
    assert!(detail.runs[0].result_commit.is_none());
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == "verification_command")
    );
}

#[test]
fn run_id_mismatch_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\" other-run");
    assert!(rejection_reason(&detail).contains("run_id other-run does not match"));
    assert!(detail.runs[0].result_commit.is_none());
}

#[test]
fn receipt_without_new_commit_fails_validation() {
    let (_dir, _db, detail) = run_agent("receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("no commit was made"));
}

#[test]
fn receipt_commit_that_is_not_branch_head_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("is not the head of taskq/"));
}

#[test]
fn dirty_worktree_fails_validation() {
    let (_dir, _db, detail) = run_agent(
        "commit work; printf 'scratch\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"",
    );
    let reason = rejection_reason(&detail);
    assert!(reason.contains("worktree is not clean"));
    assert!(reason.contains("untracked.txt"));
    // The verified commit is still recorded for inspection.
    assert!(detail.runs[0].result_commit.is_some());
}

#[test]
fn failing_verification_command_fails_validation_despite_receipt_claims() {
    let (_dir, _db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let reason = rejection_reason(&detail);
    assert!(reason.contains("verification command \"test -f seed.txt\" exited with 1"));
    let verification = detail
        .events
        .iter()
        .find(|e| e.kind == "verification_command")
        .unwrap();
    assert_eq!(verification.payload["exit_code"], 1);
    assert!(detail.runs[0].result_commit.is_some());
}

#[test]
fn receipt_structure_is_checked_before_git() {
    use cmux_taskq::domain::Receipt;
    let valid = r#"{"run_id":"r","result":"succeeded","commit":"0123456789abcdef0123456789abcdef01234567",
        "tests":{"status":"passed","evidence_or_reason":"cargo test"},
        "e2e":{"status":"not_applicable","evidence_or_reason":"library only"},
        "subagent_review":{"status":"passed","evidence_or_reason":"no findings"},"summary":"ok"}"#;
    Receipt::parse(valid).unwrap().check("r").unwrap();
    let cases = [
        (valid.replace("\"r\"", "\"other\""), "does not match"),
        (
            valid.replace("succeeded", "failed"),
            "agent reported result failed",
        ),
        (
            valid.replace("library only", " "),
            "e2e is not_applicable without evidence or reason",
        ),
        (
            valid.replace(
                "\"passed\",\"evidence_or_reason\":\"no findings\"",
                "\"failed\",\"evidence_or_reason\":\"bug\"",
            ),
            "subagent_review as failed: bug",
        ),
        (
            valid.replace("0123456789abcdef0123456789abcdef01234567", "0123456"),
            "receipt commit",
        ),
    ];
    for (text, expected) in cases {
        let error = format!(
            "{:#}",
            Receipt::parse(&text).unwrap().check("r").unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
    }
    assert!(Receipt::parse("{\"run_id\":\"r\"}").is_err());
    assert!(Receipt::parse(&valid.replace("passed", "maybe")).is_err());
}

fn event_kinds(detail: &cmux_taskq::domain::TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

#[test]
fn idle_marker_after_receipt_triggers_exit_request_and_run_finishes() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status, RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
    // The idle evidence names the hook and session that produced it.
    let idle = detail
        .events
        .iter()
        .find(|e| e.kind == "session_idle_observed")
        .unwrap();
    assert_eq!(idle.payload["hook_event_name"], "Stop");
    assert_eq!(idle.payload["session_id"], json!(run.id));
    assert_eq!(
        idle.payload["marker_path"],
        json!(run.idle_marker_path().unwrap())
    );
    assert!(
        idle.payload["marker_modified"].as_i64().unwrap()
            >= idle.payload["receipt_modified"].as_i64().unwrap()
    );
    let requested = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_requested")
        .unwrap();
    assert_eq!(requested.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(requested.payload["timeout_secs"], 120);
}

#[test]
fn missing_or_stale_idle_marker_does_not_request_exit() {
    // No marker at all, then a marker older than the receipt (an earlier turn).
    // Both sessions end by themselves, as with an operator's /exit.
    for script in [
        "commit work; receipt \"$(git rev-parse HEAD)\"; sleep 3",
        "idle; sleep 1.1; commit work; receipt \"$(git rev-parse HEAD)\"; sleep 3",
    ] {
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(1).unwrap();
        let kinds = event_kinds(&detail);
        assert!(kinds.contains(&"receipt_observed"));
        assert!(!kinds.contains(&"session_idle_observed"));
        assert!(!kinds.contains(&"exit_requested"));
    }
}

#[test]
fn unanswered_exit_request_times_out_and_retains_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 6",
    );
    backend.exit_timeout = Duration::from_secs(2);
    // The run is given up, not the supervisor: the pass ends normally with the
    // error listed per run.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    let error = outcome["errors"][0]["message"].as_str().unwrap();
    assert!(error.contains("did not exit within 2s"), "{error}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    // The session was never killed; it is still alive when supervise gives up.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    let run = &detail.runs[0];
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id));
    assert_eq!(run.status, RunStatus::Running);
    assert!(run.last_error.as_ref().unwrap().contains("did not exit"));
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
    // The lease is dropped so recovery can judge the run by its processes alone.
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"exit_requested"));
    assert!(kinds.contains(&"exit_request_timed_out"));
    assert!(!kinds.contains(&"session_exited"));
    assert!(!kinds.contains(&"lease_released"));
    let timed_out = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_request_timed_out")
        .unwrap();
    assert_eq!(timed_out.payload["timeout_secs"], 2);
    let runtime_error = detail
        .events
        .iter()
        .find(|e| e.kind == "runtime_error")
        .unwrap();
    assert_eq!(runtime_error.payload["lease_released"], true);
    // Still alive: doctor sees the wrapper and refuses recovery.
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisors"], json!([]));
    assert_eq!(report["runs"][0]["lease"], Value::Null);
    assert_eq!(report["runs"][0]["recoverable"], false);
    assert!(runtime::recover(&db, &run.id).is_err());
    // A later manual exit is still recorded by the wrapper; nothing restarts the run.
    backend.join();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.runs[0].status, RunStatus::Running);
    assert!(event_kinds(&detail).contains(&"session_exited"));
    let again = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(again["runs"], json!([]));
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
    // Recovery is per run and needs no supervisor to be stopped.
    assert_eq!(
        runtime::recover(&db, &run.id).unwrap()["run"]["status"],
        "interrupted"
    );
}

#[test]
fn claude_stop_hook_settings_publish_the_idle_marker() {
    use cmux_taskq::infrastructure::adapters::{ClaudeCode, stop_hook_settings};
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run's dir");
    fs::create_dir(&run_dir).unwrap();
    let run = TaskRun {
        id: "11111111-2222-4333-8444-555555555555".into(),
        task_id: 1,
        status: RunStatus::Starting,
        requested_provider: cmux_taskq::domain::Provider::Claude,
        actual_provider: cmux_taskq::domain::Provider::Claude,
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        branch: Some("taskq/x".into()),
        worktree_path: Some(dir.path().to_str().unwrap().into()),
        workspace_id: None,
        receipt_path: Some(run_dir.join("receipt.json").to_str().unwrap().into()),
        log_path: Some(run_dir.join("claude.debug.log").to_str().unwrap().into()),
        result_commit: None,
        repo_path: None,
        run_dir: Some(run_dir.to_str().unwrap().into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    };
    let command = ClaudeCode {
        executable: "claude".into(),
    }
    .command(&run, "prompt")
    .unwrap();
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let settings = run_dir.join("claude-settings.json");
    assert!(args.contains(&"--settings".to_string()));
    assert!(args.contains(&settings.to_str().unwrap().to_string()));
    let text = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        text,
        stop_hook_settings(&run.idle_marker_path().unwrap()).unwrap()
    );
    let parsed: Value = serde_json::from_str(&text).unwrap();
    let hook = &parsed["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    // Run the hook exactly as Claude would: shell command, event JSON on stdin.
    let payload =
        r#"{"session_id":"11111111-2222-4333-8444-555555555555","hook_event_name":"Stop"}"#;
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook["command"].as_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
        payload
    );
    assert!(!run_dir.join("idle.json.tmp").exists());
}

#[test]
fn failed_agent_retains_worktree_and_does_not_complete_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed");
    // A nonzero session exit is final; the receipt is not validated and the
    // workspace stays open for inspection.
    assert_eq!(outcome["runs"][0]["result_commit"], Value::Null);
    assert_eq!(outcome["runs"][0]["workspace_closed_at"], Value::Null);
    assert_eq!(
        outcome["runs"][0]["last_error"],
        "session exited with code 7"
    );
    assert!(backend.closed().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert_eq!(
        detail.runs[0].last_error.as_deref(),
        Some("session exited with code 7")
    );
    assert!(Path::new(detail.runs[0].worktree_path.as_ref().unwrap()).exists());
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    // A failed run does not free the task automatically, but the operator may give up on it.
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));
    queue.transition(1, TaskAction::Cancel).unwrap();
    assert_eq!(queue.show(1).unwrap().task.status, TaskStatus::Canceled);
}

#[test]
fn provisioning_failure_retains_the_run_and_stops_claiming_other_tasks() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "untouched", &[]);
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("injected workspace"), "{error}");
    assert!(error.contains("claiming stopped"), "{error}");
    let detail = queue.show(1).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status, RunStatus::Starting);
    assert!(
        run.last_error
            .as_ref()
            .unwrap()
            .contains("injected workspace")
    );
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
    // The environment is suspect: the second candidate was left alone.
    assert!(queue.show(2).unwrap().runs.is_empty());
    assert_eq!(queue.candidates().unwrap()[0].id, 2);
    // The run is disowned, so nothing has to be stopped before recovering it.
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        runtime::doctor(&db).unwrap()["runs"][0]["recoverable"],
        true
    );
    assert_eq!(
        runtime::recover(&db, &run.id).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
}

#[test]
fn claim_creates_a_lease_that_only_its_owner_can_use_or_release() {
    use cmux_taskq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.bind_repository("/repo/one/.git").unwrap();
    assert!(queue.bind_repository("/repo/two/.git").is_err());
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(base, "first").unwrap() else {
        panic!()
    };
    assert!(matches!(
        queue.claim_for_supervisor(base, "first").unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    let lease = queue.run_lease(&run.id).unwrap().unwrap();
    assert_eq!(lease.pid, std::process::id());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
    // An idle supervisor heartbeats nothing; the owner heartbeats its runs.
    assert_eq!(queue.heartbeat_leases("second").unwrap(), 0);
    assert_eq!(queue.heartbeat_leases("first").unwrap(), 1);
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "taskq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    assert!(queue.plan_run(&run.id, "second", &plan).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(queue.plan_run(&run.id, "first", &plan).is_err()); // Stale.
    assert!(queue.release_lease(&run.id, "second").is_err());
    assert_eq!(queue.run_lease(&run.id).unwrap().unwrap().heartbeat_at, 0);
    queue.heartbeat_leases("first").unwrap();
    queue.plan_run(&run.id, "first", &plan).unwrap();
    queue.release_lease(&run.id, "first").unwrap();
    assert!(queue.run_lease(&run.id).unwrap().is_none());
    assert!(queue.release_lease(&run.id, "first").is_err());
    // The token stays on the run as a record of who executed it.
    let raw_token: String = raw
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw_token, "first");
}

#[test]
fn shell_arguments_round_trip_without_expansion_and_cmux_handles_are_strict() {
    let value = "a'b $HOME $(echo injected) `echo injected`\nmore";
    let result = Command::new("/bin/sh")
        .arg("-c")
        .arg(shell_join(&["printf".into(), "%s".into(), value.into()]))
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    assert_eq!(workspace_handle("OK workspace:7\n").unwrap(), "workspace:7");
    assert!(workspace_handle("OK workspace:7; touch file").is_err());
    assert!(workspace_handle("OK surface:7").is_err());
}

#[test]
fn migration_from_v1_preserves_task_and_initializes_runtime_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(include_str!("../migrations/0001_queue.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 1).unwrap();
    raw.execute("INSERT INTO tasks(title,description,acceptance,verification_commands) VALUES ('preserved','','','[]')", []).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.schema_version().unwrap(), 6);
    assert_eq!(queue.show(1).unwrap().task.title, "preserved");
    assert!(queue.run_leases().unwrap().is_empty());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_stale_owners() {
    use cmux_taskq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor("0123456789abcdef0123456789abcdef01234567", "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            &run.id,
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "taskq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    assert!(queue.register_wrapper(&run.id, "owner", 10).is_err()); // Workspace not attached yet.
    queue
        .workspace_created(&run.id, "owner", "workspace")
        .unwrap();
    assert!(queue.register_wrapper(&run.id, "other-owner", 10).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(queue.register_wrapper(&run.id, "owner", 10).is_err());
    queue.heartbeat_leases("owner").unwrap();
    queue.register_wrapper(&run.id, "owner", 10).unwrap();
    assert!(queue.register_wrapper(&run.id, "owner", 11).is_err());
    assert!(queue.register_agent(&run.id, 11, 12).is_err());
    queue.register_agent(&run.id, 10, 12).unwrap();
    assert!(queue.finish_supervision(&run.id, "owner").is_err()); // Still live.
    queue.wrapper_exited(&run.id, 10, 0).unwrap();
    assert!(queue.heartbeat_wrapper(&run.id, 10).is_err());
    assert_eq!(
        queue.finish_supervision(&run.id, "owner").unwrap().status,
        RunStatus::Validating
    );
    let validation = Validation {
        accepted: false,
        result_commit: None,
        reason: Some("receipt was not submitted".into()),
        receipt: Value::Null,
    };
    assert!(
        queue
            .finish_validation(&run.id, "other-owner", &validation)
            .is_err()
    );
    let failed = queue
        .finish_validation(&run.id, "owner", &validation)
        .unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(
        failed.last_error.as_deref(),
        Some("receipt was not submitted")
    );
    assert!(
        queue
            .finish_validation(&run.id, "owner", &validation)
            .is_err()
    ); // Terminal.
    // A failed run never records a workspace close or cleanup failure.
    assert!(queue.workspace_closed(&run.id, "owner").is_err());
    assert!(queue.cleanup_failed(&run.id, "owner", "late").is_err());
}

#[test]
fn workspace_close_is_recorded_once_and_only_for_accepted_runs() {
    use cmux_taskq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor("0123456789abcdef0123456789abcdef01234567", "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            &run.id,
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "taskq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    queue
        .workspace_created(&run.id, "owner", WORKSPACE_ID)
        .unwrap();
    queue.register_wrapper(&run.id, "owner", 10).unwrap();
    queue.register_agent(&run.id, 10, 12).unwrap();
    // Still running: neither close nor cleanup failure may be recorded.
    assert!(queue.workspace_closed(&run.id, "owner").is_err());
    assert!(queue.cleanup_failed(&run.id, "owner", "early").is_err());
    queue.wrapper_exited(&run.id, 10, 0).unwrap();
    queue.finish_supervision(&run.id, "owner").unwrap();
    let accepted = queue
        .finish_validation(
            &run.id,
            "owner",
            &Validation {
                accepted: true,
                result_commit: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
                reason: None,
                receipt: Value::Null,
            },
        )
        .unwrap();
    assert_eq!(accepted.status, RunStatus::AwaitingIntegration);
    assert!(accepted.workspace_closed_at.is_none());
    assert!(queue.workspace_closed(&run.id, "other-owner").is_err());
    let failed = queue.cleanup_failed(&run.id, "owner", "cmux down").unwrap();
    assert_eq!(failed.status, RunStatus::AwaitingIntegration);
    assert_eq!(failed.last_error.as_deref(), Some("cmux down"));
    assert!(failed.workspace_closed_at.is_none());
    // A later successful close clears nothing but records the close once.
    let closed = queue.workspace_closed(&run.id, "owner").unwrap();
    assert!(closed.workspace_closed_at.is_some());
    assert_eq!(closed.status, RunStatus::AwaitingIntegration);
    assert!(queue.workspace_closed(&run.id, "owner").is_err());
    assert!(queue.cleanup_failed(&run.id, "owner", "late").is_err());
    let kinds: Vec<String> = queue
        .show(1)
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "workspace_closed").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "cleanup_failed").count(), 1);
}

#[test]
fn no_ready_task_ends_a_once_pass_without_creating_a_run_or_lease() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.show(1).unwrap().runs.is_empty());
    assert!(supervise_with(&db, &repo, &backend, &SuperviseOptions::new(0, true)).is_err());
}

/// A PID that certainly belonged to a process that has already exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Register a run the way `supervise` does under `token`, with the given PIDs
/// as wrapper and agent, but with no supervisor loop watching it. Returns the
/// running run.
fn orphan_run(repo: &Path, db: &Path, token: &str, wrapper: u32, agent: u32) -> TaskRun {
    use cmux_taskq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&repository.base_commit, token)
        .unwrap()
    else {
        panic!()
    };
    let run_dir = db.parent().unwrap().join(format!("orphan-{}", run.id));
    fs::create_dir(&run_dir).unwrap();
    queue
        .plan_run(
            &run.id,
            token,
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("taskq/{}", run.id),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("log")).unwrap(),
            },
        )
        .unwrap();
    let run = queue.run(&run.id).unwrap();
    repository.create_worktree(&run).unwrap();
    queue
        .workspace_created(&run.id, token, &format!("ws-{}", run.task_id))
        .unwrap();
    queue.register_wrapper(&run.id, token, wrapper).unwrap();
    queue.register_agent(&run.id, wrapper, agent).unwrap();
    let run = queue.run(&run.id).unwrap();
    assert_eq!(run.status, RunStatus::Running);
    run
}

#[test]
fn recover_requires_dead_processes_and_stale_lease_then_allows_a_new_run() {
    let (_dir, repo, db) = fixture();
    let mut wrapper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut agent = Command::new("sleep").arg("60").spawn().unwrap();
    let run = orphan_run(&repo, &db, "owner", wrapper.id(), agent.id());
    let mut queue = SqliteQueue::open(&db).unwrap();

    // Everything is alive: doctor says so and recover refuses.
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisors"][0]["pid"], json!(std::process::id()));
    assert_eq!(report["supervisors"][0]["stale"], false);
    assert_eq!(report["supervisors"][0]["alive"], true);
    assert_eq!(report["supervisors"][0]["run_ids"], json!([run.id]));
    let health = &report["runs"][0];
    assert_eq!(health["run_id"], json!(run.id));
    assert_eq!(health["status"], "running");
    assert_eq!(health["workspace_id"], "ws-1");
    assert_eq!(health["lease"]["stale"], false);
    assert_eq!(health["lease"]["alive"], true);
    assert_eq!(health["worktree_exists"], true);
    assert_eq!(health["run_dir_exists"], true);
    assert_eq!(health["receipt_exists"], false);
    assert_eq!(health["recoverable"], false);
    let processes = health["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["alive"] == true));
    let error = format!("{:#}", runtime::recover(&db, &run.id).unwrap_err());
    assert!(
        error.contains("wrapper pid") && error.contains("lease heartbeat"),
        "{error}"
    );
    assert!(queue.transition(1, TaskAction::Ready).is_err());

    // The supervisor is gone (stale heartbeat, dead PID) but the session is not.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    raw.execute("UPDATE run_processes SET heartbeat_at=0", [])
        .unwrap();
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["supervisors"][0]["alive"], false);
    assert_eq!(report["runs"][0]["lease"]["stale"], true);
    assert_eq!(report["runs"][0]["processes"][0]["heartbeat_stale"], true);
    let error = format!("{:#}", runtime::recover(&db, &run.id).unwrap_err());
    assert!(
        error.contains("agent pid") && !error.contains("supervisor"),
        "{error}"
    );
    assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Running);
    assert!(queue.run_lease(&run.id).unwrap().is_some());

    // The session processes are gone too: recovery is allowed and explicit.
    agent.kill().unwrap();
    agent.wait().unwrap();
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][0]["blockers"], json!([]));
    let outcome = runtime::recover(&db, &run.id).unwrap();
    assert_eq!(outcome["outcome"], "recovered");
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status, RunStatus::Interrupted);
    assert!(queue.run_leases().unwrap().is_empty());
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["previous_status"], "running");
    assert_eq!(recovered.payload["lease_deleted"], true);
    assert_eq!(recovered.payload["run"]["processes"][0]["alive"], false);
    assert_eq!(recovered.payload["run"]["lease"]["stale"], true);
    // Registrations and resources are left as observed.
    assert!(detail.processes.iter().all(|p| p.exited_at.is_none()));
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
    assert!(runtime::recover(&db, &run.id).is_err()); // No longer unfinished.
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));
    assert!(queue.candidates().unwrap().is_empty());

    // Retry is a separate decision: ready again, then a second run with new paths.
    queue.transition(1, TaskAction::Ready).unwrap();
    assert_eq!(queue.candidates().unwrap().len(), 1);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].status, RunStatus::Interrupted);
    assert_eq!(detail.runs[0].worktree_path, run.worktree_path);
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
    assert_ne!(detail.runs[1].worktree_path, run.worktree_path);
    assert!(queue.transition(1, TaskAction::Ready).is_err()); // Awaiting integration still owns the task.
}

#[test]
fn recover_ignores_exited_processes_and_tolerates_a_missing_lease() {
    let (_dir, repo, db) = fixture();
    // The wrapper reported its exit before the supervisor died; its live PID
    // (this test process) must not block recovery.
    let pid = std::process::id();
    let run = orphan_run(&repo, &db, "owner", pid, pid);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.wrapper_exited(&run.id, pid, 0).unwrap();
    let error = format!("{:#}", runtime::recover(&db, &run.id).unwrap_err());
    assert!(
        error.contains("supervisor pid") && !error.contains("wrapper pid"),
        "{error}"
    );
    let report = runtime::doctor(&db).unwrap();
    assert!(
        report["runs"][0]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["alive"].is_null() && p["heartbeat_stale"] == false)
    );
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(runtime::doctor(&db).unwrap()["supervisors"], json!([]));
    let outcome = runtime::recover(&db, &run.id).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(1).unwrap();
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["lease_deleted"], false);
    assert_eq!(recovered.payload["run"]["lease"], Value::Null);
    // The task can be edited again before a retry.
    assert!(queue.add_dependency(1, 1).is_err());
    queue.transition(1, TaskAction::Draft).unwrap();
    assert_eq!(queue.show(1).unwrap().task.status, TaskStatus::Draft);
    assert!(runtime::recover(&db, "no-such-run").is_err());
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn integrate(db: &Path, task_id: i64, repo: &Path) -> Result<Value> {
    runtime::integrate(db, IntegrateTarget::Task(task_id), repo)
}

fn integrate_next(db: &Path, repo: &Path) -> Value {
    runtime::integrate(db, IntegrateTarget::Next, repo).unwrap()
}

/// A task whose fake agent commits `file` with `content`; verification
/// commands default to the fixture's `test -f seed.txt`.
fn add_file_task(
    queue: &mut SqliteQueue,
    backend: &TestWorkspace,
    title: &str,
    file: &str,
    content: &str,
    verify: &[&str],
) -> i64 {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: verify.iter().map(|v| (*v).to_owned()).collect(),
            dependencies: vec![],
        })
        .unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
    backend.script_for(
        task.id,
        &format!(
            "printf '{content}\\n' > '{file}' && git add '{file}' && git commit -q -m '{title}'; receipt \"$(git rev-parse HEAD)\""
        ),
    );
    task.id
}

/// Rewrite the run's receipt the way a resumed session would after its work.
fn write_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) {
    let path = Path::new(run.receipt_path.as_ref().unwrap());
    let text = json!({
        "run_id": run.id, "result": result, "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "reran"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "session"},
        "summary": summary,
    });
    fs::write(path.with_extension("tmp"), text.to_string()).unwrap();
    fs::rename(path.with_extension("tmp"), path).unwrap();
}

/// Assert that `main` is linear, `commits` long on top of `seed`, and that
/// its head carries the landing of `run` with the same tree as `source`.
fn assert_landed(repo: &Path, run: &TaskRun, task_title: &str, expected_parent: &str) {
    let main = git_out(repo, &["rev-parse", "main"]);
    assert_eq!(run.status, RunStatus::Integrated);
    assert_eq!(run.result_commit.as_deref(), Some(main.as_str()));
    assert_eq!(git_out(repo, &["rev-parse", "main^"]), expected_parent);
    assert_eq!(
        git_out(repo, &["rev-list", "--parents", "-1", "main"])
            .split(' ')
            .count(),
        2
    );
    let history = format!("refs/taskq/runs/{}", run.id);
    let source = git_out(repo, &["rev-parse", &history]);
    assert_eq!(
        git_out(repo, &["rev-parse", "main^{tree}"]),
        git_out(repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(repo, &["log", "-1", "--format=%B", "main"]);
    assert!(message.starts_with(task_title), "{message}");
    assert!(message.contains("\n\nTaskq-Task: "), "{message}");
    assert!(
        message.ends_with(&format!("Taskq-Run: {}", run.id)),
        "{message}"
    );
    // Worktree and branch are gone; the run's history stays under the ref.
    assert!(!Path::new(run.worktree_path.as_ref().unwrap()).exists());
    assert!(
        !git_out(repo, &["branch", "--list", run.branch.as_deref().unwrap()]).contains("taskq/")
    );
}

/// A validated run plus a ready dependent task, before any landing.
fn awaiting_run() -> (TempDir, PathBuf, PathBuf, TaskRun) {
    let (dir, db, detail) = run_agent(VALID_AGENT);
    let run = detail.runs[0].clone();
    assert_eq!(run.status, RunStatus::AwaitingIntegration);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dependent = queue
        .add(NewTask {
            title: "dependent".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            dependencies: vec![1],
        })
        .unwrap();
    queue.transition(dependent.id, TaskAction::Ready).unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let repo = dir.path().join("repo's directory");
    (dir, repo, db, run)
}

#[test]
fn conflict_free_run_lands_as_one_squash_commit_and_releases_dependents() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(seed, run.base_commit);
    let source = run.result_commit.clone().unwrap();
    // Another repository is refused even though it also has a main branch.
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-b", "main"]);
    git(&other, &["config", "user.name", "test"]);
    git(&other, &["config", "user.email", "test@example.invalid"]);
    git(&other, &["commit", "--allow-empty", "-m", "unrelated"]);
    let error = format!("{:#}", integrate(&db, 1, &other).unwrap_err());
    assert!(error.contains("the queue is bound to"), "{error}");
    assert!(integrate(&db, 1, &dir.path().join("missing")).is_err());

    // Landing from the run's own worktree resolves the same repository.
    let worktree = PathBuf::from(run.worktree_path.as_ref().unwrap());
    let outcome = integrate(&db, 1, &worktree).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["task"]["status"], "completed");
    assert_eq!(outcome["run"]["status"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::Completed);
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert!(landed.last_error.is_none());
    // No rebase was needed: the landed tree is the validated tree, and the
    // history ref points at the validated commit.
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/taskq/runs/{}", run.id)]
        ),
        source
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "main^{tree}"]),
        git_out(&repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(&repo, &["log", "-1", "--format=%B", "main"]);
    assert_eq!(
        message,
        format!("test task\n\ndone\n\nTaskq-Task: 1\nTaskq-Run: {}", run.id)
    );
    // The main checkout moved with the ref.
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        landed.result_commit.clone().unwrap()
    );
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\n", run.id)
    );
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(position("validation_finished") < position("integration_started"));
    assert!(position("integration_started") < position("integration_rebased"));
    assert!(position("integration_rebased") < position("verification_command"));
    assert!(position("verification_command") < position("run_integrated"));
    assert!(position("run_integrated") < position("worktree_removed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let integrated = detail
        .events
        .iter()
        .find(|e| e.kind == "run_integrated")
        .unwrap();
    assert_eq!(integrated.run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(
        integrated.payload["result_commit"],
        json!(landed.result_commit)
    );
    assert_eq!(integrated.payload["source_commit"], json!(source));
    assert_eq!(integrated.payload["main_before"], json!(seed));
    assert_eq!(
        integrated.payload["history_ref"],
        json!(format!("refs/taskq/runs/{}", run.id))
    );
    let verification = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "verification_command")
        .unwrap();
    assert_eq!(verification.payload["phase"], "integration");
    assert!(
        verification.payload["log_path"]
            .as_str()
            .unwrap()
            .ends_with("integrate-verify-1.log")
    );
    let changed = detail
        .events
        .iter()
        .find(|e| e.kind == "task_status_changed" && e.payload["to"] == "completed")
        .unwrap();
    assert_eq!(changed.run_id.as_deref(), Some(run.id.as_str()));
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [2]
    );

    // Integration is one-shot, at every layer.
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("no run awaiting integration"), "{error}");
    assert!(queue.begin_integration(&run.id, "x", &seed).is_err());
    let error = format!("{:#}", integrate(&db, 2, &repo).unwrap_err());
    assert!(error.contains("task 2 (ready) has no run"), "{error}");
    assert!(integrate(&db, 99, &repo).is_err());
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let raw = Connection::open(&db).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES ('again',1,'integrated','claude','claude',?1)",
            [&run.base_commit],
        )
        .is_err()
    );
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
}

/// Two accepted runs land in the order their validation finished; the
/// second is rebased onto the first landing and main stays linear with one
/// commit per task, whether or not a checkout has main checked out.
#[test]
fn runs_land_fifo_by_validation_time_and_later_ones_are_rebased() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let a = add_file_task(
        &mut queue,
        &backend,
        "task a",
        "a.txt",
        "a",
        &["test -f seed.txt"],
    );
    let b = add_file_task(
        &mut queue,
        &backend,
        "task b",
        "b.txt",
        "b",
        &["test -f seed.txt"],
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let run_a = queue.show(a).unwrap().runs[0].clone();
    let run_b = queue.show(b).unwrap().runs[0].clone();
    // FIFO is by validation time, not task id.
    let mut validated_at = |run: &TaskRun| {
        queue
            .show(run.task_id)
            .unwrap()
            .events
            .iter()
            .find(|e| e.kind == "validation_finished")
            .unwrap()
            .id
    };
    let (first, second) = if validated_at(&run_a) < validated_at(&run_b) {
        (run_a.clone(), run_b.clone())
    } else {
        (run_b.clone(), run_a.clone())
    };
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id,
        first.id
    );

    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(first.id));
    let first_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id,
        second.id
    );

    // No checkout has main now: the ref is updated directly.
    git(&repo, &["checkout", "-q", "--detach"]);
    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(second.id));
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let second_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), first_landed); // Detached HEAD untouched.
    git(&repo, &["checkout", "-q", "main"]);

    let landed_second = queue.show(second.task_id).unwrap().runs[0].clone();
    assert_landed(&repo, &landed_second, "task ", &first_landed);
    let landed_first = queue.show(first.task_id).unwrap().runs[0].clone();
    assert_eq!(
        landed_first.result_commit.as_deref(),
        Some(first_landed.as_str())
    );
    // Linear: seed → first → second, one commit per task, both files present.
    assert_eq!(
        git_out(&repo, &["rev-list", "--first-parent", "main"])
            .lines()
            .collect::<Vec<_>>(),
        [second_landed.as_str(), first_landed.as_str(), seed.as_str()]
    );
    assert!(repo.join("a.txt").exists() && repo.join("b.txt").exists());
    // The second run was rebased: its history ref sits on the first landing.
    let history = git_out(
        &repo,
        &["rev-parse", &format!("refs/taskq/runs/{}", second.id)],
    );
    assert_ne!(history, second.result_commit.clone().unwrap());
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{history}^")]),
        first_landed
    );
    let rebased = queue
        .show(second.task_id)
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["main"], json!(first_landed));
    assert_eq!(rebased.payload["head_before"], json!(second.result_commit));
    assert_eq!(rebased.payload["head_after"], json!(history));
    for task in [a, b] {
        assert_eq!(queue.show(task).unwrap().task.status, TaskStatus::Completed);
    }
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Both runs change the same file: the second cannot be rebased by the
/// runtime and waits for a session, which resolves, reruns verification and
/// rewrites the receipt; then it lands like any other run.
#[test]
fn conflicting_run_needs_a_session_and_lands_after_the_session_resolves_it() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(&repo, &["rev-parse", "main"]);

    let run = queue.show(2).unwrap().runs[0].clone();
    let source = run.result_commit.clone().unwrap();
    let worktree = PathBuf::from(run.worktree_path.as_ref().unwrap());
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert_eq!(outcome["main"], json!(first_landed));
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("conflicted in change.txt"), "{reason}");
    assert!(
        reason.contains(&format!("git rebase {first_landed}")),
        "{reason}"
    );
    let parked = queue.show(2).unwrap().runs[0].clone();
    assert_eq!(parked.status, RunStatus::NeedsSession);
    assert_eq!(parked.last_error.as_deref(), Some(reason));
    assert_eq!(parked.result_commit.as_deref(), Some(source.as_str()));
    // The rebase was aborted: the worktree is back on its validated head, clean.
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), source);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(!worktree.join(".git").join("rebase-merge").exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let detail = queue.show(2).unwrap();
    let deferred = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_deferred")
        .unwrap();
    assert_eq!(deferred.payload["status"], "needs_session");
    assert_eq!(deferred.payload["conflicts"], json!(["change.txt"]));
    assert_eq!(deferred.payload["aborted"], true);
    assert!(
        deferred.payload["output_tail"]
            .as_str()
            .unwrap()
            .contains("CONFLICT")
    );
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    // A parked run still owns its task and is not picked by --next.
    assert!(queue.transition(2, TaskAction::Ready).is_err());
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));

    // Nothing changed in the worktree: the runtime tries again and parks it again.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");

    // The session resolves the conflict on top of main.
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rebase", &first_landed])
        .output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(&worktree, &["add", "change.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .status()
        .unwrap();
    assert!(status.success());
    let resolved = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(resolved, source);

    // Until the receipt names the new head, the session is not done.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("receipt commit {source} is not the head")),
        "{reason}"
    );
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), resolved); // Left as the session made it.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);

    write_receipt(&parked, &resolved, "succeeded", "resolved");
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(2).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/taskq/runs/{}", run.id)]
        ),
        resolved
    );
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the session\n"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%b", "main"])
            .lines()
            .next(),
        Some("resolved")
    );
    assert_eq!(queue.show(2).unwrap().task.status, TaskStatus::Completed);
    // The parked attempts were before the landing; the reason is cleared.
    assert!(landed.last_error.is_none());
    let detail = queue.show(2).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_deferred")
            .count(),
        3
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_started")
            .count(),
        4
    );
    assert_eq!(kinds.iter().filter(|k| **k == "run_integrated").count(), 1);
}

/// A session that finds the change no longer needed writes a failed receipt
/// with the reason; the run ends without touching main and the task can be
/// retried or canceled.
#[test]
fn failed_receipt_from_a_session_ends_the_run_without_landing() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let run = queue.show(2).unwrap().runs[0].clone();
    write_receipt(
        &run,
        run.result_commit.as_deref().unwrap(),
        "failed",
        "already covered by task 1",
    );
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "failed", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("already covered by task 1"), "{reason}");
    let failed = queue.show(2).unwrap().runs[0].clone();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(failed.last_error.as_deref(), Some(reason));
    assert!(Path::new(failed.worktree_path.as_ref().unwrap()).exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    assert!(
        git_out(&repo, &["for-each-ref", "refs/taskq/runs/"])
            .lines()
            .count()
            == 1
    );
    assert_eq!(queue.show(2).unwrap().task.status, TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(event_kinds(&queue.show(2).unwrap()).contains(&"integration_failed"));
    // Retry or give up is the operator's call, as after any failed run.
    queue.transition(2, TaskAction::Cancel).unwrap();
}

/// The rebase applies cleanly but the earlier landing broke this run's
/// verification (a semantic conflict): the run is parked with the rebased
/// tree in place so a session can fix it on top of main.
#[test]
fn verification_failure_after_rebase_needs_a_session_and_keeps_the_rebased_tree() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let breaker = queue
        .add(NewTask {
            title: "drop seed".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec!["true".into()],
            dependencies: vec![],
        })
        .unwrap()
        .id;
    queue.transition(breaker, TaskAction::Ready).unwrap();
    backend.script_for(
        breaker,
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let victim = add_file_task(
        &mut queue,
        &backend,
        "needs seed",
        "v.txt",
        "v",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        integrate(&db, breaker, &repo).unwrap()["outcome"],
        "integrated"
    );
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert!(!repo.join("seed.txt").exists());

    let run = queue.show(victim).unwrap().runs[0].clone();
    let outcome = integrate(&db, victim, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("\"test -f seed.txt\" exited with 1 after the rebase"),
        "{reason}"
    );
    let worktree = PathBuf::from(run.worktree_path.as_ref().unwrap());
    let head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(head, run.result_commit.clone().unwrap());
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD^"]), main);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(
        Path::new(run.run_dir.as_ref().unwrap())
            .join("integrate-verify-1.log")
            .exists()
    );
    let parked = queue.show(victim).unwrap().runs[0].clone();
    assert_eq!(parked.status, RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    // The session restores what the verification needs and reports the new head.
    fs::write(worktree.join("seed.txt"), "restored\n").unwrap();
    git(&worktree, &["add", "seed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "restore seed"]);
    write_receipt(
        &parked,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "restored seed",
    );
    assert_eq!(
        integrate(&db, victim, &repo).unwrap()["outcome"],
        "integrated"
    );
    let landed = queue.show(victim).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "needs seed", &main);
    assert!(repo.join("seed.txt").exists() && repo.join("v.txt").exists());
}

/// One run lands at a time. An `integrate` process that dies leaves the run
/// `integrating` with a stale lease; `recover` puts it back in the queue
/// instead of interrupting it, and the next `integrate` lands it. A landing
/// that cannot fast-forward the main checkout gives the slot back too.
#[test]
fn integration_slot_is_exclusive_and_an_abandoned_landing_is_recoverable() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let other = add_file_task(
        &mut queue,
        &backend,
        "other",
        "o.txt",
        "o",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(1).unwrap().runs[0].clone();
    let seed = git_out(&repo, &["rev-parse", "main"]);

    // Take the slot by hand, as a crashed `integrate` would have.
    let taken = queue.begin_integration(&run.id, "crashed", &seed).unwrap();
    assert_eq!(taken.status, RunStatus::Integrating);
    assert!(queue.transition(1, TaskAction::Ready).is_err());
    let error = format!("{:#}", integrate(&db, other, &repo).unwrap_err());
    assert!(
        error.contains(&format!("run {} is integrating", run.id)),
        "{error}"
    );
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("is already integrating"), "{error}");
    assert!(queue.begin_integration(&run.id, "again", &seed).is_err());
    assert_eq!(
        queue.show(other).unwrap().runs[0].status,
        RunStatus::AwaitingIntegration
    );
    // It is visible while alive, and recoverable once its process is gone.
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["runs"][0]["run_id"], json!(run.id));
    assert_eq!(report["runs"][0]["status"], "integrating");
    assert_eq!(report["runs"][0]["recoverable"], false);
    assert!(runtime::recover(&db, &run.id).is_err());
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db).unwrap()["runs"][0]["recoverable"],
        true
    );
    let recovered = runtime::recover(&db, &run.id).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let event = queue
        .show(1)
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(event.payload["previous_status"], "integrating");
    assert_eq!(event.payload["status"], "awaiting_integration");
    assert!(queue.run_leases().unwrap().is_empty());

    // A local change in the main checkout that collides with the landing
    // makes the fast-forward fail; the run goes back to the queue.
    fs::write(repo.join("change.txt"), "uncommitted local edit\n").unwrap();
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("fast-forward main"), "{error}");
    assert!(
        error.contains("returned to awaiting_integration"),
        "{error}"
    );
    let returned = queue.show(1).unwrap().runs[0].clone();
    assert_eq!(returned.status, RunStatus::AwaitingIntegration);
    assert!(
        returned
            .last_error
            .as_ref()
            .unwrap()
            .contains("before main moved")
    );
    assert!(event_kinds(&queue.show(1).unwrap()).contains(&"integration_error"));
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), seed);
    assert!(queue.run_leases().unwrap().is_empty());
    fs::remove_file(repo.join("change.txt")).unwrap();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = queue.show(1).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert_eq!(
        integrate(&db, other, &repo).unwrap()["outcome"],
        "integrated"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
}

const IDLE_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Two independent tasks execute at the same time under one resident
/// supervisor; the dependent task waits for `integrate` and is then picked up
/// by the same loop with the new `main` as its base.
#[test]
fn independent_tasks_run_concurrently_and_a_dependent_starts_after_integration() {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "independent", &[]);
    add_ready_task(&mut queue, "dependent", &[1]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = SuperviseOptions::new(4, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };

    // Both independent runs are alive at once; the dependent has none.
    wait_until(&db, Duration::from_secs(20), |queue| {
        let running: Vec<TaskRun> = queue
            .active_runs()
            .unwrap()
            .into_iter()
            .filter(|r| r.status == RunStatus::Running)
            .collect();
        running.len() == 2
            && running
                .iter()
                .all(|r| queue.run_lease(&r.id).unwrap().is_some())
    });
    assert!(queue.show(3).unwrap().runs.is_empty());
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        status["supervisors"][0]["run_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(status["runs"].as_array().unwrap().len(), 2);
    assert!(status["runs"][0]["lease"]["pid"].is_number());
    let doctor = runtime::doctor(&db).unwrap();
    assert_eq!(doctor["runs"].as_array().unwrap().len(), 2);
    assert!(
        doctor["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false)
    );

    wait_until(&db, Duration::from_secs(30), |queue| {
        [1, 2]
            .iter()
            .all(|task| queue.show(*task).unwrap().runs[0].status == RunStatus::AwaitingIntegration)
    });
    // Awaiting integration does not satisfy the dependency; the loop idles.
    thread::sleep(Duration::from_secs(3));
    assert!(queue.show(3).unwrap().runs.is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    let first = queue.show(1).unwrap().runs[0].clone();
    let second = queue.show(2).unwrap().runs[0].clone();
    assert_ne!(first.workspace_id, second.workspace_id);
    assert_eq!(first.base_commit, second.base_commit);
    assert!(queue.run_leases().unwrap().is_empty());

    // Landing unblocks the dependent; the resident loop claims it from the landed main.
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_ne!(landed, first.result_commit.clone().unwrap());
    assert_eq!(
        queue.show(1).unwrap().runs[0].result_commit.as_deref(),
        Some(landed.as_str())
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .show(3)
            .unwrap()
            .runs
            .first()
            .is_some_and(|r| r.status == RunStatus::AwaitingIntegration)
    });
    let third = queue.show(3).unwrap().runs[0].clone();
    assert_eq!(third.base_commit, landed);
    assert_ne!(third.base_commit, second.base_commit);

    // A graceful stop ends the loop once nothing is active.
    options.stop.store(true, Ordering::SeqCst);
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 3);
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1), workspace_id(2)]);
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));
    // The independent second run conflicts with the first (same file) and
    // waits for a session; the dependent, built on the landing, lands cleanly.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(integrate(&db, 3, &repo).unwrap()["outcome"], "integrated");
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{landed}..main")]),
        "1"
    );
    drop(dir);
}

/// A run that never answers the exit request is given up without disturbing
/// the run next to it, and can be recovered by itself once its session ends.
#[test]
fn a_timed_out_run_is_abandoned_while_the_other_run_is_accepted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "healthy", &[]);
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(
        1,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 6",
    );
    backend.exit_timeout = Duration::from_secs(2);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    let stuck = queue.show(1).unwrap().runs[0].clone();
    let healthy = queue.show(2).unwrap().runs[0].clone();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["runs"][0]["id"], json!(healthy.id));
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["errors"][0]["run_id"], json!(stuck.id));
    assert_eq!(outcome["errors"][0]["task_id"], 1);
    assert_eq!(healthy.status, RunStatus::AwaitingIntegration);
    assert!(healthy.last_error.is_none());
    assert!(healthy.workspace_closed_at.is_some());
    assert_eq!(stuck.status, RunStatus::Running);
    assert!(stuck.last_error.as_ref().unwrap().contains("did not exit"));
    assert!(queue.run_leases().unwrap().is_empty());
    // Only the stuck run is unfinished; its live wrapper still blocks recovery.
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["runs"].as_array().unwrap().len(), 1);
    assert_eq!(report["runs"][0]["run_id"], json!(stuck.id));
    assert_eq!(report["runs"][0]["recoverable"], false);
    backend.join();
    assert_eq!(
        runtime::recover(&db, &stuck.id).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(
        queue.show(2).unwrap().runs[0].status,
        RunStatus::AwaitingIntegration
    );
}

/// A nonzero session exit and a rejected receipt in one pass leave the
/// accepted run untouched; every run releases its lease.
#[test]
fn failed_runs_in_the_same_pass_do_not_affect_the_accepted_run() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "crashes", &[]);
    add_ready_task(&mut queue, "no receipt", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(2, "commit work; exit 7");
    backend.script_for(3, "commit work");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    let mut statuses: Vec<(i64, String)> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["task_id"].as_i64().unwrap(),
                r["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        [
            (1, "awaiting_integration".to_owned()),
            (2, "failed".to_owned()),
            (3, "failed".to_owned())
        ]
    );
    assert!(
        queue.show(3).unwrap().runs[0]
            .last_error
            .as_ref()
            .unwrap()
            .contains("receipt was not submitted")
    );
    assert_eq!(backend.closed(), [workspace_id(0)]);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));
    // Failed tasks can be retried independently; the accepted one still owns its slot.
    queue.transition(2, TaskAction::Ready).unwrap();
    assert!(queue.transition(1, TaskAction::Ready).is_err());
    assert_eq!(queue.candidates().unwrap()[0].id, 2);
}

/// Recovering one orphaned run touches neither the lease nor the processes of
/// the run that shares its supervisor.
#[test]
fn recovering_one_orphaned_run_leaves_the_other_running() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "still alive", &[]);
    let mut dead_wrapper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut dead_agent = Command::new("sleep").arg("60").spawn().unwrap();
    let mut live_wrapper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut live_agent = Command::new("sleep").arg("60").spawn().unwrap();
    let orphan = orphan_run(&repo, &db, "owner", dead_wrapper.id(), dead_agent.id());
    let survivor = orphan_run(&repo, &db, "owner", live_wrapper.id(), live_agent.id());
    // One supervisor, two leases; it died and took nothing with it.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["supervisors"][0]["run_ids"],
        json!([orphan.id, survivor.id])
    );
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["runs"].as_array().unwrap().len(), 2);
    assert!(
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false && r["lease"]["stale"] == true)
    );
    for child in [&mut dead_wrapper, &mut dead_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][1]["recoverable"], false);
    assert_eq!(
        runtime::recover(&db, &orphan.id).unwrap()["run"]["status"],
        "interrupted"
    );
    // The survivor keeps its lease, processes and status; only the orphan changed.
    assert_eq!(queue.run(&survivor.id).unwrap().status, RunStatus::Running);
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, survivor.id);
    assert_eq!(queue.processes(&survivor.id).unwrap().len(), 2);
    let error = format!("{:#}", runtime::recover(&db, &survivor.id).unwrap_err());
    assert!(error.contains("wrapper pid"), "{error}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["runs"][0]["run_id"], json!(survivor.id));
    for child in [&mut live_wrapper, &mut live_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(
        runtime::recover(&db, &survivor.id).unwrap()["run"]["status"],
        "interrupted"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}
