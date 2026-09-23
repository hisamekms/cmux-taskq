use anyhow::{Result, bail};
use dagq::{
    VERSION,
    application::{AgentProvider, SupervisorEnvironment, TaskStore, WorkspaceBackend},
    domain::{GoalEdit, NewGoal, NewTask, RunStatus, TaskAction, TaskRun, TaskStatus},
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
            goal_id: None,
            context: String::new(),
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
        // Every context section is present whether or not it has entries.
        assert!(prompt.contains("Goal"));
        assert!(prompt.contains("Context"));
        assert!(prompt.contains("Predecessor tasks"));
        assert!(prompt.contains("Sibling tasks in progress"));
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
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        unreachable!("only up preflights the detached connection")
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
    // The maintainer workspace is `up`'s business; the supervisor never asks.
    fn find_named(&self, _: &str) -> Result<Option<String>> {
        bail!("not used by the supervisor")
    }
    fn create_named(&self, _: &str, _: &Path, _: &str) -> Result<String> {
        bail!("not used by the supervisor")
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
        Path::new(env!("CARGO_BIN_EXE_dagq")),
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
fn run_agent(script: &str) -> (TempDir, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with(script, false)
}

fn run_agent_with(script: &str, close_fail: bool) -> (TempDir, PathBuf, dagq::domain::TaskDetail) {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, script);
    backend.close_fail = close_fail;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    // These scripts exit on their own, like a maintainer's /exit; nothing was requested.
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
/// The prompt the run's agent was started with, as `provision` wrote it.
fn read_prompt(run: &TaskRun) -> String {
    fs::read_to_string(Path::new(run.run_dir.as_ref().unwrap()).join("prompt.txt")).unwrap()
}

fn rejection_reason(detail: &dagq::domain::TaskDetail) -> String {
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
    // The only task has no goal, no context, no predecessor, and nothing else was in progress.
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Goal: none, this task stands alone\n"),
        "{prompt}"
    );
    assert!(prompt.contains("Context: none\n"), "{prompt}");
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
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
    assert!(rejection_reason(&detail).contains("is not the head of dagq/"));
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
    use dagq::domain::Receipt;
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
    // follow_ups is optional and only its shape is checked.
    let without = Receipt::parse(valid).unwrap();
    assert!(without.follow_ups.is_none());
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("follow_ups")
    );
    let with = valid.replace(
        "\"summary\":\"ok\"",
        "\"summary\":\"ok\",\"follow_ups\":[{\"title\":\"next\",\"description\":\"later\"}]",
    );
    let receipt = Receipt::parse(&with).unwrap();
    receipt.check("r").unwrap();
    assert_eq!(receipt.follow_ups.as_ref().unwrap()[0]["title"], "next");
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["follow_ups"][0]["description"],
        "later"
    );
    Receipt::parse(&valid.replace("\"summary\":\"ok\"", "\"summary\":\"ok\",\"follow_ups\":[]"))
        .unwrap()
        .check("r")
        .unwrap();
    let error = format!(
        "{:#}",
        Receipt::parse(&valid.replace(
            "\"summary\":\"ok\"",
            "\"summary\":\"ok\",\"follow_ups\":{\"title\":\"next\"}"
        ))
        .unwrap()
        .check("r")
        .unwrap_err()
    );
    assert!(error.contains("follow_ups must be an array"), "{error}");
}

