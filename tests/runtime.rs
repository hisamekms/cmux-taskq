use anyhow::{Result, bail};
use cmux_taskq::{
    application::{AgentProvider, TaskQueue, WorkspaceBackend},
    domain::{NewTask, RunStatus, TaskAction, TaskRun, TaskStatus},
    infrastructure::{
        adapters::{shell_join, workspace_handle},
        sqlite::SqliteQueue,
    },
    runtime,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    thread,
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
    let task = queue
        .add(NewTask {
            title: "test task".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            dependencies: vec![],
        })
        .unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
    (dir, repo, db)
}

/// Shell prelude for the fake agent: `receipt COMMIT [RUN_ID]` writes an
/// atomically renamed receipt claiming success with evidence on every check.
const AGENT_PRELUDE: &str = r#"
test -f seed.txt || exit 99
printf 'fixture log\n' > "$LOG"
receipt() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "${2:-$RUN_ID}" "$1" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
commit() { printf 'change\n' > change.txt && git add change.txt && git commit -q -m "$1"; }
sleep 2
"#;
const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";

struct TestProvider {
    script: String,
}
impl AgentProvider for TestProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, run: &TaskRun, prompt: &str) -> Result<Command> {
        assert!(prompt.contains("Acceptance criteria:"));
        assert!(prompt.contains("test -f seed.txt"));
        let mut command = Command::new("/bin/sh");
        command
            .current_dir(run.worktree_path.as_ref().unwrap())
            .env("RUN_ID", &run.id)
            .env("RECEIPT", run.receipt_path.as_ref().unwrap())
            .env("LOG", run.log_path.as_ref().unwrap())
            .env("BASE", &run.base_commit)
            .arg("-c")
            .arg(format!("{AGENT_PRELUDE}\n{}", self.script));
        Ok(command)
    }
}

const WORKSPACE_ID: &str = "01234567-89ab-4def-8123-456789abcdef";

struct TestWorkspace {
    db: PathBuf,
    fail: bool,
    close_fail: bool,
    script: String,
    worker: Mutex<Option<thread::JoinHandle<Result<Value>>>>,
    closed: Mutex<Vec<String>>,
}
impl TestWorkspace {
    fn new(db: &Path, fail: bool, script: &str) -> Self {
        Self {
            db: db.into(),
            fail,
            close_fail: false,
            script: script.into(),
            worker: Mutex::new(None),
            closed: Mutex::new(Vec::new()),
        }
    }
    fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
    fn join(&self) {
        self.worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
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
            "SELECT token FROM supervisor_leases",
            [],
            |r| r.get(0),
        )?;
        let db = self.db.clone();
        let id = run.id.clone();
        let script = self.script.clone();
        *self.worker.lock().unwrap() = Some(thread::spawn(move || {
            runtime::session_with_provider(&db, &id, &token, &TestProvider { script })
        }));
        Ok(WORKSPACE_ID.into())
    }
    fn capture(&self, _: &str) -> Result<String> {
        Ok("fixture terminal screen".into())
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        // The session must have exited before the supervisor gives up the workspace.
        let exited: bool = Connection::open(&self.db)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_processes WHERE role='wrapper' AND exited_at IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        assert!(exited);
        if self.close_fail {
            bail!("injected workspace close failure");
        }
        self.closed.lock().unwrap().push(workspace_id.into());
        Ok(())
    }
}

fn supervise(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    // /bin/sh --version is not portable; a tiny standalone provider preflight stub.
    let stub = db.parent().unwrap().join("claude-stub");
    fs::write(&stub, "#!/bin/sh\nprintf 'test provider\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    runtime::supervise(
        db,
        repo,
        backend,
        &stub,
        Path::new(env!("CARGO_BIN_EXE_cmux-taskq")),
    )
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
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    let run = &detail.runs[0];
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
    assert!(queue.supervisor_lease().unwrap().is_none());
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
    assert_eq!(outcome["run"]["status"], "failed");
    // A nonzero session exit is final; the receipt is not validated and the
    // workspace stays open for inspection.
    assert_eq!(outcome["run"]["result_commit"], Value::Null);
    assert_eq!(outcome["run"]["workspace_closed_at"], Value::Null);
    assert!(backend.closed().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert!(Path::new(detail.runs[0].worktree_path.as_ref().unwrap()).exists());
    assert!(queue.supervisor_lease().unwrap().is_none());
    assert!(queue.candidates().unwrap().is_empty());
    // A failed run does not free the task automatically, but the operator may give up on it.
    assert_eq!(runtime::doctor(&db).unwrap()["runs"], json!([]));
    queue.transition(1, TaskAction::Cancel).unwrap();
    assert_eq!(queue.show(1).unwrap().task.status, TaskStatus::Canceled);
}

#[test]
fn ambiguous_provisioning_failure_keeps_lease_and_planned_paths() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert!(supervise(&db, &repo, &backend).is_err());
    let mut queue = SqliteQueue::open(&db).unwrap();
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
    assert!(queue.supervisor_lease().unwrap().is_some());
    assert!(supervise(&db, &repo, &backend).is_err());
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
}

