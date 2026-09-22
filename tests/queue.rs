use std::sync::{Arc, Barrier};

use cmux_taskq::{
    application::TaskQueue,
    domain::{
        ClaimOutcome, GoalEdit, GoalVerdict, NewGoal, NewTask, Provider, RunStatus, SupervisorMode,
        TaskAction, TaskStatus,
    },
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
        goal_id: None,
        context: String::new(),
    }
}

fn new_goal(title: &str) -> NewGoal {
    NewGoal {
        title: title.into(),
        description: "One problem several tasks solve".into(),
        acceptance: "Every task landed and the feature works end to end".into(),
        constraints: "Keep the module boundary".into(),
        doc: Some("docs/adr/0009-goal-groups-tasks.md".into()),
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
    // The dependent stays blocked; the claimed task owns its run.
    assert!(matches!(
        reopened.claim(BASE).unwrap(),
        ClaimOutcome::NoReadyTask
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

/// The read-only views the worker prompt is built from: direct predecessors
/// with their integrated run, and every task in progress.
#[test]
fn predecessors_carry_the_integrated_run_and_in_progress_tasks_are_listed() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id;
    let b = queue.add(new_task("b")).unwrap().id;
    let c = queue.add(new_task("c")).unwrap().id;
    queue.add_dependency(c, b).unwrap();
    queue.add_dependency(c, a).unwrap();
    assert!(queue.predecessors(a).unwrap().is_empty());
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // a landed through a run; b was completed without one (no integrated run).
    queue.transition(a, TaskAction::Ready).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(BASE).unwrap() else {
        panic!()
    };
    assert_eq!(
        queue
            .tasks_in_progress()
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        vec![a]
    );
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='integrated', result_commit=?2, run_dir='/nowhere' WHERE id=?1",
        rusqlite::params![run.id, BASE],
    )
    .unwrap();
    raw.execute(
        "UPDATE tasks SET status='completed' WHERE id IN (?1, ?2)",
        [a, b],
    )
    .unwrap();
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // Predecessors come in ID order regardless of the order the edges were added.
    let predecessors = queue.predecessors(c).unwrap();
    assert_eq!(
        predecessors
            .iter()
            .map(|p| (p.task.id, p.task.title.as_str()))
            .collect::<Vec<_>>(),
        vec![(a, "a"), (b, "b")]
    );
    let landed = predecessors[0].integrated_run.as_ref().unwrap();
    assert_eq!(landed.id, run.id);
    assert_eq!(landed.status, RunStatus::Integrated);
    assert_eq!(landed.result_commit.as_deref(), Some(BASE));
    assert_eq!(landed.run_dir.as_deref(), Some("/nowhere"));
    assert!(predecessors[1].integrated_run.is_none());
    // A task that does not exist has no predecessors rather than an error.
    assert!(queue.predecessors(99).unwrap().is_empty());
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
fn concurrent_connections_claim_each_ready_task_once() {
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
    // No queue-wide slot: both tasks are claimed, each exactly once.
    let mut claimed: Vec<i64> = runs.iter().map(|r| r.task_id).collect();
    claimed.sort();
    assert_eq!(claimed, [1, 2]);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::NoReadyTask))
            .count(),
        6
    );
    assert_eq!(queue.show(1).unwrap().runs.len(), 1);
    assert_eq!(queue.show(2).unwrap().runs.len(), 1);
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
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
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
    }
}

#[test]
fn database_constraints_guard_per_task_runs_and_integration_ownership() {
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
    // Another task may execute at the same time; the same task may not.
    assert!(insert(a).is_err());
    assert!(insert(b).is_ok());
    raw.execute("DELETE FROM task_runs WHERE id='extra'", [])
        .unwrap();
    raw.execute("UPDATE task_runs SET status='awaiting_integration'", [])
        .unwrap();
    assert!(insert(a).is_err());
    assert!(insert(b).is_ok());
    raw.execute("DELETE FROM task_runs WHERE id='extra'", [])
        .unwrap();
    raw.execute("UPDATE task_runs SET status='integrated'", [])
        .unwrap();
    // One integrated run per task; a later attempt may still be claimed.
    assert!(insert(a).is_ok());
    assert!(
        raw.execute(
            "UPDATE task_runs SET status='integrated' WHERE id='extra'",
            []
        )
        .is_err()
    );
}