fn event_kinds(detail: &dagq::domain::TaskDetail) -> Vec<&str> {
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
    // Both sessions end by themselves, as with a maintainer's /exit.
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
    use dagq::infrastructure::adapters::{ClaudeCode, stop_hook_settings};
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run's dir");
    fs::create_dir(&run_dir).unwrap();
    let run = TaskRun {
        id: "11111111-2222-4333-8444-555555555555".into(),
        task_id: 1,
        status: RunStatus::Starting,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        branch: Some("dagq/x".into()),
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
    // A failed run does not free the task automatically, but the maintainer may give up on it.
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
    // The run is disowned, so nothing has to be stopped before recovering it;
    // the drained loop took its registration with it.
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisors"], json!([]));
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(
        runtime::recover(&db, &run.id).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
}

#[test]
fn claim_creates_a_lease_that_only_its_owner_can_use_or_release() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
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
        branch: "dagq/test".into(),
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
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(queue.show(1).unwrap().task.title, "preserved");
    assert!(queue.run_leases().unwrap().is_empty());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_stale_owners() {
    use dagq::{
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
                branch: "dagq/test".into(),
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
    use dagq::{
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
                branch: "dagq/test".into(),
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
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(1).unwrap().runs.is_empty());
    assert!(supervise_with(&db, &repo, &backend, &SuperviseOptions::new(0, true)).is_err());
    assert!(queue.supervisors().unwrap().is_empty());
}

/// A resident supervisor that holds no run is still listed by `status` and
/// `doctor` through its registration, which its heartbeat refreshes and a
/// graceful stop removes.
#[test]
fn resident_supervisor_without_runs_is_listed_until_it_stops() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    let backend = Arc::new(TestWorkspace::new(&db, true, VALID_AGENT));
    let options = SuperviseOptions::new(3, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    let registered = queue.supervisors().unwrap().remove(0);
    assert_eq!(registered.pid, std::process::id());
    assert_eq!(registered.parallel, 3);
    for report in [runtime::status(&db).unwrap(), runtime::doctor(&db).unwrap()] {
        assert_eq!(report["runs"], json!([]));
        assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
        let entry = &report["supervisors"][0];
        assert_eq!(entry["pid"], json!(std::process::id()));
        assert_eq!(entry["alive"], true);
        assert_eq!(entry["registered"], true);
        assert_eq!(entry["parallel"], 3);
        assert_eq!(entry["started_at"], json!(registered.started_at));
        assert_eq!(entry["stale"], false);
        assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
        assert_eq!(entry["run_ids"], json!([]));
    }
    // The heartbeat keeps the registration fresh while nothing runs.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap()[0].heartbeat_at > 0
    });
    assert!(queue.run_leases().unwrap().is_empty());

    options.stop.store(true, Ordering::SeqCst);
    let outcome = supervisor.join().unwrap().unwrap();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    assert_eq!(runtime::doctor(&db).unwrap()["supervisors"], json!([]));

    // An error out of the loop itself (here: main vanished before a claim)
    // ends the process with nothing active, so it deregisters too.
    let options = SuperviseOptions::new(1, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    queue.transition(1, TaskAction::Ready).unwrap();
    let error = format!("{:#}", supervisor.join().unwrap().unwrap_err());
    assert!(error.contains("Needed a single revision"), "{error}");
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(1).unwrap().runs.is_empty());
}

/// `--log-dir` adds one file per supervisor start, named by the
/// registration's `started_at` and the PID, with the startup facts, the
/// progress messages that also go to stderr, and the final result.
#[test]
fn supervise_log_dir_records_each_start_in_its_own_file() {
    let (dir, repo, db) = fixture();
    let log_dir = dir.path().join("logs").join("nested");
    let mut options = SuperviseOptions::new(2, true);
    options.log_dir = Some(log_dir.clone());
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let before = runtime::unix_time();
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    let pid = std::process::id();
    let logs = |pid: u32| -> Vec<PathBuf> {
        let mut logs: Vec<PathBuf> = fs::read_dir(&log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_str().unwrap();
                name.starts_with("supervisor-") && name.ends_with(&format!("-{pid}.log"))
            })
            .collect();
        logs.sort();
        logs
    };
    let first = logs(pid);
    assert_eq!(first.len(), 1, "{first:?}");
    let name = first[0].file_name().unwrap().to_str().unwrap();
    let started_at: i64 = name
        .strip_prefix("supervisor-")
        .unwrap()
        .strip_suffix(&format!("-{pid}.log"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(started_at >= before && started_at <= runtime::unix_time());
    let text = fs::read_to_string(&first[0]).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(1).unwrap().runs.remove(0);
    let token: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        text.contains(&format!(
            "] supervisor {token} started: version {VERSION}, pid {pid}, parallel 2, db {}, repository {}",
            db.canonicalize().unwrap().display(),
            repo.canonicalize().unwrap().display()
        )),
        "{text}"
    );
    assert!(text.contains(&format!(
        "task 1 running in workspace {WORKSPACE_ID}; run {}",
        run.id
    )));
    assert!(text.contains(&format!("receipt received for {}", run.id)));
    assert!(text.contains(&format!("run {} is awaiting_integration", run.id)));
    assert!(text.contains(&format!(
        "] supervisor {token} exiting: {{\"errors\":[],\"outcome\":\"finished\""
    )));

    // A second start gets its own file (same second or not, the name differs by token order at worst).
    thread::sleep(Duration::from_millis(1100));
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    let second = logs(pid);
    assert_eq!(second.len(), 2, "{second:?}");
    let text = fs::read_to_string(second.iter().find(|p| *p != &first[0]).unwrap()).unwrap();
    assert!(text.contains(&format!("started: version {VERSION}, pid")));
    assert!(!text.contains("task 1 running"));

    // A log directory that cannot be created is a startup failure that
    // leaves no registration behind (launchd would retry forever).
    options.log_dir = Some(dir.path().join("seed-file-as-dir"));
    fs::write(options.log_dir.as_ref().unwrap(), "not a directory").unwrap();
    let error = supervise_with(&db, &repo, &backend, &options).unwrap_err();
    assert!(format!("{error:#}").contains("create"), "{error:#}");
    assert!(queue.supervisors().unwrap().is_empty());
}

/// A registration whose process died, or whose heartbeat stopped, is
/// reported as stale by `status` and `doctor` and left for the maintainer;
/// neither a later supervisor nor `recover` removes it, and an `integrate`
/// or orphaned lease holder is listed next to it without a registration.
#[test]
fn killed_supervisor_registration_is_reported_stale_and_never_deleted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dead = dead_pid();
    let killed = queue
        .register_supervisor("killed", dead, 4, VERSION)
        .unwrap();
    // Killed a moment ago: the heartbeat is fresh, the pid is gone.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"], json!([]));
    let entry = &status["supervisors"][0];
    assert_eq!(entry["pid"], json!(dead));
    assert_eq!(entry["alive"], false);
    assert_eq!(entry["stale"], true);
    assert_eq!(entry["registered"], true);
    assert_eq!(entry["parallel"], 4);
    // The build the process ran, which `up` compares against its own.
    assert_eq!(entry["binary_version"], VERSION);
    assert_eq!(entry["heartbeat_at"], json!(killed.heartbeat_at));
    assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
    // Alive but silent: stale by heartbeat age alone.
    queue
        .register_supervisor("hung", std::process::id(), 1, VERSION)
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE supervisors SET heartbeat_at=1700000000 WHERE token='hung'",
        [],
    )
    .unwrap();
    drop(raw);
    let doctor = runtime::doctor(&db).unwrap();
    assert_eq!(doctor["supervisors"].as_array().unwrap().len(), 2);
    let hung = &doctor["supervisors"][1];
    assert_eq!(hung["alive"], true);
    assert_eq!(hung["stale"], true);
    assert!(hung["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(hung["run_ids"], json!([]));

    // A run owned without a registration (an `integrate` process, or a
    // supervisor from before the registry) is still attributed to its lease.
    let orphan = orphan_run(&repo, &db, "owner", std::process::id(), std::process::id());
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], false);
    assert_eq!(supervisors[2]["parallel"], Value::Null);
    // A lease holder without a registration recorded no version either.
    assert_eq!(supervisors[2]["binary_version"], Value::Null);
    assert_eq!(supervisors[2]["started_at"], Value::Null);
    assert_eq!(supervisors[2]["pid"], json!(std::process::id()));
    assert_eq!(supervisors[2]["alive"], true);
    assert_eq!(supervisors[2]["stale"], false);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id]));
    assert_eq!(status["runs"][0]["lease"]["pid"], json!(std::process::id()));
    // A registered supervisor's leases join it by token rather than by pid.
    queue
        .register_supervisor("owner", std::process::id(), 2, VERSION)
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], true);
    assert_eq!(supervisors[2]["parallel"], 2);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id]));

    // Recovery of the run and a later supervisor's own registration and
    // deregistration leave the stale rows alone.
    queue
        .wrapper_exited(&orphan.id, std::process::id(), 0)
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::recover(&db, &orphan.id).unwrap()["run"]["status"],
        "interrupted"
    );
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "finished"
    );
    let tokens: Vec<String> = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .map(|s| s.token)
        .collect();
    assert_eq!(tokens, ["killed", "hung", "owner"]);
    assert_eq!(
        runtime::doctor(&db).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
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
    use dagq::{
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
    let run_dir = dagq::infrastructure::location::runs_dir(db).join(&run.id);
    fs::create_dir_all(&run_dir).unwrap();
    queue
        .plan_run(
            &run.id,
            token,
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
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
            goal_id: None,
            context: String::new(),
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
    write_receipt_json(run, session_receipt(run, commit, result, summary));
}

/// A session's receipt for `commit`, with the evidence it would give.
fn session_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) -> Value {
    json!({
        "run_id": run.id, "result": result, "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "reran"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "session"},
        "summary": summary,
    })
}

fn write_receipt_json(run: &TaskRun, receipt: Value) {
    let path = Path::new(run.receipt_path.as_ref().unwrap());
    fs::write(path.with_extension("tmp"), receipt.to_string()).unwrap();
    fs::rename(path.with_extension("tmp"), path).unwrap();
}

/// The payloads of the `verification_command` events the landing recorded
/// (`phase: integration`), oldest first. Validation's own runs of the same
/// commands carry no `phase`.
fn integration_verifications(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "verification_command" && e.payload["phase"] == "integration")
        .map(|e| &e.payload)
        .collect()
}

