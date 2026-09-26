//! Runtime tests: The sweep of workspaces, build outputs and worktrees.
use crate::runtime_support;

use runtime_support::*;

/// Supervisor options that sweep the workspaces of ended runs on every pass.
fn sweeping_options() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, true)
    }
}

/// The `workspace_closed` payloads of a run.
fn closes_of(queue: &SqliteQueue, run: &TaskRun) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "workspace_closed")
        .map(|e| e.payload)
        .collect()
}

/// Task 180: the supervisor's sweep closes the worker workspace of a failed
/// run the triage never takes: its task was canceled, or made ready and
/// run again, or a newer run of the in-progress task took its place. The
/// latest failed run of an in-progress task is the triage's and stays
/// open, a workspace cmux does not list gets no event, and worktrees and
/// branches stay.
#[test]
fn the_sweep_closes_the_workspaces_of_failed_runs_the_triage_does_not_take() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        // The stub `claude` gives no verdict: each waits for a person.
        assert_eq!(run.status(), RunStatus::Failed);
        assert!(run.workspace_closed_at().is_none());
    }
    assert!(backend.closed().is_empty());
    let workspace = |run: &TaskRun| run.workspace_id().unwrap().to_owned();

    // A person cancels task 1 and runs task 2 again; task 3 waits. While
    // cmux does not list task 2's first workspace, nothing is recorded of it.
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    backend.hidden.lock().unwrap().push(workspace(&first[1]));
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    let closed = closes_of(&queue, &first[0]);
    assert_eq!(
        closed,
        [json!({"workspace_id": workspace(&first[0]), "by": "supervisor", "reason": "superseded"})]
    );
    assert!(
        queue
            .run(first[0].id())
            .unwrap()
            .workspace_closed_at()
            .is_some()
    );
    assert!(backend.closed().contains(&workspace(&first[0])));
    assert!(closes_of(&queue, &first[1]).is_empty());
    let task2 = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(task2.runs.len(), 2);
    assert_eq!(task2.task.status(), TaskStatus::InProgress);

    // Listed again, the first run of task 2 is no longer its task's
    // latest: the next sweep closes it; the other runs are left alone.
    backend.hidden.lock().unwrap().clear();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(
        closes_of(&queue, &first[1]),
        [json!({"workspace_id": workspace(&first[1]), "by": "supervisor", "reason": "superseded"})]
    );
    assert_eq!(closes_of(&queue, &first[0]).len(), 1);
    // The triage's runs: task 3's only run, task 2's latest.
    assert!(closes_of(&queue, &first[2]).is_empty());
    assert!(!backend.closed().contains(&workspace(&first[2])));
    let latest = queue.show(TaskId::new(2)).unwrap().runs[1].clone();
    assert_eq!(latest.status(), RunStatus::Failed);
    assert!(!backend.closed().contains(&workspace(&latest)));
    // The run of the task that was readied keeps its worktree and branch;
    // the canceled task's go (task 376).
    let run = &first[1];
    assert!(Path::new(run.worktree_path().unwrap()).is_dir());
    git(
        &repo,
        &["rev-parse", "--verify", "--quiet", run.branch().unwrap()],
    );
    assert!(!Path::new(first[0].worktree_path().unwrap()).exists());
}

/// Task 180: a landed run's workspaces are swept too, however it landed:
/// a resume workspace the run's close left open, and the worker workspace
/// of a run landed by hand without its close. A cmux failure records
/// `cleanup_failed` and the sweep goes on to the next; a workspace cmux
/// does not list gets no event.
#[test]
fn the_sweep_closes_every_workspace_left_open_by_a_landed_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    // A resume workspace left open, one cmux no longer lists, and the
    // worker workspace nobody closed, as when a person integrated the run.
    for (workspace, attempt) in [("resume-ws", 1), ("gone-ws", 2)] {
        queue
            .record_runtime_event(
                run.id(),
                "workspace_created",
                json!({"workspace_id": workspace, "resume_attempt": attempt}),
            )
            .unwrap();
    }
    backend.list("resume-ws");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET workspace_id='hand-ws', workspace_closed_at=NULL WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    backend.list("hand-ws");
    let before = closes_of(&queue, &run).len();

    backend.close_fail = true;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failures = |run: &TaskRun| -> Vec<Value> {
        queue
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "cleanup_failed")
            .map(|e| e.payload)
            .collect()
    };
    let failed = failures(&run);
    let workspaces: Vec<&Value> = failed.iter().map(|f| &f["workspace_id"]).collect();
    assert_eq!(
        workspaces,
        [&json!("hand-ws"), &json!("resume-ws")],
        "{failed:?}"
    );
    assert!(failed.iter().all(|f| f["by"] == "supervisor"));
    assert_eq!(closes_of(&queue, &run).len(), before);

    backend.close_fail = false;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let closed = closes_of(&queue, &run);
    assert_eq!(
        closed[before..],
        [
            json!({"workspace_id": "hand-ws", "by": "supervisor", "reason": "ended"}),
            json!({"workspace_id": "resume-ws", "by": "supervisor", "reason": "ended"}),
        ]
    );
    assert!(queue.run(run.id()).unwrap().workspace_closed_at().is_some());
    assert!(backend.closed().contains(&"resume-ws".to_owned()));
    assert!(backend.closed().contains(&"hand-ws".to_owned()));
    assert!(!closed.iter().any(|c| c["workspace_id"] == "gone-ws"));

    // Nothing is listed any more: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(closes_of(&queue, &run).len(), closed.len());
    assert_eq!(failures(&run).len(), 2);
}