#[test]
fn migration_to_v5_rebuilds_runs_moves_the_lease_and_keeps_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(include_str!("../migrations/0001_queue.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../migrations/0002_supervisor.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../migrations/0003_workspace_close.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 3).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('rebuilt','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit,last_error)
         VALUES ('run-failed',1,'failed','claude','claude','{BASE}',NULL,'rejected');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit,workspace_closed_at)
         VALUES ('run-awaiting',1,'awaiting_integration','claude','claude','{BASE}','{BASE}',1700000000);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-failed','validation_finished','{{}}');
         INSERT INTO run_processes(run_id,role,pid,exited_at,exit_code) VALUES ('run-awaiting','wrapper',1,1,0);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('orphaned','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-orphan',2,'running','claude','claude','{BASE}','old-token');
         INSERT INTO supervisor_leases(singleton,token,pid,heartbeat_at) VALUES (1,'old-token',4242,1700000000);"
    ))
    .unwrap();
    drop(raw);
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // The queue-wide lease became the orphaned run's lease; the slot index is gone.
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, "run-orphan");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert!(queue.run_lease("run-awaiting").unwrap().is_none());
    let raw = Connection::open(&path).unwrap();
    let objects: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE name IN ('supervisor_leases','one_executing_run_per_queue','run_leases','one_unfinished_run_per_task') ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(objects, ["one_unfinished_run_per_task", "run_leases"]);
    drop(raw);
    let detail = queue.show(1).unwrap();
    assert_eq!(
        detail
            .runs
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["run-failed", "run-awaiting"]
    );
    assert_eq!(detail.runs[0].last_error.as_deref(), Some("rejected"));
    assert_eq!(detail.runs[1].status, RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[1].workspace_closed_at, Some(1700000000));
    assert_eq!(detail.events[0].run_id.as_deref(), Some("run-failed"));
    assert_eq!(detail.processes[0].exit_code, Some(0));
    assert!(queue.candidates().unwrap().is_empty());
    // Enforcement is back on and the rebuilt table is the referenced one.
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'ghost','x','{}')",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_runs WHERE id='run-failed'", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_runs SET status='integrated' WHERE id='run-awaiting'",
            []
        )
        .is_ok()
    );
}

#[test]
fn migration_to_v6_adds_the_integration_statuses_and_the_single_slot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_queue.sql"),
        include_str!("../migrations/0002_supervisor.sql"),
        include_str!("../migrations/0003_workspace_close.sql"),
        include_str!("../migrations/0004_integration.sql"),
        include_str!("../migrations/0005_run_leases.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 5).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('awaiting','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-awaiting',1,'awaiting_integration','claude','claude','{BASE}','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-awaiting','validation_finished','{{}}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('running','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-running',2,'running','claude','claude','{BASE}','tok');
         INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES ('run-running','tok',4242,1700000000);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('other','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-other',3,'awaiting_integration','claude','claude','{BASE}','{BASE}');"
    ))
    .unwrap();
    drop(raw);
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(1).unwrap().runs[0].status,
        RunStatus::AwaitingIntegration
    );
    assert_eq!(queue.show(2).unwrap().runs[0].status, RunStatus::Running);
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, "run-running");
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id,
        "run-awaiting"
    );
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    let objects: Vec<String> = raw
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'one_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        objects,
        [
            "one_integrated_run_per_task",
            "one_integrating_run_per_queue",
            "one_unfinished_run_per_task"
        ]
    );
    // The new statuses are accepted; only one run may integrate at a time.
    raw.execute(
        "UPDATE task_runs SET status='integrating' WHERE id='run-awaiting'",
        [],
    )
    .unwrap();
    assert!(
        raw.execute(
            "UPDATE task_runs SET status='integrating' WHERE id='run-other'",
            []
        )
        .is_err()
    );
    raw.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id='run-other'",
        [],
    )
    .unwrap();
    // A parked run still owns its task.
    assert!(
        raw.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES ('extra',3,'claimed','claude','claude',?1)",
            [BASE],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'ghost','x','{}')",
            [],
        )
        .is_err()
    );
    assert!(queue.candidates().unwrap().is_empty());
    assert!(queue.transition(3, TaskAction::Cancel).is_err());
}