/// The `integration_receipt` events of the task's run, oldest first.
fn integration_receipts(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "integration_receipt")
        .map(|e| &e.payload)
        .collect()
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
    let history = format!("refs/dagq/runs/{}", run.id);
    let source = git_out(repo, &["rev-parse", &history]);
    assert_eq!(
        git_out(repo, &["rev-parse", "main^{tree}"]),
        git_out(repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(repo, &["log", "-1", "--format=%B", "main"]);
    assert!(message.starts_with(task_title), "{message}");
    assert!(message.contains("\n\nDagq-Task: "), "{message}");
    assert!(
        message.ends_with(&format!("Dagq-Run: {}", run.id)),
        "{message}"
    );
    // Worktree and branch are gone; the run's history stays under the ref.
    assert!(!Path::new(run.worktree_path.as_ref().unwrap()).exists());
    assert!(
        !git_out(repo, &["branch", "--list", run.branch.as_deref().unwrap()]).contains("dagq/")
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
            goal_id: None,
            context: String::new(),
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
    // Main has not moved, so the rebase was a no-op and the verification
    // commands were not run a second time on the validated tree.
    assert_eq!(outcome["verification_skipped"], json!(true), "{outcome}");
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
        git_out(&repo, &["rev-parse", &format!("refs/dagq/runs/{}", run.id)]),
        source
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "main^{tree}"]),
        git_out(&repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(&repo, &["log", "-1", "--format=%B", "main"]);
    assert_eq!(
        message,
        format!("test task\n\ndone\n\nDagq-Task: 1\nDagq-Run: {}", run.id)
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
    assert!(position("integration_rebased") < position("integration_verification_skipped"));
    assert!(position("integration_verification_skipped") < position("run_integrated"));
    // The only verification commands are validation's, before the landing.
    assert!(position("verification_command") < position("integration_started"));
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
        json!(format!("refs/dagq/runs/{}", run.id))
    );
    assert_eq!(integrated.payload["verification_skipped"], json!(true));
    assert!(
        detail
            .events
            .iter()
            .all(|e| e.kind != "verification_command" || e.payload["phase"] != "integration"),
        "{:?}",
        event_kinds(&detail)
    );
    let skipped = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_verification_skipped")
        .unwrap();
    assert_eq!(skipped.run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(skipped.payload["main"], json!(seed));
    assert_eq!(skipped.payload["head"], json!(source));
    assert_eq!(
        skipped.payload["reason"],
        "rebase was a no-op; validation already verified this head"
    );
    assert!(
        !Path::new(run.run_dir.as_ref().unwrap())
            .join("integrate-verify-1.log")
            .exists()
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

/// Move everything the queue at `db` owns (the database with its WAL files,
/// `runs/`, logs) from its directory into `to`, leaving the repository. The
/// runs keep the absolute paths they stored at claim time, as every queue
/// written before ADR-0017 does. Returns the database's new path.
fn move_queue(db: &Path, repo: &Path, to: &Path) -> PathBuf {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(db.parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        if path != repo && path != to {
            fs::rename(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
    to.join(db.file_name().unwrap())
}

#[test]
fn moved_queue_directory_resolves_run_paths_and_lands_awaiting_runs() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let old_runs = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap());
    // An unfinished run next to the awaiting one, for `status` and `doctor`.
    let other = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "unfinished", &[])
    };
    let unfinished = orphan_run(&repo, &db, "old-supervisor", dead_pid(), dead_pid());
    assert_eq!(unfinished.task_id, other);

    let moved = dir.path().join("moved queue");
    let db = move_queue(&db, &repo, &moved);
    let runs = moved.canonicalize().unwrap().join("runs");
    assert!(!old_runs.exists());
    // The database still holds the paths of the old location: nothing is
    // migrated, they are resolved again from the run ID on every read.
    let raw = Connection::open(&db).unwrap();
    let stored: String = raw
        .query_row(
            "SELECT worktree_path FROM task_runs WHERE id=?1",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, run.worktree_path.clone().unwrap());
    assert!(stored.starts_with(old_runs.to_str().unwrap()), "{stored}");
    // Git still records the worktrees at their old paths.
    assert!(git_out(&repo, &["worktree", "list"]).contains("prunable"));

    let text = |path: PathBuf| Some(path.to_str().unwrap().to_owned());
    let expected = |id: &str| dagq::domain::RunPaths::new(&runs, id);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let shown = queue.show(1).unwrap().runs[0].clone();
    let paths = expected(&run.id);
    assert_eq!(shown.run_dir, text(paths.run_dir.clone()));
    assert_eq!(shown.worktree_path, text(paths.worktree.clone()));
    assert_eq!(shown.receipt_path, text(paths.receipt.clone()));
    assert_eq!(shown.log_path, text(paths.log.clone()));
    assert_eq!(shown.repo_path, run.repo_path);
    assert!(paths.worktree.is_dir() && paths.receipt.is_file());

    let status = runtime::status(&db).unwrap();
    let entry = &status["runs"].as_array().unwrap()[0];
    assert_eq!(entry["run_id"], json!(unfinished.id), "{status}");
    assert_eq!(
        entry["worktree_path"],
        json!(text(expected(&unfinished.id).worktree)),
        "{status}"
    );
    let doctor = runtime::doctor(&db).unwrap();
    let health = &doctor["runs"].as_array().unwrap()[0];
    assert_eq!(health["run_id"], json!(unfinished.id), "{doctor}");
    assert_eq!(
        health["worktree_path"],
        json!(text(expected(&unfinished.id).worktree))
    );
    assert_eq!(health["worktree_exists"], json!(true), "{doctor}");
    assert_eq!(
        health["run_dir"],
        json!(text(expected(&unfinished.id).run_dir))
    );
    assert_eq!(health["run_dir_exists"], json!(true), "{doctor}");

    // The awaiting run lands from the new location, and its worktree, whose
    // Git record is repaired on the way, is removed with its branch.
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(1).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    let kinds = event_kinds(&queue.show(1).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"worktree_removed".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"cleanup_failed".to_owned()), "{kinds:?}");
    let listing = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!listing.contains(&run.id), "{listing}");
}

/// A dependent's prompt names each predecessor with the commit `integrate`
/// landed and the summary its receipt carried, and lists the other tasks in
/// progress at claim time without the task itself.
#[test]
fn prompt_describes_landed_predecessors_and_sibling_tasks_in_progress() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = queue.show(1).unwrap().runs[0].clone();
    assert_eq!(landed.status, RunStatus::Integrated);
    let landed_commit = landed.result_commit.clone().unwrap();
    assert_eq!(landed_commit, git_out(&repo, &["rev-parse", "main"]));
    // The squash commit, not the run's validated head, is what the prompt names.
    assert_ne!(landed_commit, run.result_commit.unwrap());
    add_ready_task(&mut queue, "independent", &[]);

    // The queue's read-only view the prompt is built from.
    let predecessors = queue.predecessors(2).unwrap();
    assert_eq!(predecessors.len(), 1);
    assert_eq!(predecessors[0].task.id, 1);
    assert_eq!(predecessors[0].task.title, "test task");
    let integrated = predecessors[0].integrated_run.as_ref().unwrap();
    assert_eq!(integrated.id, run.id);
    assert_eq!(
        integrated.result_commit.as_deref(),
        Some(landed_commit.as_str())
    );
    assert!(queue.predecessors(3).unwrap().is_empty());
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // The dependent (task 2) is claimed before the independent task 3.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let dependent = queue.show(2).unwrap().runs[0].clone();
    assert_eq!(dependent.status, RunStatus::AwaitingIntegration);
    assert_eq!(dependent.base_commit, landed_commit);
    let prompt = read_prompt(&dependent);
    assert!(
        prompt.contains(&format!(
            "Predecessor tasks (their changes are already in your base commit):\n\
             - task 1: test task; result commit {landed_commit}; summary: done\n"
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("Predecessor tasks: none"), "{prompt}");
    // Nothing else was in progress when task 2 was claimed; it is not listed itself.
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 2: dependent"), "{prompt}");

    let independent = queue.show(3).unwrap().runs[0].clone();
    assert_eq!(independent.status, RunStatus::AwaitingIntegration);
    let prompt = read_prompt(&independent);
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: dependent\n"
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 3: independent"), "{prompt}");
    // A completed task is not in progress.
    assert!(!prompt.contains("- task 1: test task"), "{prompt}");
    // The sections sit between the verification commands and the receipt contract.
    let position = |needle: &str| prompt.find(needle).unwrap();
    assert!(position("Verification commands") < position("Predecessor tasks"));
    assert!(position("Predecessor tasks") < position("Sibling tasks in progress"));
    assert!(position("Sibling tasks in progress") < position("Write a completion receipt"));
}

/// A ready task with a goal and a context, registered on the queue.
fn add_ready_task_in(
    queue: &mut SqliteQueue,
    title: &str,
    goal_id: Option<i64>,
    context: &str,
) -> i64 {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            dependencies: vec![],
            goal_id,
            context: context.into(),
        })
        .unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
    task.id
}

