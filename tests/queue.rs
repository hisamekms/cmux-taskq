use std::sync::{Arc, Barrier};

use cmux_taskq::{
    application::TaskQueue,
    domain::{ClaimOutcome, NewTask, Provider, RunStatus, TaskAction, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use tempfile::TempDir;

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn new_task(title: &str) -> NewTask {
    NewTask {
        title: title.into(),
        description: "A small development task".into(),
        acceptance: "The regression test passes".into(),
        verification_commands: vec!["cargo test".into()],
        dependencies: vec![],
    }
}

fn fixture() -> (TempDir, SqliteQueue) {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    (dir, queue)
}

#[test]
fn tasks_dependencies_runs_and_events_survive_reopen() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("先行タスク 'quoted' 🦀")).unwrap();
    let mut spec = new_task("後続タスク");
    spec.dependencies = vec![a.id, a.id];
    let b = queue.add(spec).unwrap();
    queue.transition(a.id, TaskAction::Ready).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(BASE).unwrap() else {
        panic!()
    };
    assert_eq!(run.status, RunStatus::Claimed);
    assert_eq!(run.requested_provider, Provider::Claude);
    assert_eq!(run.actual_provider, Provider::Claude);
    drop(queue);

    let mut reopened = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let detail = reopened.show(a.id).unwrap();
    assert_eq!(detail.task.title, a.title);
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert_eq!(detail.task.verification_commands, vec!["cargo test"]);
    assert_eq!(detail.runs[0].id, run.id);
    assert_eq!(detail.runs[0].base_commit, BASE);
    assert!(detail.runs[0].workspace_id.is_none());
    assert_eq!(
        detail
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        ["task_created", "task_status_changed", "run_claimed"]
    );
    assert_eq!(detail.events[2].run_id.as_deref(), Some(run.id.as_str()));
    let second = reopened.show(b.id).unwrap();
    assert_eq!(second.dependencies, vec![a.id]);
    assert_eq!(second.events.len(), 2); // Duplicate dependency is idempotent.
    assert!(matches!(
        reopened.claim(BASE).unwrap(),
        ClaimOutcome::Busy { .. }
    ));
}

#[test]
fn invalid_registration_rolls_back_task_dependencies_and_events() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("existing")).unwrap();
    let mut spec = new_task("invalid dependency");
    spec.dependencies = vec![a.id, 999];
    assert!(queue.add(spec).is_err());
    assert!(queue.add(new_task(" \n\t")).is_err());
    let mut spec = new_task("blank verification");
    spec.verification_commands = vec![" ".into()];
    assert!(queue.add(spec).is_err());
    assert_eq!(queue.list().unwrap().len(), 1);
    let b = queue.add(new_task("next")).unwrap();
    assert_eq!(queue.show(b.id).unwrap().events.len(), 1);
    assert!(queue.show(b.id).unwrap().dependencies.is_empty());
    assert!(queue.show(999).is_err());
}

#[test]
fn dependencies_reject_self_cycles_and_missing_tasks_and_can_be_removed() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("b")).unwrap().id;
    let c = queue.add(new_task("c")).unwrap().id;
    queue.add_dependency(b, a).unwrap();
    queue.add_dependency(c, b).unwrap();
    assert!(queue.add_dependency(a, a).is_err());
    assert!(queue.add_dependency(a, c).is_err());
    assert!(queue.add_dependency(a, 999).is_err());
    assert!(queue.add_dependency(999, a).is_err());
    assert!(queue.show(a).unwrap().dependencies.is_empty());
    queue.transition(c, TaskAction::Ready).unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    queue.remove_dependency(c, b).unwrap();
    assert_eq!(queue.candidates().unwrap()[0].id, c);
    assert!(queue.remove_dependency(c, b).is_err());
    assert_eq!(
        queue.show(c).unwrap().events.last().unwrap().kind,
        "dependency_removed"
    );
}

