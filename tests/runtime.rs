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
use serde_json::Value;
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

struct TestProvider {
    exit_code: i32,
}
impl AgentProvider for TestProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, run: &TaskRun, prompt: &str) -> Result<Command> {
        assert!(prompt.contains("Acceptance criteria:"));
        assert!(prompt.contains("test -f seed.txt"));
        let mut command = Command::new("/bin/sh");
        command.current_dir(run.worktree_path.as_ref().unwrap()).arg("-c")
            .arg("test -f seed.txt || exit 99; printf 'fixture log\n' > \"$1\"; sleep 2; exit \"$2\"")
            .arg("test-agent").arg(run.log_path.as_ref().unwrap()).arg(self.exit_code.to_string());
        Ok(command)
    }
}

struct TestWorkspace {
    db: PathBuf,
    fail: bool,
    exit_code: i32,
    worker: Mutex<Option<thread::JoinHandle<Result<Value>>>>,
}
impl TestWorkspace {
    fn new(db: &Path, fail: bool, exit_code: i32) -> Self {
        Self {
            db: db.into(),
            fail,
            exit_code,
            worker: Mutex::new(None),
        }
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
        let code = self.exit_code;
        *self.worker.lock().unwrap() = Some(thread::spawn(move || {
            runtime::session_with_provider(&db, &id, &token, &TestProvider { exit_code: code })
        }));
        Ok("01234567-89ab-4def-8123-456789abcdef".into())
    }
    fn capture(&self, _: &str) -> Result<String> {
        Ok("fixture terminal screen".into())
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

#[test]
fn successful_session_hands_off_to_validation_and_keeps_resources() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, 0);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["run"]["status"], "validating");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    let run = &detail.runs[0];
    assert!(Path::new(run.worktree_path.as_ref().unwrap()).exists());
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
    assert!(queue.supervisor_lease().unwrap().is_none());
    assert!(detail.events.iter().any(|e| e.kind == "agent_started"));
    assert!(supervise(&db, &repo, &backend).is_err()); // No unvalidated-run bypass.
}

#[test]
fn failed_agent_retains_worktree_and_does_not_complete_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, 7);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["run"]["status"], "failed");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(1).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert!(Path::new(detail.runs[0].worktree_path.as_ref().unwrap()).exists());
    assert!(queue.supervisor_lease().unwrap().is_none());
    assert!(queue.candidates().unwrap().is_empty());
}

#[test]
fn ambiguous_provisioning_failure_keeps_lease_and_planned_paths() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, true, 0);
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
    assert_eq!(queue.schema_version().unwrap(), 2);
    assert_eq!(queue.show(1).unwrap().task.title, "preserved");
    assert!(queue.supervisor_lease().unwrap().is_none());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_stale_owners() {
    use cmux_taskq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
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
}

#[test]
fn no_ready_task_releases_lease_without_creating_a_run() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(1, TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, 0);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "no_ready_task"
    );
    assert!(queue.supervisor_lease().unwrap().is_none());
    assert!(queue.show(1).unwrap().runs.is_empty());
}