/// The section headings of a prompt in order of appearance, so tests can
/// compare the shape of prompts with and without a goal.
fn section_order(prompt: &str) -> Vec<usize> {
    [
        "Task title:",
        "Verification commands",
        "Goal",
        "Context",
        "Predecessor tasks",
        "Sibling tasks in progress",
        "Your assignment is this task only.",
        "Write a completion receipt",
    ]
    .iter()
    .map(|heading| {
        prompt
            .find(heading)
            .unwrap_or_else(|| panic!("{heading}: {prompt}"))
    })
    .collect()
}

/// A task's prompt carries its goal as a Goal section (ID, title,
/// description, acceptance, constraints and the doc path, unread) and its
/// context as a Context section; a task without either says so in the same
/// place, so both prompts have the same sequence of sections.
#[test]
fn prompt_describes_the_goal_and_the_context_and_keeps_one_shape_without_them() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal title".into(),
            description: "goal description\nsecond line".into(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: Some("docs/plans/goal.md".into()),
        })
        .unwrap();
    fs::write(repo.join("unrelated.md"), "not read\n").unwrap();
    add_ready_task_in(
        &mut queue,
        "grouped",
        Some(goal.id),
        "why this task exists\nread docs/design/x.md first",
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let alone = read_prompt(&queue.show(1).unwrap().runs[0]);
    assert!(
        alone.contains("Goal: none, this task stands alone\n"),
        "{alone}"
    );
    assert!(alone.contains("Context: none\n"), "{alone}");
    assert!(!alone.contains("goal title"), "{alone}");

    let grouped = read_prompt(&queue.show(2).unwrap().runs[0]);
    assert!(
        grouped.contains(&format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {}\nGoal title: goal title\nGoal description:\ngoal description\nsecond line\n\
             Goal acceptance:\ngoal acceptance\nGoal constraints:\ngoal constraints\n\
             Goal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n",
            goal.id
        )),
        "{grouped}"
    );
    // The doc is named by path only; the prompt never embeds its content.
    assert!(!grouped.contains("not read"), "{grouped}");
    assert!(
        grouped.contains(
            "Context (why this task exists and what to read first):\n\
             why this task exists\nread docs/design/x.md first\n"
        ),
        "{grouped}"
    );
    assert!(!grouped.contains("Goal: none"), "{grouped}");
    assert!(!grouped.contains("Context: none"), "{grouped}");
    // Both prompts have the same sections in the same order.
    let order = section_order(&grouped);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{grouped}");
    let order = section_order(&alone);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{alone}");
    // The receipt example shows the optional follow_ups, and the scope rule names it.
    for prompt in [&alone, &grouped] {
        assert!(
            prompt.contains(
                "\"summary\":\"...\",\"follow_ups\":[{\"title\":\"...\",\"description\":\"...\"}]}\n"
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "Your assignment is this task only. Do not change what a sibling task owns; \
                 if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n"
            ),
            "{prompt}"
        );
        assert!(prompt.contains("follow_ups is optional"), "{prompt}");
    }
}

/// The Sibling section of a task with a goal lists only the in-progress
/// tasks of that goal; a task without a goal still sees every task in
/// progress. Claims in one pass go in ID order, so a prompt lists the
/// siblings claimed before it.
#[test]
fn prompt_lists_only_the_siblings_of_the_same_goal_in_progress() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Task 1 (no goal) is in progress before the grouped tasks are claimed.
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(1).unwrap().task.status, TaskStatus::InProgress);
    let a = queue
        .add_goal(NewGoal {
            title: "goal a".into(),
            ..NewGoal::default()
        })
        .unwrap();
    let b = queue
        .add_goal(NewGoal {
            title: "goal b".into(),
            ..NewGoal::default()
        })
        .unwrap();
    assert_eq!(add_ready_task_in(&mut queue, "a first", Some(a.id), ""), 2);
    assert_eq!(add_ready_task_in(&mut queue, "b only", Some(b.id), ""), 3);
    assert_eq!(add_ready_task_in(&mut queue, "a second", Some(a.id), ""), 4);
    assert_eq!(add_ready_task_in(&mut queue, "alone", None, ""), 5);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 4);

    let mut prompt_of = |task_id: i64| read_prompt(&queue.show(task_id).unwrap().runs[0]);
    // Task 2: task 1 is in progress but belongs to no goal, so it is not a sibling.
    let first = prompt_of(2);
    assert!(
        first.contains("Sibling tasks in progress: none\n"),
        "{first}"
    );
    assert!(!first.contains("- task 1: test task"), "{first}");
    // Task 3: nothing of goal b is in progress; goal a's task 2 is not listed.
    let only = prompt_of(3);
    assert!(only.contains("Sibling tasks in progress: none\n"), "{only}");
    assert!(!only.contains("- task 2: a first"), "{only}");
    // Task 4: its sibling task 2 is listed, task 3 of goal b and task 1 are not.
    let second = prompt_of(4);
    assert!(
        second.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: a first\n"
        ),
        "{second}"
    );
    assert!(!second.contains("- task 3: b only"), "{second}");
    assert!(!second.contains("- task 1: test task"), "{second}");
    assert!(!second.contains("- task 4: a second"), "{second}");
    // Task 5 has no goal and sees every task in progress except itself.
    let alone = prompt_of(5);
    assert!(
        alone.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 1: test task\n- task 2: a first\n- task 3: b only\n- task 4: a second\n"
        ),
        "{alone}"
    );
    assert!(!alone.contains("- task 5: alone"), "{alone}");
}