#[test]
fn migration_to_v7_adds_the_supervisor_registry_and_keeps_leases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_queue.sql"),
        include_str!("../migrations/0002_supervisor.sql"),
        include_str!("../migrations/0003_workspace_close.sql"),
        include_str!("../migrations/0004_integration.sql"),
        include_str!("../migrations/0005_run_leases.sql"),
        include_str!("../migrations/0006_merge_queue.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 6).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('running','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-running',1,'running','claude','claude','{BASE}','tok');
         INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES ('run-running','tok',4242,1700000000);"
    ))
    .unwrap();
    assert!(raw.execute("SELECT count(*) FROM supervisors", []).is_err());
    drop(raw);
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // A v6 supervisor that was running has no registration; its lease is intact.
    assert!(queue.supervisors().unwrap().is_empty());
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, "run-running");
    assert_eq!(leases[0].token, "tok");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert_eq!(queue.show(1).unwrap().runs[0].status, RunStatus::Running);

    // Registration: one row per token, a parallel limit of at least one,
    // heartbeat refreshed with the leases, removed only by deregistration.
    let before = queue.heartbeat("tok").unwrap();
    assert_eq!(before, 1);
    let registered = queue.register_supervisor("sv", 4243, 2).unwrap();
    assert_eq!(registered.token, "sv");
    assert_eq!(registered.pid, 4243);
    assert_eq!(registered.parallel, 2);
    assert!(registered.started_at > 1700000000);
    assert_eq!(registered.heartbeat_at, registered.started_at);
    assert!(queue.register_supervisor("sv", 4243, 2).is_err());
    assert!(queue.register_supervisor("zero", 4244, 0).is_err());
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE supervisors SET heartbeat_at=0 WHERE token='sv'", [])
        .unwrap();
    assert!(
        raw.execute(
            "INSERT INTO supervisors(token,pid,parallel) VALUES ('bad',1,0)",
            []
        )
        .is_err()
    );
    drop(raw);
    assert_eq!(queue.heartbeat("sv").unwrap(), 0); // No lease, still refreshed.
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].heartbeat_at >= registered.started_at);
    assert_eq!(queue.heartbeat("nobody").unwrap(), 0);

    // The mode is `up`'s to record once the process has registered; a
    // supervisor started by hand keeps none, and only the two modes fit.
    assert_eq!(listed[0].mode, None);
    assert_eq!(listed[0].workspace_id, None);
    queue
        .set_supervisor_mode("sv", SupervisorMode::InCmux, Some("ws-1"))
        .unwrap();
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed[0].mode, Some(SupervisorMode::InCmux));
    assert_eq!(listed[0].workspace_id.as_deref(), Some("ws-1"));
    queue
        .set_supervisor_mode("sv", SupervisorMode::Launchd, None)
        .unwrap();
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed[0].mode, Some(SupervisorMode::Launchd));
    assert_eq!(listed[0].workspace_id, None);
    assert!(
        queue
            .set_supervisor_mode("nobody", SupervisorMode::Launchd, None)
            .is_err()
    );
    let raw = Connection::open(&path).unwrap();
    assert!(
        raw.execute("UPDATE supervisors SET mode='by-hand' WHERE token='sv'", [])
            .is_err()
    );
    drop(raw);

    assert!(queue.deregister_supervisor("sv").unwrap());
    assert!(!queue.deregister_supervisor("sv").unwrap());
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
}