#[test]
fn stale_lease_is_not_stolen_and_queue_cannot_switch_repositories() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.acquire_supervisor("first", "/repo/one/.git").unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE supervisor_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(
        queue
            .acquire_supervisor("second", "/repo/one/.git")
            .is_err()
    );
    assert!(
        queue
            .acquire_supervisor("second", "/repo/two/.git")
            .is_err()
    );
    assert!(queue.release_supervisor("wrong-owner").is_err());
    assert_eq!(queue.supervisor_lease().unwrap().unwrap().heartbeat_at, 0);
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
    assert_eq!(queue.schema_version().unwrap(), 3);
    assert_eq!(queue.show(1).unwrap().task.title, "preserved");
    assert!(queue.supervisor_lease().unwrap().is_none());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_stale_owners() {
    use cmux_taskq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.acquire_supervisor("owner", "/test/.git").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim("0123456789abcdef0123456789abcdef01234567")
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
    raw.execute("UPDATE supervisor_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(queue.register_wrapper(&run.id, "owner", 10).is_err());
    queue.heartbeat_supervisor("owner").unwrap();
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
    queue.acquire_supervisor("owner", "/test/.git").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim("0123456789abcdef0123456789abcdef01234567")
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
fn no_ready_task_releases_lease_without_creating_a_run() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "no_ready_task"
    );
    assert!(queue.supervisor_lease().unwrap().is_none());
    assert!(queue.show(1).unwrap().runs.is_empty());
}

/// A PID that certainly belonged to a process that has already exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Register a run the way `supervise` does, with the given PIDs as wrapper and
/// agent, but with no supervisor loop watching it. Returns the running run.
fn orphan_run(repo: &Path, db: &Path, wrapper: u32, agent: u32) -> TaskRun {
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
        .acquire_supervisor("owner", &path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&repository.base_commit).unwrap() else {
        panic!()
    };
    let run_dir = db.parent().unwrap().join("orphan");
    fs::create_dir(&run_dir).unwrap();
    queue
        .plan_run(
            &run.id,
            "owner",
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
    queue.workspace_created(&run.id, "owner", "ws-1").unwrap();
    queue.register_wrapper(&run.id, "owner", wrapper).unwrap();
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
    let run = orphan_run(&repo, &db, wrapper.id(), agent.id());
    let mut queue = SqliteQueue::open(&db).unwrap();

    // Everything is alive: doctor says so and recover refuses.
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisor"]["stale"], false);
    assert_eq!(report["supervisor"]["alive"], true);
    let health = &report["runs"][0];
    assert_eq!(health["run_id"], json!(run.id));
    assert_eq!(health["status"], "running");
    assert_eq!(health["workspace_id"], "ws-1");
    assert_eq!(health["worktree_exists"], true);
    assert_eq!(health["run_dir_exists"], true);
    assert_eq!(health["receipt_exists"], false);
    assert_eq!(health["recoverable"], false);
    let processes = health["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["alive"] == true));
    let error = format!("{:#}", runtime::recover(&db, &run.id).unwrap_err());
    assert!(
        error.contains("wrapper pid") && error.contains("supervisor heartbeat"),
        "{error}"
    );
    assert!(queue.transition(1, TaskAction::Ready).is_err());

    // The supervisor is gone (stale heartbeat, dead PID) but the session is not.
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE supervisor_leases SET heartbeat_at=0, pid=?1",
        [dead_pid()],
    )
    .unwrap();
    raw.execute("UPDATE run_processes SET heartbeat_at=0", [])
        .unwrap();
    let report = runtime::doctor(&db).unwrap();
    assert_eq!(report["supervisor"]["stale"], true);
    assert_eq!(report["supervisor"]["alive"], false);
    assert_eq!(report["runs"][0]["processes"][0]["heartbeat_stale"], true);
    let error = format!("{:#}", runtime::recover(&db, &run.id).unwrap_err());
    assert!(
        error.contains("agent pid") && !error.contains("supervisor"),
        "{error}"
    );
    assert_eq!(queue.run(&run.id).unwrap().status, RunStatus::Running);
    assert!(queue.supervisor_lease().unwrap().is_some());

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
    assert!(queue.supervisor_lease().unwrap().is_none());
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["previous_status"], "running");
    assert_eq!(recovered.payload["lease_deleted"], true);
    assert_eq!(recovered.payload["run"]["processes"][0]["alive"], false);
    assert_eq!(recovered.payload["supervisor"]["stale"], true);
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
    assert_eq!(outcome["run"]["status"], "awaiting_integration");
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
    let run = orphan_run(&repo, &db, pid, pid);
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
        .execute("DELETE FROM supervisor_leases", [])
        .unwrap();
    assert_eq!(runtime::doctor(&db).unwrap()["supervisor"], Value::Null);
    let outcome = runtime::recover(&db, &run.id).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(1).unwrap();
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["lease_deleted"], false);
    assert_eq!(recovered.payload["supervisor"], Value::Null);
    // The task can be edited again before a retry.
    assert!(queue.add_dependency(1, 1).is_err());
    queue.transition(1, TaskAction::Draft).unwrap();
    assert_eq!(queue.show(1).unwrap().task.status, TaskStatus::Draft);
    assert!(runtime::recover(&db, "no-such-run").is_err());
}