/// `prompt.txt` is a snapshot taken at claim time: a run claimed before a
/// goal edit keeps the old wording, and a run claimed after it gets the new.
#[test]
fn prompt_snapshots_the_goal_at_claim_time() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "before edit".into(),
            acceptance: "old acceptance".into(),
            ..NewGoal::default()
        })
        .unwrap();
    add_ready_task_in(&mut queue, "early", Some(goal.id), "");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let early = queue.show(2).unwrap().runs[0].clone();
    let before = read_prompt(&early);
    assert!(before.contains("Goal title: before edit\n"), "{before}");
    assert!(
        before.contains("Goal acceptance:\nold acceptance\n"),
        "{before}"
    );

    queue
        .edit_goal(
            goal.id,
            GoalEdit {
                title: Some("after edit".into()),
                acceptance: Some("new acceptance".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    add_ready_task_in(&mut queue, "late", Some(goal.id), "");
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let late = queue.show(3).unwrap().runs[0].clone();
    let after = read_prompt(&late);
    assert!(after.contains("Goal title: after edit\n"), "{after}");
    assert!(
        after.contains("Goal acceptance:\nnew acceptance\n"),
        "{after}"
    );
    assert!(!after.contains("before edit"), "{after}");
    // The earlier run's prompt was not rewritten by the edit or the later claim.
    assert_eq!(read_prompt(&early), before);
    // The late run also sees the early one as a sibling still in progress.
    assert!(after.contains("- task 2: early\n"), "{after}");
}

/// A predecessor whose receipt is gone from its run directory is still
/// named in the prompt; the successor's run starts and runs as usual.
#[test]
fn successor_starts_when_the_predecessor_receipt_is_unavailable() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let receipt = Path::new(run.receipt_path.as_ref().unwrap());
    assert_eq!(
        receipt,
        Path::new(run.run_dir.as_ref().unwrap()).join("receipt.json")
    );
    fs::remove_file(receipt).unwrap();

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(2).unwrap();
    let dependent = &detail.runs[0];
    assert_eq!(dependent.status, RunStatus::AwaitingIntegration);
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"agent_started"), "{kinds:?}");
    let landed_commit = queue.show(1).unwrap().runs[0]
        .result_commit
        .clone()
        .unwrap();
    let prompt = read_prompt(dependent);
    assert!(
        prompt.contains(&format!(
            "- task 1: test task; result commit {landed_commit}; summary: (receipt unavailable)\n"
        )),
        "{prompt}"
    );
    // A corrupt receipt is described the same way.
    let corrupt = queue.predecessors(2).unwrap();
    fs::write(receipt, "not json").unwrap();
    let summary = runtime::PredecessorSummary::from_predecessor(&corrupt[0]);
    assert_eq!(summary.summary, "(receipt unavailable)");
    assert_eq!(summary.result_commit, landed_commit);
    assert_eq!((summary.task_id, summary.title.as_str()), (1, "test task"));
    // A predecessor completed without an integrated run has neither.
    let by_hand = dagq::domain::Predecessor {
        task: corrupt[0].task.clone(),
        integrated_run: None,
    };
    let summary = runtime::PredecessorSummary::from_predecessor(&by_hand);
    assert_eq!(summary.result_commit, "(not landed)");
    assert_eq!(summary.summary, "(receipt unavailable)");
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
        &["rev-parse", &format!("refs/dagq/runs/{}", second.id)],
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

/// The landing reruns the verification commands only when the rebase moved
/// the head. The first run lands on an unmoved main, so its rebase is a
/// no-op and validation's run of the same commands on the same commit
/// stands; the second is rebased onto that landing, so its tree is new and
/// the commands run again.
#[test]
fn reverification_is_skipped_for_a_no_op_rebase_and_runs_when_the_head_moves() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // `ls` always passes, and its output names the tree it ran on.
    let first = add_file_task(&mut queue, &backend, "first", "first.txt", "1", &["ls"]);
    let second = add_file_task(&mut queue, &backend, "second", "second.txt", "2", &["ls"]);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    // Landing order is FIFO by validation time; take whichever validated first.
    let validated_first = queue.next_awaiting_integration().unwrap().unwrap();
    // `landed_file` is the file the first landing puts on main, so it exists
    // in the second run's tree only after the rebase.
    let (first, second, landed_file) = if validated_first.task_id == first {
        (first, second, "first.txt")
    } else {
        (second, first, "second.txt")
    };
    let head_before = queue.show(first).unwrap().runs[0]
        .result_commit
        .clone()
        .unwrap();

    // (a) Main has not moved: the rebase is a no-op and nothing is rerun.
    let outcome = integrate(&db, first, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["verification_skipped"], json!(true), "{outcome}");
    let detail = queue.show(first).unwrap();
    let run = detail.runs[0].clone();
    assert!(
        integration_verifications(&detail).is_empty(),
        "{:?}",
        event_kinds(&detail)
    );
    assert!(
        !Path::new(run.run_dir.as_ref().unwrap())
            .join("integrate-verify-1.log")
            .exists()
    );
    let skipped: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == "integration_verification_skipped")
        .collect();
    assert_eq!(skipped.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(skipped[0].payload["main"], json!(seed));
    assert_eq!(skipped[0].payload["head"], json!(head_before));
    assert_eq!(
        skipped[0].payload["reason"],
        "rebase was a no-op; validation already verified this head"
    );

    // (b) The second run is rebased onto that landing: a new tree, so the
    // verification commands run again and leave their log and event.
    let landed = git_out(&repo, &["rev-parse", "main"]);
    let outcome = integrate(&db, second, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    let detail = queue.show(second).unwrap();
    let run = detail.runs[0].clone();
    let rebased = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["main"], json!(landed));
    assert_ne!(
        rebased.payload["head_before"],
        rebased.payload["head_after"]
    );
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verifications[0]["command"], "ls");
    assert_eq!(verifications[0]["exit_code"], 0);
    assert!(
        verifications[0]["log_path"]
            .as_str()
            .unwrap()
            .ends_with("integrate-verify-1.log")
    );
    // The rerun ran on the rebased tree: it sees the file the first landing
    // added, which validation's own run of `ls` could not have seen.
    let integration_log =
        fs::read_to_string(Path::new(run.run_dir.as_ref().unwrap()).join("integrate-verify-1.log"))
            .unwrap();
    assert!(integration_log.contains(landed_file), "{integration_log}");
    let validation_log =
        fs::read_to_string(Path::new(run.run_dir.as_ref().unwrap()).join("verify-1.log")).unwrap();
    assert!(!validation_log.contains(landed_file), "{validation_log}");
    assert!(!event_kinds(&detail).contains(&"integration_verification_skipped"));
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

    // Every receipt that passed its checks was recorded, even when the
    // landing then stopped: the stale one names the validated head.
    let detail = queue.show(2).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    assert!(recorded.iter().all(|p| p["commit"] == json!(source)));
    assert!(recorded.iter().all(|p| p["main"] == json!(first_landed)));

    let mut rewritten = session_receipt(&parked, &resolved, "succeeded", "resolved");
    rewritten["tests"]["evidence_or_reason"] = json!("cargo test after the rebase: 12 passed");
    rewritten["follow_ups"] = json!([
        {"title": "dedupe change.txt", "description": "both tasks wrote it"}
    ]);
    write_receipt_json(&parked, rewritten.clone());
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    // The session rebased the branch itself, so this rebase is a no-op -- but
    // the head is the session's, not the one validation verified, so the
    // verification commands still run here.
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    let landed = queue.show(2).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    // The receipt the session rewrote is what the DB keeps for the landing,
    // while validation_finished still holds the one from before the conflict.
    let detail = queue.show(2).unwrap();
    assert!(!event_kinds(&detail).contains(&"integration_verification_skipped"));
    let rebased = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["head_before"], json!(resolved));
    assert_eq!(rebased.payload["head_after"], json!(resolved));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verifications[0]["exit_code"], 0);
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    let last = recorded[3];
    assert_eq!(last["commit"], json!(resolved));
    assert_eq!(last["main"], json!(first_landed));
    assert_eq!(last["receipt"], rewritten);
    assert_eq!(last["receipt"]["commit"], json!(resolved));
    assert_eq!(
        last["receipt"]["tests"]["evidence_or_reason"],
        json!("cargo test after the rebase: 12 passed")
    );
    assert_eq!(
        last["receipt"]["follow_ups"],
        json!([{"title": "dedupe change.txt", "description": "both tasks wrote it"}])
    );
    let validated = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(validated.payload["receipt"]["commit"], json!(source));
    assert!(validated.payload["receipt"].get("follow_ups").is_none());
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("refs/dagq/runs/{}", run.id)]),
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
        git_out(&repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count()
            == 1
    );
    assert_eq!(queue.show(2).unwrap().task.status, TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    let detail = queue.show(2).unwrap();
    let failed_event = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_failed")
        .unwrap();
    assert_eq!(failed_event.payload["status"], "failed");
    assert_eq!(failed_event.payload["reason"], json!(reason));
    assert_eq!(
        failed_event.payload["receipt"],
        session_receipt(
            &run,
            run.result_commit.as_deref().unwrap(),
            "failed",
            "already covered by task 1"
        )
    );
    // A failed receipt is not a receipt for a landing: only the first
    // (conflicting) attempt recorded one.
    assert_eq!(integration_receipts(&detail).len(), 1);
    // Retry or give up is the maintainer's call, as after any failed run.
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
            goal_id: None,
            context: String::new(),
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
    // The receipt was read and recorded before the verification failed.
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(
        recorded[0]["commit"],
        json!(run.result_commit.clone().unwrap())
    );
    assert_eq!(recorded[0]["main"], json!(main));
    assert_eq!(recorded[0]["receipt"]["run_id"], json!(run.id));
    assert_eq!(recorded[0]["receipt"]["result"], "succeeded");
    assert_eq!(
        recorded[0]["receipt"]["commit"],
        json!(run.result_commit.clone().unwrap())
    );
    let kinds = event_kinds(&detail);
    let receipt_at = kinds
        .iter()
        .position(|k| *k == "integration_receipt")
        .unwrap();
    let deferred_at = kinds
        .iter()
        .rposition(|k| *k == "integration_deferred")
        .unwrap();
    assert!(receipt_at < deferred_at, "{kinds:?}");
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
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(
        recorded[1]["commit"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id)]
        ))
    );
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