#[test]
fn migration_from_v6_adds_goals_and_keeps_tasks_runs_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_queue.sql"),
        include_str!("../migrations/0002_supervisor.sql"),
        include_str!("../migrations/0003_workspace_close.sql"),
        include_str!("../migrations/0004_integration.sql"),
        include_str!("../migrations/0005_run_leases.sql"),
        include_str!("../migrations/0006_merge_queue.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 6).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('landed','why','done','[\"cargo test\"]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-landed',1,'integrated','claude','claude','{BASE}','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,NULL,'task_created','{{}}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-landed','run_claimed','{{}}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('waiting','','','[]','ready');
         INSERT INTO task_dependencies(task_id,predecessor_id) VALUES (2,1);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (2,NULL,'task_created','{{}}');"
    ))
    .unwrap();
    drop(raw);
    let mut queue = SqliteQueue::open(&path).unwrap();
    // 0007 (supervisors), 0008 (goals) and 0009 (supervisor mode) are
    // applied together.
    assert_eq!(SqliteQueue::SCHEMA_VERSION, 9);
    assert_eq!(queue.schema_version().unwrap(), 9);
    let landed = queue.show(1).unwrap();
    assert_eq!(landed.task.title, "landed");
    assert_eq!(landed.task.description, "why");
    assert_eq!(landed.task.goal_id, None);
    assert_eq!(landed.task.context, "");
    assert_eq!(landed.task.status, TaskStatus::Completed);
    assert_eq!(landed.runs.len(), 1);
    assert_eq!(landed.runs[0].status, RunStatus::Integrated);
    // Events keep their IDs, order and run reference across the rebuild.
    assert_eq!(
        landed
            .events
            .iter()
            .map(|e| (e.id, e.task_id, e.run_id.as_deref()))
            .collect::<Vec<_>>(),
        [(1, Some(1), None), (2, Some(1), Some("run-landed"))]
    );
    let waiting = queue.show(2).unwrap();
    assert_eq!(waiting.task.goal_id, None);
    assert_eq!(waiting.task.context, "");
    assert_eq!(waiting.dependencies, [1]);
    assert_eq!(waiting.events[0].id, 3);
    assert_eq!(queue.candidates().unwrap()[0].id, 2);
    assert!(queue.list_goals().unwrap().is_empty());
    // New goals and events continue the sequences; foreign keys are enforced.
    let goal = queue.add_goal(new_goal("after")).unwrap();
    assert_eq!(goal.id, 1);
    assert_eq!(queue.show_goal(goal.id).unwrap().events[0].id, 4);
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        raw.execute("UPDATE tasks SET goal_id=99 WHERE id=2", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "INSERT INTO run_events(kind,payload) VALUES ('orphan','{}')",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "INSERT INTO run_events(goal_id,run_id,kind,payload) VALUES (1,'run-landed','x','{}')",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute("UPDATE goals SET verdict='achieved' WHERE id=1", [])
            .is_err()
    );
}