#[test]
fn candidates_require_every_predecessor_to_be_completed() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("b")).unwrap().id;
    let c = queue.add(new_task("c")).unwrap().id;
    queue.add_dependency(c, a).unwrap();
    queue.add_dependency(c, b).unwrap();
    queue.transition(c, TaskAction::Ready).unwrap();
    assert!(matches!(
        queue.claim(BASE).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    // Seed lifecycle states that the supervisor/integration verifier will own.
    // No public complete command is exposed until that verifier exists.
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [a])
        .unwrap();
    for status in ["draft", "ready", "in_progress", "canceled"] {
        raw.execute(
            "UPDATE tasks SET status=?1 WHERE id=?2",
            rusqlite::params![status, b],
        )
        .unwrap();
        assert!(
            !queue.candidates().unwrap().iter().any(|t| t.id == c),
            "{status}"
        );
    }
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [b])
        .unwrap();
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        vec![c]
    );
    let ClaimOutcome::Claimed { run } = queue.claim(BASE).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id, c);
}

#[test]
fn manual_transitions_cannot_change_claimed_or_terminal_tasks() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    assert!(queue.transition(a, TaskAction::Draft).is_err());
    queue.transition(a, TaskAction::Ready).unwrap();
    queue.transition(a, TaskAction::Draft).unwrap();
    queue.transition(a, TaskAction::Ready).unwrap();
    assert!(queue.claim("main").is_err());
    assert_eq!(queue.show(a).unwrap().task.status, TaskStatus::Ready);
    queue.claim(BASE).unwrap();
    for action in [TaskAction::Ready, TaskAction::Draft, TaskAction::Cancel] {
        assert!(queue.transition(a, action).is_err());
    }
    let b = queue.add(new_task("b")).unwrap().id;
    assert!(queue.add_dependency(a, b).is_err());
    assert!(queue.remove_dependency(a, b).is_err());
    queue.transition(b, TaskAction::Cancel).unwrap();
    assert!(queue.transition(b, TaskAction::Ready).is_err());
    assert!(queue.add_dependency(b, a).is_err());
}

#[test]
fn concurrent_connections_claim_only_one_execution_slot() {
    let (dir, mut queue) = fixture();
    for title in ["first", "second"] {
        let task = queue.add(new_task(title)).unwrap();
        queue.transition(task.id, TaskAction::Ready).unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path().join("queue.db");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut queue = SqliteQueue::open(path).unwrap();
                barrier.wait();
                queue.claim(BASE).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let runs: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            ClaimOutcome::Claimed { run } => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task_id, 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Busy { run_id } if run_id == &runs[0].id))
            .count(),
        7
    );
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
    assert!(queue.show(2).unwrap().runs.is_empty());
}

#[test]
fn concurrent_opposite_edges_cannot_create_a_cycle() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("b")).unwrap().id;
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = [(a, b), (b, a)]
        .into_iter()
        .map(|(task, predecessor)| {
            let path = dir.path().join("queue.db");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut queue = SqliteQueue::open(path).unwrap();
                barrier.wait();
                queue.add_dependency(task, predecessor)
            })
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .filter_map(|w| w.join().unwrap().ok())
            .count(),
        1
    );
    assert_eq!(
        queue.show(a).unwrap().dependencies.len() + queue.show(b).unwrap().dependencies.len(),
        1
    );
}