/// Provision and start a run under `token` exactly as `supervise` does
/// (claim, plan, prompt, worktree, workspace with the wrapper thread of the
/// test backend), but with no loop or heartbeat behind the token: the state
/// a supervisor leaves when it is killed after the session started. Returns
/// the `running` run once the wrapper registered its agent.
fn start_run_under_dead_supervisor(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> TaskRun {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            location::runs_dir,
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&repository.main_head().unwrap(), token)
        .unwrap()
    else {
        panic!("no candidate to claim")
    };
    let run_dir = runs_dir(&db.canonicalize().unwrap()).join(&run.id);
    queue
        .plan_run(
            &run.id,
            token,
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
            },
        )
        .unwrap();
    fs::create_dir_all(&run_dir).unwrap();
    let run = queue.run(&run.id).unwrap();
    let task = queue.show(run.task_id).unwrap().task;
    fs::write(
        run_dir.join("prompt.txt"),
        runtime::prompt(&task, &run, None, &[], &[]).unwrap(),
    )
    .unwrap();
    repository.create_worktree(&run).unwrap();
    let command = shell_join(&[
        "runner".into(),
        "--db".into(),
        path_text(db).unwrap(),
        "session".into(),
    ]);
    let workspace = backend.create(&run, &command).unwrap();
    queue.workspace_created(&run.id, token, &workspace).unwrap();
    wait_until(db, Duration::from_secs(10), |queue| {
        queue.run(&run.id).unwrap().status == RunStatus::Running
    });
    queue.run(&run.id).unwrap()
}

/// Age the lease of `run` so it is stale by heartbeat while its pid (this
/// test process) is alive, like a supervisor that stopped heartbeating.
fn age_lease(db: &Path, run: &TaskRun, seconds: i64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch()-?2 WHERE run_id=?1",
            rusqlite::params![run.id, seconds],
        )
        .unwrap();
}

fn adoption_events(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "run_adopted")
        .map(|e| &e.payload)
        .collect()
}

fn supervisor_token_of(db: &Path, run: &TaskRun) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap()
}

/// A `running` run whose supervisor stopped heartbeating (lease 31 s old)
/// while its wrapper keeps heartbeating is adopted by the next supervisor
/// with a free slot: the lease and `supervisor_token` move to the adopter,
/// `run_adopted` records what was taken over, and the adopter drives the
/// run through receipt, idle, one exit request, validation and close.
#[test]
fn stale_lease_of_a_live_wrapper_is_adopted_and_driven_to_awaiting_integration() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Nothing else is claimable: the pass exists only for the adoption.
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(
        runtime::status(&db).unwrap()["runs"][0]["lease"]["stale"],
        true
    );

    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["id"], json!(run.id));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);

    let detail = queue.show(1).unwrap();
    let adopted_run = &detail.runs[0];
    assert_eq!(adopted_run.status, RunStatus::AwaitingIntegration);
    assert!(adopted_run.last_error.is_none());
    assert!(adopted_run.result_commit.is_some());
    assert!(adopted_run.workspace_closed_at.is_some());
    assert!(queue.run_leases().unwrap().is_empty());
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    let payload = adopted[0];
    assert_eq!(payload["previous_token"], "dead-supervisor");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    let age = payload["previous_heartbeat_age_secs"].as_i64().unwrap();
    assert!(age >= 31, "{payload}");
    assert_eq!(payload["wrapper"]["pid"], json!(std::process::id()));
    assert_eq!(payload["wrapper"]["alive"], true);
    assert_eq!(payload["wrapper"]["exited_at"], Value::Null);
    assert_eq!(payload["pid"], json!(std::process::id()));
    // The adopter's token replaced the claimer's on the run.
    let adopter = payload["token"].as_str().unwrap();
    assert_ne!(adopter, "dead-supervisor");
    assert_eq!(supervisor_token_of(&db, &run), adopter);
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("lease_acquired") < position("run_adopted"));
    assert!(position("run_adopted") < position("receipt_observed"));
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(position("session_exited") < position("supervision_finished"));
    assert!(position("supervision_finished") < position("validation_finished"));
    assert!(position("validation_finished") < position("workspace_closed"));
    assert!(position("workspace_closed") < position("lease_released"));
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"runtime_error"));
    assert!(!kinds.contains(&"run_recovered"));
    // The adoption is a fact of this run's history, not a second run.
    assert_eq!(detail.runs.len(), 1);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "run_adopted")
        .unwrap();
    assert_eq!(event.run_id.as_deref(), Some(run.id.as_str()));
}

/// The incident of task 15: the supervisor was killed a moment ago, so its
/// lease pid is dead while its heartbeat is still fresh. The pid rule alone
/// makes the lease stale, and the run is adopted without waiting for the TTL.
#[test]
fn dead_supervisor_pid_with_a_fresh_heartbeat_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "killed");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch() WHERE run_id=?1",
            rusqlite::params![run.id, dead_pid()],
        )
        .unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "killed");
    assert_ne!(adopted[0]["previous_pid"], json!(std::process::id()));
    assert!(adopted[0]["previous_heartbeat_age_secs"].as_i64().unwrap() < 30);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// Everything adoption must leave alone: a fresh lease; a stale lease whose