#[test]
fn goal_close_verdicts_depend_on_task_statuses() {
    let (dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("feature")).unwrap();
    assert!(!goal.is_closed());
    assert_eq!(
        goal.doc.as_deref(),
        Some("docs/adr/0009-goal-groups-tasks.md")
    );
    let mut spec = new_task("first");
    spec.goal_id = Some(goal.id);
    let a = queue.add(spec).unwrap();
    assert_eq!(a.goal_id, Some(goal.id));
    let mut spec = new_task("second");
    spec.goal_id = Some(goal.id);
    let b = queue.add(spec).unwrap().id;
    // Draft tasks block `achieved` but not `abandoned`.
    assert!(queue.close_goal(goal.id, GoalVerdict::Achieved).is_err());
    queue.transition(a.id, TaskAction::Ready).unwrap();
    queue.claim(BASE).unwrap();
    // An in-progress task blocks both verdicts.
    let error = format!(
        "{:#}",
        queue
            .close_goal(goal.id, GoalVerdict::Abandoned)
            .unwrap_err()
    );
    assert!(error.contains("1 task(s) in_progress"), "{error}");
    assert!(queue.close_goal(goal.id, GoalVerdict::Achieved).is_err());
    assert!(!queue.show_goal(goal.id).unwrap().closed);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [a.id])
        .unwrap();
    assert!(queue.close_goal(goal.id, GoalVerdict::Achieved).is_err());
    queue.transition(b, TaskAction::Cancel).unwrap();
    let closed = queue.close_goal(goal.id, GoalVerdict::Achieved).unwrap();
    assert!(closed.is_closed());
    assert_eq!(closed.verdict, Some(GoalVerdict::Achieved));
    assert!(closed.closed_at.is_some());
    assert!(queue.close_goal(goal.id, GoalVerdict::Abandoned).is_err());
    let summary = &queue.list_goals().unwrap()[0];
    assert!(summary.closed);
    assert_eq!(summary.verdict, Some(GoalVerdict::Achieved));
    assert_eq!(
        (
            summary.tasks.total,
            summary.tasks.completed,
            summary.tasks.canceled
        ),
        (2, 1, 1)
    );

    // Nothing running: an untouched goal may be abandoned with draft tasks left.
    let other = queue.add_goal(new_goal("dropped")).unwrap();
    let mut spec = new_task("never started");
    spec.goal_id = Some(other.id);
    let c = queue.add(spec).unwrap().id;
    queue.transition(c, TaskAction::Ready).unwrap();
    let abandoned = queue.close_goal(other.id, GoalVerdict::Abandoned).unwrap();
    assert_eq!(abandoned.verdict, Some(GoalVerdict::Abandoned));
    assert_eq!(queue.show(c).unwrap().task.status, TaskStatus::Ready);
    assert!(queue.show_goal(99).is_err());
    assert!(queue.close_goal(99, GoalVerdict::Achieved).is_err());
    assert!(
        queue
            .add_goal(NewGoal {
                title: " ".into(),
                ..NewGoal::default()
            })
            .is_err()
    );
}

#[test]
fn set_goal_and_add_with_goal_follow_the_dependency_rules() {
    let (_dir, mut queue) = fixture();
    let open = queue.add_goal(new_goal("open")).unwrap().id;
    let closed = queue.add_goal(new_goal("closed")).unwrap().id;
    queue.close_goal(closed, GoalVerdict::Achieved).unwrap();
    let mut spec = new_task("into closed goal");
    spec.goal_id = Some(closed);
    assert!(queue.add(spec).is_err());
    let mut spec = new_task("into missing goal");
    spec.goal_id = Some(99);
    assert!(queue.add(spec).is_err());
    let mut spec = new_task("invalid goal id");
    spec.goal_id = Some(0);
    assert!(queue.add(spec).is_err());
    assert!(queue.list().unwrap().is_empty());

    let task = queue.add(new_task("movable")).unwrap().id;
    assert_eq!(
        queue.set_goal(task, Some(open)).unwrap().goal_id,
        Some(open)
    );
    assert!(queue.set_goal(task, Some(closed)).is_err());
    assert!(queue.set_goal(task, Some(99)).is_err());
    assert_eq!(queue.show(task).unwrap().task.goal_id, Some(open));
    queue.transition(task, TaskAction::Ready).unwrap();
    assert_eq!(queue.set_goal(task, None).unwrap().goal_id, None);
    assert_eq!(
        queue.set_goal(task, Some(open)).unwrap().goal_id,
        Some(open)
    );
    queue.claim(BASE).unwrap();
    assert!(queue.set_goal(task, None).is_err());
    assert!(queue.set_goal(task, Some(open)).is_err());
    assert_eq!(queue.show(task).unwrap().task.goal_id, Some(open));
    assert!(queue.set_goal(99, Some(open)).is_err());
    let canceled = queue.add(new_task("canceled")).unwrap().id;
    queue.transition(canceled, TaskAction::Cancel).unwrap();
    assert!(queue.set_goal(canceled, Some(open)).is_err());
    // The goal's task list and counts follow the moves.
    let detail = queue.show_goal(open).unwrap();
    assert_eq!(
        detail
            .tasks
            .iter()
            .map(|t| (t.id, t.title.as_str(), t.status))
            .collect::<Vec<_>>(),
        [(task, "movable", TaskStatus::InProgress)]
    );
    assert_eq!(queue.list_goals().unwrap()[0].tasks.in_progress, 1);
}