#[test]
fn event_write_failure_rolls_back_claim_and_task_transition() {
    let (dir, mut queue) = fixture();
    let task = queue.add(new_task("atomic claim")).unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER reject_claim_event BEFORE INSERT ON run_events
        WHEN NEW.kind='run_claimed' BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
    )
    .unwrap();
    assert!(queue.claim(BASE).is_err());
    let detail = queue.show(task.id).unwrap();
    assert_eq!(detail.task.status, TaskStatus::Ready);
    assert!(detail.runs.is_empty());
    assert_eq!(detail.events.len(), 2);
    raw.execute_batch("DROP TRIGGER reject_claim_event;")
        .unwrap();
    assert!(matches!(
        queue.claim(BASE).unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
}

#[test]
fn awaiting_integration_keeps_dependents_blocked_but_frees_execution_slot() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("depends on a")).unwrap().id;
    let c = queue.add(new_task("independent")).unwrap().id;
    queue.add_dependency(b, a).unwrap();
    for id in [a, b, c] {
        queue.transition(id, TaskAction::Ready).unwrap();
    }
    let ClaimOutcome::Claimed { run } = queue.claim(BASE).unwrap() else {
        panic!()
    };
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='awaiting_integration', branch='taskq/a',
        worktree_path='/tmp/a', workspace_id='ws-a', receipt_path='/tmp/receipt.json',
        log_path='/tmp/run.log', result_commit=?1 WHERE id=?2",
        rusqlite::params![BASE, run.id],
    )
    .unwrap();
    drop(queue);
    let mut queue = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let detail = queue.show(a).unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status, RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[0].worktree_path.as_deref(), Some("/tmp/a"));
    assert_eq!(
        detail.runs[0].receipt_path.as_deref(),
        Some("/tmp/receipt.json")
    );
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [c]
    );
    let ClaimOutcome::Claimed { run } = queue.claim(BASE).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id, c);
}

#[test]
fn initialization_is_repeatable_and_preserves_existing_tasks() {
    let (dir, mut queue) = fixture();
    queue.add(new_task("preserved")).unwrap();
    drop(queue);
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    assert_eq!(queue.schema_version().unwrap(), 2);
    assert_eq!(queue.list().unwrap().len(), 1);
    assert!(SqliteQueue::open(dir.path().join("typo.db")).is_err());
    assert!(!dir.path().join("typo.db").exists());
}

#[test]
fn foreign_and_future_databases_are_rejected_without_rewriting_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.db");
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("CREATE TABLE other_app(value); INSERT INTO other_app VALUES ('keep');")
        .unwrap();
    assert!(SqliteQueue::init(&path).is_err());
    assert_eq!(
        raw.query_row("SELECT value FROM other_app", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let future = dir.path().join("future.db");
    drop(SqliteQueue::init(&future).unwrap());
    let raw = Connection::open(&future).unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    assert!(SqliteQueue::open(&future).is_err());
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        99
    );
}

#[test]
fn dependency_change_and_claim_are_serialized() {
    let (dir, mut queue) = fixture();
    let task = queue.add(new_task("ready task")).unwrap().id;
    let prerequisite = queue.add(new_task("unfinished prerequisite")).unwrap().id;
    queue.transition(task, TaskAction::Ready).unwrap();
    let mut claimant = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let mut editor = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let claim = std::thread::spawn(move || {
        barrier.wait();
        claimant.claim(BASE).unwrap()
    });
    let edit = std::thread::spawn(move || {
        other.wait();
        editor.add_dependency(task, prerequisite)
    });
    let claim = claim.join().unwrap();
    let edit = edit.join().unwrap();
    let detail = queue.show(task).unwrap();
    match claim {
        ClaimOutcome::Claimed { .. } => {
            assert!(edit.is_err());
            assert!(detail.dependencies.is_empty());
            assert_eq!(detail.task.status, TaskStatus::InProgress);
        }
        ClaimOutcome::NoReadyTask => {
            edit.unwrap();
            assert_eq!(detail.dependencies, [prerequisite]);
            assert_eq!(detail.task.status, TaskStatus::Ready);
            assert!(detail.runs.is_empty());
        }
        ClaimOutcome::Busy { .. } => panic!("no run existed before this claim"),
    }
}

#[test]
fn database_constraints_guard_execution_slot_and_integration_ownership() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("b")).unwrap().id;
    queue.transition(a, TaskAction::Ready).unwrap();
    queue.claim(BASE).unwrap();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    let insert = |id: i64| {
        raw.execute(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('extra',?1,'claimed','claude','claude',?2)",
        rusqlite::params![id, BASE],
    )
    };
    assert!(insert(b).is_err());
    raw.execute("UPDATE task_runs SET status='awaiting_integration'", [])
        .unwrap();
    assert!(insert(a).is_err());
    assert!(insert(b).is_ok());
}