/// wrapper is dead or silent (that is `recover`'s case, and `doctor` still
/// says so); `claimed` / `starting` runs; runs without a lease row; and an
/// `integrating` run. A supervisor pass over them adopts nothing and writes
/// no `run_adopted` event.
#[test]
fn fresh_leases_dead_wrappers_early_runs_leaseless_and_integrating_runs_are_not_adopted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    for title in [
        "fresh",
        "dead wrapper",
        "silent wrapper",
        "starting",
        "leaseless",
        "claimed",
    ] {
        add_ready_task(&mut queue, title, &[]);
    }
    let raw = Connection::open(&db).unwrap();
    // Fresh lease, live wrapper (children that heartbeat nothing but are alive; the
    // lease is what decides here).
    let mut children: Vec<std::process::Child> = Vec::new();
    let mut spawn = || {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        children.push(child);
        pid
    };
    let fresh = orphan_run(&repo, &db, "fresh-owner", spawn(), spawn());
    // Stale lease, wrapper dead: recoverable, never adopted.
    let dead_wrapper = orphan_run(&repo, &db, "gone", dead_pid(), dead_pid());
    // Stale lease, wrapper alive but silent for longer than the TTL.
    let silent_wrapper = orphan_run(&repo, &db, "gone", spawn(), spawn());
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=unixepoch()-31, pid=?1 WHERE token='gone'",
        [dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=unixepoch()-31 WHERE run_id IN (?1, ?2)",
        [&dead_wrapper.id, &silent_wrapper.id],
    )
    .unwrap();
    // `starting` with a stale lease: register_wrapper needs the claimer's token.
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let base = queue.run(&fresh.id).unwrap().base_commit;
    let ClaimOutcome::Claimed { run: starting } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            &starting.id,
            "gone-early",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/starting".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    let starting = queue.run(&starting.id).unwrap();
    assert_eq!(starting.status, RunStatus::Starting);
    // `running` without a lease: abandoned by a runtime error or recovered.
    let leaseless = orphan_run(&repo, &db, "abandoned", spawn(), spawn());
    queue
        .abandon_run(&leaseless.id, "abandoned", "exit request timed out")
        .unwrap();
    assert!(queue.run_lease(&leaseless.id).unwrap().is_none());
    // `claimed` with a stale lease.
    let ClaimOutcome::Claimed { run: claimed } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=0, pid=?1 WHERE token='gone-early'",
        [dead_pid()],
    )
    .unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let before = runtime::doctor(&db).unwrap();
    let health = |report: &Value, run: &TaskRun| -> Value {
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == json!(run.id))
            .cloned()
            .unwrap()
    };
    assert_eq!(health(&before, &dead_wrapper)["recoverable"], true);
    assert_eq!(health(&before, &silent_wrapper)["recoverable"], false);
    assert_eq!(health(&before, &fresh)["recoverable"], false);
    assert_eq!(health(&before, &starting)["recoverable"], true);
    assert_eq!(health(&before, &claimed)["recoverable"], true);

    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    for run in [
        &fresh,
        &dead_wrapper,
        &silent_wrapper,
        &starting,
        &leaseless,
        &claimed,
    ] {
        let detail = queue.show(run.task_id).unwrap();
        assert!(
            adoption_events(&detail).is_empty(),
            "run {} of task {} was adopted",
            run.id,
            run.task_id
        );
        assert_eq!(queue.run(&run.id).unwrap().status, run.status);
    }
    // Leases, tokens and doctor's verdicts are exactly as before the pass.
    assert_eq!(
        queue.run_lease(&fresh.id).unwrap().unwrap().token,
        "fresh-owner"
    );
    assert_eq!(
        queue.run_lease(&dead_wrapper.id).unwrap().unwrap().token,
        "gone"
    );
    assert_eq!(
        queue.run_lease(&silent_wrapper.id).unwrap().unwrap().token,
        "gone"
    );
    assert_eq!(
        queue.run_lease(&starting.id).unwrap().unwrap().token,
        "gone-early"
    );
    assert_eq!(
        queue.run_lease(&claimed.id).unwrap().unwrap().token,
        "gone-early"
    );
    assert!(queue.run_lease(&leaseless.id).unwrap().is_none());
    let after = runtime::doctor(&db).unwrap();
    for run in [&fresh, &dead_wrapper, &silent_wrapper, &starting, &claimed] {
        assert_eq!(
            health(&after, run)["recoverable"],
            health(&before, run)["recoverable"]
        );
        assert_eq!(
            health(&after, run)["lease"]["stale"],
            health(&before, run)["lease"]["stale"]
        );
    }
    assert_eq!(health(&after, &dead_wrapper)["recoverable"], true);
    assert_eq!(
        runtime::recover(&db, &dead_wrapper.id).unwrap()["run"]["status"],
        "interrupted"
    );
    for child in &mut children {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

/// An `integrating` run is never adopted, however stale its lease: a
/// crashed landing is `recover`'s case and goes back to the merge queue.
#[test]
fn integrating_run_with_a_stale_lease_is_not_adopted() {
    let (_dir, repo, db, run) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue.begin_integration(&run.id, "crashed", &main).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert!(queue.candidates().unwrap().is_empty()); // The dependent still waits.
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Integrating);
    assert_eq!(queue.run_lease(&run.id).unwrap().unwrap().token, "crashed");
    assert!(adoption_events(&queue.show(1).unwrap()).is_empty());
    assert_eq!(
        runtime::recover(&db, &run.id).unwrap()["run"]["status"],
        "awaiting_integration"
    );
}

/// The previous supervisor already asked the session to exit: the adopter
/// rebuilds that from the `exit_requested` event and does not send `/exit`
/// again, and its receipt observation is not repeated either.
#[test]
fn adopter_does_not_repeat_an_exit_request_the_previous_supervisor_sent() {
    let (_dir, repo, db) = fixture();
    // The session ends on its own a few seconds after going idle, as it
    // would after the /exit that was already typed.
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 4",
    );
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let receipt = PathBuf::from(run.receipt_path.as_ref().unwrap());
    wait_until(&db, Duration::from_secs(10), |_| receipt.is_file());
    // What the previous supervisor recorded before it died.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            &run.id,
            "receipt_observed",
            json!({"path": run.receipt_path, "validated": false}),
        )
        .unwrap();
    queue
        .record_runtime_event(&run.id, "session_idle_observed", json!({}))
        .unwrap();
    queue
        .record_runtime_event(
            &run.id,
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(1).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(
        kinds.iter().filter(|k| **k == "receipt_observed").count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session_idle_observed")
            .count(),
        1
    );
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// The exit timeout of an adopted run restarts at adoption: a session that
/// still ignores the earlier request is given up after the adopter's own
/// timeout, with one `exit_requested` event in total.
#[test]
fn adopted_exit_request_times_out_from_the_adoption() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(2);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            &run.id,
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let error = outcome["errors"][0]["message"].as_str().unwrap();
    assert!(error.contains("did not exit within 2s"), "{error}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(1).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(kinds.contains(&"exit_request_timed_out"));
    assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Running);
    assert!(queue.run_leases().unwrap().is_empty());
    // Let the fake session out; nothing adopts a lease-less run afterwards.
    fs::write(exit_request_path(run.run_dir.as_ref().unwrap()), "").unwrap();
    backend.join();
    let again = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(again["runs"], json!([]));
    assert_eq!(adoption_events(&queue.show(1).unwrap()).len(), 1);
}