#[test]
fn goal_events_are_recorded_without_a_run() {
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("tracked")).unwrap();
    assert!(queue.edit_goal(goal.id, GoalEdit::default()).is_err());
    assert!(
        queue
            .edit_goal(
                goal.id,
                GoalEdit {
                    title: Some(" ".into()),
                    ..GoalEdit::default()
                }
            )
            .is_err()
    );
    let edited = queue
        .edit_goal(
            goal.id,
            GoalEdit {
                title: Some("renamed".into()),
                doc: Some(String::new()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    assert_eq!(edited.title, "renamed");
    assert_eq!(edited.doc, None);
    assert_eq!(edited.acceptance, goal.acceptance);
    let task = queue.add(new_task("member")).unwrap().id;
    queue.set_goal(task, Some(goal.id)).unwrap();
    queue.set_goal(task, Some(goal.id)).unwrap(); // Unchanged: no event.
    queue.set_goal(task, None).unwrap();
    queue.close_goal(goal.id, GoalVerdict::Achieved).unwrap();
    assert!(queue.edit_goal(99, GoalEdit::default()).is_err());

    let detail = queue.show_goal(goal.id).unwrap();
    assert!(detail.closed);
    assert!(detail.tasks.is_empty());
    assert_eq!(
        detail
            .events
            .iter()
            .map(|e| (e.kind.as_str(), e.goal_id, e.task_id, e.run_id.clone()))
            .collect::<Vec<_>>(),
        [
            ("goal_created", Some(goal.id), None, None),
            ("goal_updated", Some(goal.id), None, None),
            ("goal_closed", Some(goal.id), None, None),
        ]
    );
    assert_eq!(detail.events[0].payload["goal"]["title"], "tracked");
    let updated = &detail.events[1].payload;
    assert_eq!(updated["old"]["title"], "tracked");
    assert_eq!(updated["new"]["title"], "renamed");
    assert_eq!(updated["old"]["doc"], "docs/adr/0009-goal-groups-tasks.md");
    assert!(updated["new"]["doc"].is_null());
    assert_eq!(detail.events[2].payload["verdict"], "achieved");
    assert_eq!(detail.events[2].payload["tasks"]["total"], 0);

    let task_events = queue.show(task).unwrap().events;
    assert_eq!(
        task_events
            .iter()
            .map(|e| (e.kind.as_str(), e.task_id, e.goal_id, e.run_id.clone()))
            .collect::<Vec<_>>(),
        [
            ("task_created", Some(task), None, None),
            ("task_goal_changed", Some(task), None, None),
            ("task_goal_changed", Some(task), None, None),
        ]
    );
    assert!(task_events[0].payload["goal_id"].is_null());
    assert_eq!(task_events[1].payload["from"], serde_json::Value::Null);
    assert_eq!(task_events[1].payload["to"], goal.id);
    assert_eq!(task_events[2].payload["from"], goal.id);
    assert!(task_events[2].payload["to"].is_null());
}