/// A worker script that commits, leaves build outputs in its worktree and
/// fails.
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug/deps llvm-cov-target; \
     head -c 65536 /dev/zero > target/debug/deps/big; ln target/debug/deps/big target/debug/big; \
     echo p > llvm-cov-target/profraw; receipt \"$(git rev-parse HEAD)\"; exit 7";

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Whether the branch exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// Task 376: a run that ends loses the build outputs of its worktree at
/// once (`target/`, `llvm-cov-target/`), recorded with the bytes they took;
/// its sources and commit stay. Once its task is canceled the worktree and
/// branch go; a task made ready again keeps its runs' worktrees; the sweep
/// removes build outputs left behind later; a tracked `target/` stays; the
/// checkout the supervisor was given is never touched.
#[test]
fn ended_runs_lose_their_build_outputs_and_canceled_tasks_their_worktrees() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    // The main checkout's own build outputs are not the runtime's.
    fs::create_dir_all(repo.join("target/debug")).unwrap();
    fs::write(repo.join("target/debug/mine"), "keep").unwrap();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        assert_eq!(run.status(), RunStatus::Failed);
        let worktree = Path::new(run.worktree_path().unwrap());
        assert!(!worktree.join("target").exists(), "{}", worktree.display());
        assert!(!worktree.join("llvm-cov-target").exists());
        assert!(worktree.join("change.txt").is_file());
        assert!(Path::new(run.run_dir().unwrap()).is_dir());
        let removed = payloads_of(&queue, run, "build_outputs_removed");
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert_eq!(
            removed[0]["paths"],
            json!([
                worktree.join("target").to_string_lossy(),
                worktree.join("llvm-cov-target").to_string_lossy()
            ])
        );
        assert_eq!(removed[0]["by"], "supervisor");
        // The hard link is counted once.
        let bytes = removed[0]["bytes"].as_u64().unwrap();
        assert!((65536..2 * 65536).contains(&bytes), "{bytes}");
    }
    assert!(repo.join("target/debug/mine").is_file());

    // Task 3 tracks a file under `target/`; build outputs appear again in
    // task 2's worktree; a person cancels task 1 and readies task 2.
    let tracked = Path::new(first[2].worktree_path().unwrap());
    fs::create_dir_all(tracked.join("target")).unwrap();
    fs::write(tracked.join("target/tracked.txt"), "source").unwrap();
    git(tracked, &["add", "target/tracked.txt"]);
    git(tracked, &["commit", "-q", "-m", "track target"]);
    let left = Path::new(first[1].worktree_path().unwrap()).join("target/debug");
    fs::create_dir_all(&left).unwrap();
    fs::write(left.join("left"), "x").unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();

    let canceled = &first[0];
    assert!(!Path::new(canceled.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, canceled.branch().unwrap()));
    let removed = payloads_of(&queue, canceled, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["path"], canceled.worktree_path().unwrap());
    assert_eq!(removed[0]["branch"], canceled.branch().unwrap());
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["by"], "supervisor");
    assert!(removed[0]["bytes"].as_u64().unwrap() > 0);
    assert!(Path::new(canceled.run_dir().unwrap()).is_dir());

    // Task 2 runs again: its first run keeps its worktree and branch but
    // loses what was built there since.
    let retried = &first[1];
    let worktree = Path::new(retried.worktree_path().unwrap());
    assert!(worktree.join("change.txt").is_file());
    assert!(!worktree.join("target").exists());
    assert!(branch_exists(&repo, retried.branch().unwrap()));
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert_eq!(queue.show(TaskId::new(2)).unwrap().runs.len(), 2);

    assert!(tracked.join("target/tracked.txt").is_file());
    assert_eq!(
        payloads_of(&queue, &first[2], "build_outputs_removed").len(),
        1
    );
    assert!(repo.join("target/debug/mine").is_file());

    // Nothing is left: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(payloads_of(&queue, canceled, "worktree_removed").len(), 1);
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert!(payloads_of(&queue, canceled, "cleanup_failed").is_empty());
}

/// Task 376: once a task completes, the worktree and branch of its
/// earlier failed run go too, and a worktree the landing could not remove
/// is removed by the sweep; the landed run keeps its run directory.
#[test]
fn a_completed_task_loses_the_worktrees_of_all_its_runs() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert!(Path::new(failed.worktree_path().unwrap()).is_dir());

    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    backend.script_for(1, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let landed = detail.runs[1].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);

    assert!(!Path::new(failed.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, failed.branch().unwrap()));
    let removed = payloads_of(&queue, &failed, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_completed");
    assert!(Path::new(failed.run_dir().unwrap()).is_dir());
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());

    // A landed run whose worktree is still there (its removal failed, or a
    // person put it back) is removed by the sweep; its branch was already
    // gone.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            landed.worktree_path().unwrap(),
            "main",
        ],
    );
    fs::create_dir_all(Path::new(landed.worktree_path().unwrap()).join("target")).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());
    let removed = payloads_of(&queue, &landed, "worktree_removed");
    assert_eq!(
        removed.last().unwrap()["reason"],
        "task_completed",
        "{removed:?}"
    );
    assert!(payloads_of(&queue, &landed, "cleanup_failed").is_empty());
    assert!(Path::new(landed.run_dir().unwrap()).is_dir());
}

/// Task 376: build outputs that cannot be removed record `cleanup_failed`
/// once per process and are removed by a later sweep.
#[test]
fn build_outputs_that_cannot_be_removed_are_retried_by_the_sweep() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let locked = Path::new(run.worktree_path().unwrap()).join("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("file"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failed = payloads_of(&queue, &run, "cleanup_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["path"], run.worktree_path().unwrap());
    assert_eq!(failed[0]["by"], "supervisor");
    assert!(locked.join("file").is_file());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 1);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!locked.exists());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 2);
}