/// The supervisor died after the wrapper reported its exit but before
/// `supervision_finished`, and, separately, while a run was `validating`.
/// Both are adopted: the first finishes supervision from the recorded exit,
/// the second restarts validation from the receipt and worktree.
#[test]
fn exited_wrapper_and_validating_runs_are_adopted_and_validated() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "validating", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // The first session goes idle after its receipt and then ends on its own
    // (a maintainer's /exit by the old procedure): the adopter must not
    // send /exit to a session that already exited.
    backend.script_for(
        1,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 1",
    );
    let exited = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-a");
    let validating = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-b");
    backend.join(); // Both sessions end by themselves.
    for run in [&exited, &validating] {
        assert!(event_kinds(&queue.show(run.task_id).unwrap()).contains(&"session_exited"));
        assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Running);
    }
    queue.finish_supervision(&validating.id, "dead-b").unwrap();
    assert_eq!(
        queue.run(&validating.id).unwrap().status,
        RunStatus::Validating
    );
    age_lease(&db, &exited, 31);
    age_lease(&db, &validating, 31);

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1)]);
    for run in [&exited, &validating] {
        let detail = queue.show(run.task_id).unwrap();
        let after = &detail.runs[0];
        assert_eq!(after.status, RunStatus::AwaitingIntegration, "{}", run.id);
        assert!(after.last_error.is_none(), "{after:?}");
        assert!(!event_kinds(&detail).contains(&"exit_requested"));
        assert!(after.result_commit.is_some());
        assert!(after.workspace_closed_at.is_some());
        let adopted = adoption_events(&detail);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0]["wrapper"]["alive"], Value::Null);
        assert!(adopted[0]["wrapper"]["exited_at"].is_number());
        let kinds = event_kinds(&detail);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "supervision_finished")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "validation_finished")
                .count(),
            1
        );
        assert!(kinds.contains(&"verification_command"));
    }
    let detail = queue.show(exited.task_id).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("session_exited") < position("run_adopted"));
    assert!(position("run_adopted") < position("supervision_finished"));
    let detail = queue.show(validating.task_id).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("supervision_finished") < position("run_adopted"));
    assert!(position("run_adopted") < position("validation_finished"));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Two supervisors with free slots find the same stale lease at once: the
/// transaction lets exactly one of them adopt, the other sees no lease
/// under the old token and moves on. One `run_adopted` event, one owner.
#[test]
fn two_supervisors_racing_for_one_stale_lease_adopt_it_once() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || supervise(&db, &repo, &backend))
        })
        .collect();
    let outcomes: Vec<Value> = racers
        .into_iter()
        .map(|racer| racer.join().unwrap().unwrap())
        .collect();
    backend.join();
    let driven: Vec<&Value> = outcomes
        .iter()
        .filter(|o| !o["runs"].as_array().unwrap().is_empty())
        .collect();
    assert_eq!(driven.len(), 1, "{outcomes:?}");
    assert_eq!(driven[0]["runs"][0]["status"], "awaiting_integration");
    assert!(outcomes.iter().all(|o| o["errors"] == json!([])));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(supervisor_token_of(&db, &run), adopted[0]["token"]);
    assert_eq!(detail.runs[0].status, RunStatus::AwaitingIntegration);

    // The queue method itself: a second adoption under the old token, or
    // one against a fresh lease, takes nothing.
    add_ready_task(&mut queue, "second", &[]);
    add_ready_task(&mut queue, "early", &[]);
    let second = orphan_run(&repo, &db, "fresh", std::process::id(), std::process::id());
    assert!(
        queue
            .adopt_run(&second.id, "fresh", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.run_lease(&second.id).unwrap().unwrap().token, "fresh");
    age_lease(&db, &second, 31);
    let taken = queue
        .adopt_run(&second.id, "fresh", "first", 1, json!({"pid": 1}))
        .unwrap()
        .unwrap();
    assert_eq!(taken.status, RunStatus::Running);
    assert!(
        queue
            .adopt_run(&second.id, "fresh", "second", 2, json!({}))
            .unwrap()
            .is_none()
    );
    let lease = queue.run_lease(&second.id).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("first", 1));
    assert!(runtime::unix_time() - lease.heartbeat_at <= 5);
    assert_eq!(supervisor_token_of(&db, &second), "first");
    assert!(queue.holds_lease(&second.id, "first").unwrap());
    assert!(!queue.holds_lease(&second.id, "fresh").unwrap());
    assert!(queue.has_run_event(&second.id, "run_adopted").unwrap());
    assert!(!queue.has_run_event(&second.id, "receipt_observed").unwrap());
    let payload = &adoption_events(&queue.show(second.task_id).unwrap())[0].clone();
    assert_eq!(payload["wrapper"], json!({"pid": 1}));
    assert_eq!(payload["previous_token"], "fresh");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    // A `starting` run is refused by the method too, stale or not.
    use dagq::domain::ClaimOutcome;
    let ClaimOutcome::Claimed { run: early } = queue
        .claim_for_supervisor(&second.base_commit, "early")
        .unwrap()
    else {
        panic!()
    };
    age_lease(&db, &early, 31);
    assert!(
        queue
            .adopt_run(&early.id, "early", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    // The status/doctor lists the adopted run under the adopter's token like any other.
    let status = runtime::status(&db).unwrap();
    let holders: Vec<&Value> = status["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["run_ids"].as_array().unwrap().contains(&json!(second.id)))
        .collect();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0]["pid"], 1);
}

/// A resident supervisor whose lease was taken over (here by hand, as an
/// adopter or `recover` would) drops the run from its slots without writing
/// anything more about it, while the adopter drives the run to the end.
#[test]
fn a_supervisor_that_lost_its_lease_stops_touching_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = SuperviseOptions::new(2, false);
    let original = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(20), |queue| {
        queue
            .active_runs()
            .unwrap()
            .first()
            .is_some_and(|r| r.status == RunStatus::Running)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.active_runs().unwrap().remove(0);
    // Draining: the original keeps driving its run but adopts and claims
    // nothing more, so it cannot take the lease back once it loses it.
    options.stop.store(true, Ordering::SeqCst);
    // The lease changes hands: another token, stale, as a killed
    // supervisor's would look to an adopter.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET token='taken', heartbeat_at=unixepoch()-31 WHERE run_id=?1",
            [&run.id],
        )
        .unwrap();
    // The original notices within a tick, drops the run and, draining with
    // nothing active, exits.
    let outcome = original.join().unwrap().unwrap();
    assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Running);
    assert_eq!(queue.run_lease(&run.id).unwrap().unwrap().token, "taken");
    let adopter = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        adopter["runs"][0]["status"], "awaiting_integration",
        "{adopter}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id));
    assert!(
        outcome["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("held by another process"),
        "{outcome}"
    );
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.runs[0].status, RunStatus::AwaitingIntegration);
    assert!(detail.runs[0].last_error.is_none());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "taken");
    assert!(queue.supervisors().unwrap().is_empty());
}
