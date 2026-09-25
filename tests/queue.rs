use std::{
    path::Path,
    sync::{Arc, Barrier, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dagq::{
    VERSION,
    application::{
        Clock, Generators, IdGenerator, StatusFilter, TaskQuery, TaskStore, dependency_graph,
        timestamp,
    },
    domain::search::{SearchKind, SearchQuery, SearchRef},
    domain::{
        ClaimOutcome, CommitSha, EventId, EvidenceCheck, GoalEdit, GoalId, GoalStatus, GoalVerdict,
        NewGoal, NewNote, NewTask, NotePage, NoteQuery, NoteTarget, PlannerOrigin, PlannerOwner,
        Priority, ProposalId, ProposalStatus, Provider, RunId, RunStatus, Submission,
        SupervisorMode, TaskAction, TaskEdit, TaskId, TaskStatus,
    },
    infrastructure::{
        schema::{MIGRATIONS, floor_for},
        sqlite::SqliteQueue,
    },
};
use rusqlite::Connection;
use tempfile::TempDir;

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn base() -> CommitSha {
    CommitSha::try_from(BASE).unwrap()
}

fn new_task(title: &str) -> NewTask {
    NewTask {
        title: title.into(),
        description: "A small development task".into(),
        acceptance: "The regression test passes".into(),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        dependencies: vec![],
        goal_dependencies: Vec::new(),
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
        draft: false,
    }
}

/// An old queue after `dagq migrate`: opening it alone is refused and
/// leaves its `user_version` as it was (ADR-0045 decision 5).
fn migrated(path: &Path) -> SqliteQueue {
    let version = |path: &Path| -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    let before = version(path);
    let error = SqliteQueue::open(path).err().unwrap().to_string();
    assert!(error.contains("run `dagq migrate`"), "{error}");
    assert_eq!(version(path), before);
    let report = SqliteQueue::migrate(path, None, 1_700_000_000).unwrap();
    assert_eq!(report.previous_version, before);
    assert_eq!(report.schema_version, SqliteQueue::SCHEMA_VERSION);
    // Every migration so far is breaking, so the old queue was copied first.
    let backup = report.backup.unwrap();
    assert!(backup.ends_with(format!("backups/queue-{before}-1700000000.sqlite3")));
    assert_eq!(version(&backup), before);
    SqliteQueue::open(path).unwrap()
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
    spec.dependencies = vec![a.id(), a.id()];
    let b = queue.add(spec).unwrap();
    queue.transition(a.id(), TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.status(), RunStatus::Claimed);
    assert_eq!(run.requested_provider(), Provider::Claude);
    assert_eq!(run.actual_provider(), Provider::Claude);
    drop(queue);

    let mut reopened = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let detail = reopened.show(a.id()).unwrap();
    assert_eq!(detail.task.title(), a.title());
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.task.verification_commands(), vec!["cargo test"]);
    assert_eq!(detail.runs[0].id(), run.id());
    assert_eq!(detail.runs[0].base_commit().as_str(), BASE);
    assert!(detail.runs[0].workspace_id().is_none());
    assert_eq!(
        detail
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "task_created",
            "task_status_changed",
            "review_bypassed",
            "run_claimed"
        ]
    );
    assert_eq!(
        detail.events[3].run_id.as_ref().map(RunId::as_str),
        Some(run.id().as_str())
    );
    let second = reopened.show(b.id()).unwrap();
    assert_eq!(second.dependencies, vec![a.id()]);
    assert_eq!(second.events.len(), 2); // Duplicate dependency is idempotent.
    // The dependent stays blocked; the claimed task owns its run.
    assert!(matches!(
        reopened.claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
}

#[test]
fn invalid_registration_rolls_back_task_dependencies_and_events() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("existing")).unwrap();
    let mut spec = new_task("invalid dependency");
    spec.dependencies = vec![a.id(), TaskId::new(999)];
    assert!(queue.add(spec).is_err());
    assert!(queue.add(new_task(" \n\t")).is_err());
    let mut spec = new_task("blank verification");
    spec.verification_commands = vec![" ".into()];
    assert!(queue.add(spec).is_err());
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 1);
    let b = queue.add(new_task("next")).unwrap();
    assert_eq!(queue.show(b.id()).unwrap().events.len(), 1);
    assert!(queue.show(b.id()).unwrap().dependencies.is_empty());
    assert!(queue.show(TaskId::new(999)).is_err());
}

#[test]
fn dependencies_reject_self_cycles_and_missing_tasks_and_can_be_removed() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("b")).unwrap().id();
    let c = queue.add(new_task("c")).unwrap().id();
    queue.add_dependency(b, a).unwrap();
    queue.add_dependency(c, b).unwrap();
    assert!(queue.add_dependency(a, a).is_err());
    assert!(queue.add_dependency(a, c).is_err());
    assert!(queue.add_dependency(a, TaskId::new(999)).is_err());
    assert!(queue.add_dependency(TaskId::new(999), a).is_err());
    assert!(queue.show(a).unwrap().dependencies.is_empty());
    queue.transition(c, TaskAction::BypassReview).unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    queue.remove_dependency(c, b).unwrap();
    assert_eq!(queue.candidates().unwrap()[0].id(), c);
    assert!(queue.remove_dependency(c, b).is_err());
    assert_eq!(
        queue.show(c).unwrap().events.last().unwrap().kind,
        "dependency_removed"
    );
}

#[test]
fn candidates_require_every_predecessor_to_be_completed() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("b")).unwrap().id();
    let c = queue.add(new_task("c")).unwrap().id();
    queue.add_dependency(c, a).unwrap();
    queue.add_dependency(c, b).unwrap();
    queue.transition(c, TaskAction::BypassReview).unwrap();
    assert!(matches!(
        queue.claim(&base()).unwrap(),
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
            !queue.candidates().unwrap().iter().any(|t| t.id() == c),
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
            .map(|t| t.id())
            .collect::<Vec<_>>(),
        vec![c]
    );
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id(), c);
}

/// The read-only views the worker prompt is built from: direct predecessors
/// with their integrated run, and every task in progress.
#[test]
fn predecessors_carry_the_integrated_run_and_in_progress_tasks_are_listed() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("b")).unwrap().id();
    let c = queue.add(new_task("c")).unwrap().id();
    queue.add_dependency(c, b).unwrap();
    queue.add_dependency(c, a).unwrap();
    assert!(queue.predecessors(a).unwrap().is_empty());
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // a landed through a run; b was completed without one (no integrated run).
    queue.transition(a, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(
        queue
            .tasks_in_progress()
            .unwrap()
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>(),
        vec![a]
    );
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='integrated', result_commit=?2, run_dir='/nowhere' WHERE id=?1",
        rusqlite::params![run.id(), BASE],
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
            .map(|p| (p.task.id(), p.task.title()))
            .collect::<Vec<_>>(),
        vec![(a, "a"), (b, "b")]
    );
    let landed = predecessors[0].integrated_run.as_ref().unwrap();
    assert_eq!(landed.id(), run.id());
    assert_eq!(landed.status(), RunStatus::Integrated);
    assert_eq!(landed.result_commit().map(CommitSha::as_str), Some(BASE));
    // The stored path is not trusted: the run directory is resolved under the
    // queue's own `runs/` (ADR-0017).
    let run_dir = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("runs")
        .join(run.id().as_str());
    assert_eq!(landed.run_dir(), run_dir.to_str());
    assert!(predecessors[1].integrated_run.is_none());
    // A task that does not exist has no predecessors rather than an error.
    assert!(queue.predecessors(TaskId::new(99)).unwrap().is_empty());
}

/// A goal dependency holds the claim until the goal is closed as achieved,
/// and the prompt's view of the goal lists its completed tasks (ADR-0038).
#[test]
fn a_goal_dependency_holds_the_claim_until_the_goal_is_achieved() {
    let (dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("upstream")).unwrap().id();
    let mut spec = new_task("upstream work");
    spec.goal_id = Some(goal);
    let member = queue.add(spec).unwrap().id();
    let mut spec = new_task("follow-up draft");
    spec.goal_id = Some(goal);
    let follow_up = queue.add(spec).unwrap().id();
    let mut spec = new_task("downstream");
    spec.goal_dependencies = vec![goal, goal];
    let waiting = queue.add(spec).unwrap().id();
    assert_eq!(queue.show(waiting).unwrap().goal_dependencies, [goal]);
    // Adding the same edge again changes nothing.
    queue.add_goal_dependency(waiting, goal).unwrap();
    assert!(queue.add_goal_dependency(waiting, GoalId::new(99)).is_err());
    assert!(queue.add_goal_dependency(TaskId::new(99), goal).is_err());
    queue.transition(waiting, TaskAction::BypassReview).unwrap();
    queue.transition(member, TaskAction::BypassReview).unwrap();

    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id(), member);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='integrated', result_commit=?2 WHERE id=?1",
        rusqlite::params![run.id(), BASE],
    )
    .unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [member])
        .unwrap();
    // Every task of the open goal but a draft follow-up is done: still held.
    assert!(queue.candidates().unwrap().is_empty());
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    queue.transition(follow_up, TaskAction::Cancel).unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    queue.close_goal(goal, GoalVerdict::Achieved).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id(), waiting);

    let goals = queue.goal_predecessors(waiting).unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].goal.title(), "upstream");
    assert_eq!(
        goals[0]
            .tasks
            .iter()
            .map(|p| p.task.id())
            .collect::<Vec<_>>(),
        [member]
    );
    assert_eq!(
        goals[0].tasks[0]
            .integrated_run
            .as_ref()
            .and_then(|run| run.result_commit())
            .map(CommitSha::as_str),
        Some(BASE)
    );
    assert!(queue.goal_predecessors(member).unwrap().is_empty());
    // A claimed task's goal dependencies are fixed; it is a dependent of
    // the goal until it finishes.
    assert!(queue.add_goal_dependency(waiting, goal).is_err());
    assert!(queue.remove_goal_dependency(waiting, goal).is_err());
    let dependents = queue.show_goal(goal).unwrap().dependents;
    assert_eq!(
        dependents
            .iter()
            .map(|t| (t.id, t.status))
            .collect::<Vec<_>>(),
        [(waiting, TaskStatus::InProgress)]
    );
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [waiting])
        .unwrap();
    assert!(queue.show_goal(goal).unwrap().dependents.is_empty());
}

#[test]
fn manual_transitions_cannot_change_claimed_or_terminal_tasks() {
    let (_dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    assert!(queue.transition(a, TaskAction::Draft).is_err());
    queue.transition(a, TaskAction::BypassReview).unwrap();
    queue.transition(a, TaskAction::Draft).unwrap();
    queue.transition(a, TaskAction::BypassReview).unwrap();
    assert!(CommitSha::try_from("main").is_err());
    assert_eq!(queue.show(a).unwrap().task.status(), TaskStatus::Ready);
    queue.claim(&base()).unwrap();
    for action in [TaskAction::Ready, TaskAction::Draft, TaskAction::Cancel] {
        assert!(queue.transition(a, action).is_err());
    }
    let b = queue.add(new_task("b")).unwrap().id();
    assert!(queue.add_dependency(a, b).is_err());
    assert!(queue.remove_dependency(a, b).is_err());
    queue.transition(b, TaskAction::Cancel).unwrap();
    assert!(queue.transition(b, TaskAction::BypassReview).is_err());
    assert!(queue.add_dependency(b, a).is_err());
}

#[test]
fn concurrent_connections_claim_each_ready_task_once() {
    let (dir, mut queue) = fixture();
    for title in ["first", "second"] {
        let task = queue.add(new_task(title)).unwrap();
        queue
            .transition(task.id(), TaskAction::BypassReview)
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path().join("queue.db");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut queue = SqliteQueue::open(path).unwrap();
                barrier.wait();
                queue.claim(&base()).unwrap()
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
    let mut claimed: Vec<i64> = runs.iter().map(|r| r.task_id().as_i64()).collect();
    claimed.sort();
    assert_eq!(claimed, [1, 2]);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::NoReadyTask))
            .count(),
        6
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
    assert_eq!(queue.show(TaskId::new(2)).unwrap().runs.len(), 1);
}

#[test]
fn concurrent_opposite_edges_cannot_create_a_cycle() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("b")).unwrap().id();
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
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER reject_claim_event BEFORE INSERT ON run_events
        WHEN NEW.kind='run_claimed' BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
    )
    .unwrap();
    assert!(queue.claim(&base()).is_err());
    let detail = queue.show(task.id()).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert!(detail.runs.is_empty());
    assert_eq!(detail.events.len(), 3);
    raw.execute_batch("DROP TRIGGER reject_claim_event;")
        .unwrap();
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
}

#[test]
fn awaiting_integration_keeps_dependents_blocked_but_frees_execution_slot() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("depends on a")).unwrap().id();
    let c = queue.add(new_task("independent")).unwrap().id();
    queue.add_dependency(b, a).unwrap();
    for id in [a, b, c] {
        queue.transition(id, TaskAction::BypassReview).unwrap();
    }
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='awaiting_integration', branch='dagq/a',
        worktree_path='/tmp/a', workspace_id='ws-a', receipt_path='/tmp/receipt.json',
        log_path='/tmp/run.log', result_commit=?1 WHERE id=?2",
        rusqlite::params![BASE, run.id()],
    )
    .unwrap();
    drop(queue);
    let mut queue = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let detail = queue.show(a).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    // Stored paths are resolved again under the queue's `runs/<run-id>/`.
    let run_dir = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("runs")
        .join(run.id().as_str());
    assert_eq!(
        detail.runs[0].worktree_path(),
        run_dir.join("worktree").to_str()
    );
    assert_eq!(
        detail.runs[0].receipt_path(),
        run_dir.join("receipt.json").to_str()
    );
    assert!(detail.runs[0].run_dir().is_none());
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>(),
        [c]
    );
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.task_id(), c);
}

#[test]
fn initialization_is_repeatable_and_preserves_existing_tasks() {
    let (dir, mut queue) = fixture();
    queue.add(new_task("preserved")).unwrap();
    drop(queue);
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 1);
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
    // A newer queue whose floor is above this binary's schema: refused by
    // every entry point, and left as it was.
    let future = dir.path().join("future.db");
    drop(SqliteQueue::init(&future).unwrap());
    let raw = Connection::open(&future).unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    raw.execute("UPDATE schema_floor SET floor = 99", [])
        .unwrap();
    for error in [
        SqliteQueue::open(&future).err().unwrap(),
        SqliteQueue::init(&future).err().unwrap(),
        SqliteQueue::migrate(&future, None, 0).err().unwrap(),
    ] {
        let error = error.to_string();
        assert!(
            error.contains("unsupported queue schema version 99")
                && error.contains("older than schema 99")
                && error.contains("install a newer dagq"),
            "{error}"
        );
    }
    assert!(!SqliteQueue::schema(&future).unwrap().opens);
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        99
    );
}

/// What a later binary's compatible migration does: a table and a nullable
/// column this binary does not know (ADR-0045 decision 6).
fn apply_future_compatible_migration(raw: &Connection) {
    raw.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN future_hint TEXT;
         ALTER TABLE task_runs ADD COLUMN future_weight INTEGER NOT NULL DEFAULT 0;
         CREATE TABLE future_things (id INTEGER PRIMARY KEY, note TEXT);
         PRAGMA user_version = {};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
}

#[test]
fn a_newer_queue_within_the_floor_is_used_as_it_is() {
    let (dir, mut queue) = fixture();
    let first = queue.add(new_task("before")).unwrap().id();
    drop(queue);
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    apply_future_compatible_migration(&raw);
    let newer = SqliteQueue::SCHEMA_VERSION + 1;
    let schema = SqliteQueue::schema(&path).unwrap();
    assert_eq!(
        (schema.schema_version, schema.floor, schema.opens),
        (newer, floor_for(SqliteQueue::SCHEMA_VERSION), true)
    );
    assert!(schema.pending.is_empty());
    // Reads, writes and a claim all work on the columns this binary knows.
    let mut queue = SqliteQueue::open(&path).unwrap();
    let second = queue.add(new_task("after")).unwrap().id();
    queue.transition(second, TaskAction::BypassReview).unwrap();
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
    assert_eq!(queue.show(first).unwrap().task.title(), "before");
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 2);
    drop(queue);
    drop(SqliteQueue::init(&path).unwrap());
    // Nothing to apply, and a newer queue is never taken down to this binary.
    let report = SqliteQueue::migrate(&path, None, 0).unwrap();
    assert_eq!(
        (report.previous_version, report.schema_version),
        (newer, newer)
    );
    assert!(report.applied.is_empty() && report.backup.is_none());
    assert!(!dir.path().join("backups").exists());
    assert_eq!(
        raw.query_row("SELECT count(*) FROM future_things", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        newer
    );
}

/// A queue at schema 23, before the floor table: what the fixed binary
/// leaves until `dagq migrate` runs.
fn queue_before_the_floor(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 23).unwrap();
    path
}

#[test]
fn opening_or_initializing_an_older_queue_never_migrates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_before_the_floor(&dir);
    for error in [
        SqliteQueue::open(&path).err().unwrap(),
        SqliteQueue::init(&path).err().unwrap(),
    ] {
        assert_eq!(
            error.to_string(),
            format!(
                "queue schema version 23 is older than this binary's schema {}; run `dagq \
                 migrate` to apply the {} pending migration(s)",
                SqliteQueue::SCHEMA_VERSION,
                SqliteQueue::SCHEMA_VERSION - 23
            )
        );
    }
    let schema = SqliteQueue::schema(&path).unwrap();
    assert_eq!((schema.schema_version, schema.floor), (23, 23));
    assert!(!schema.opens);
    assert_eq!(
        schema
            .pending
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        vec![(24, false), (25, false), (26, true), (27, false)]
    );
    let raw = Connection::open(&path).unwrap();
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        23
    );
    // A copy from an earlier attempt in the same second is kept.
    std::fs::create_dir_all(dir.path().join("backups")).unwrap();
    std::fs::write(dir.path().join("backups/queue-23-5.sqlite3"), "earlier").unwrap();
    let report = SqliteQueue::migrate(&path, Some(&|_| false), 5).unwrap();
    assert_eq!(report.floor, floor_for(SqliteQueue::SCHEMA_VERSION));
    assert_eq!(report.applied.len(), 4);
    let backup = report.backup.unwrap();
    assert!(
        backup.ends_with("backups/queue-23-5-1.sqlite3"),
        "{backup:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("backups/queue-23-5.sqlite3")).unwrap(),
        "earlier"
    );
    let schema = SqliteQueue::schema(&path).unwrap();
    assert!(schema.opens && schema.pending.is_empty());
    SqliteQueue::open(&path).unwrap();
}

#[test]
fn a_breaking_migration_waits_for_an_idle_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_before_the_floor(&dir);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO supervisors(token, pid, parallel) VALUES ('live', 101, 1), ('dead', 102, 1);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('t','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-live',1,'running','claude','claude','{BASE}');
         INSERT INTO run_processes(run_id,role,pid) VALUES ('run-live','wrapper',103);"
    ))
    .unwrap();
    let alive = |pid: u32| pid != 102;
    let error = SqliteQueue::migrate(&path, Some(&alive), 0)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("breaking migration(s) 24, 25, 27")
            && error.contains("supervisor live (pid 101)")
            && !error.contains("dead")
            && error.contains("run run-live (running)")
            && error.contains("wrapper of run run-live (pid 103)")
            && error.contains("down --wait"),
        "{error}"
    );
    // Refused before anything was copied or applied.
    assert!(!dir.path().join("backups").exists());
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        23
    );
    // Once the supervisor and the run are gone, it goes through.
    raw.execute_batch(
        "DELETE FROM supervisors WHERE token='live';
         UPDATE run_processes SET exited_at=1, exit_code=0;
         UPDATE task_runs SET status='failed';",
    )
    .unwrap();
    let report = SqliteQueue::migrate(&path, Some(&alive), 0).unwrap();
    assert_eq!(report.schema_version, SqliteQueue::SCHEMA_VERSION);
    // A second migrate has nothing left to do.
    let again = SqliteQueue::migrate(&path, Some(&alive), 0).unwrap();
    assert!(again.applied.is_empty() && again.backup.is_none());
}

#[test]
fn dependency_change_and_claim_are_serialized() {
    let (dir, mut queue) = fixture();
    let task = queue.add(new_task("ready task")).unwrap().id();
    let prerequisite = queue.add(new_task("unfinished prerequisite")).unwrap().id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let mut claimant = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let mut editor = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let claim = std::thread::spawn(move || {
        barrier.wait();
        claimant.claim(&base()).unwrap()
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
            assert_eq!(detail.task.status(), TaskStatus::InProgress);
        }
        ClaimOutcome::NoReadyTask => {
            edit.unwrap();
            assert_eq!(detail.dependencies, [prerequisite]);
            assert_eq!(detail.task.status(), TaskStatus::Ready);
            assert!(detail.runs.is_empty());
        }
    }
}

#[test]
fn database_constraints_guard_per_task_runs_and_integration_ownership() {
    let (dir, mut queue) = fixture();
    let a = queue.add(new_task("a")).unwrap().id();
    let b = queue.add(new_task("b")).unwrap().id();
    queue.transition(a, TaskAction::BypassReview).unwrap();
    queue.claim(&base()).unwrap();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    let insert = |id: TaskId| {
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
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // The queue-wide lease became the orphaned run's lease; the slot index is gone.
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-orphan");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert!(
        queue
            .run_lease(&RunId::new("run-awaiting").unwrap())
            .unwrap()
            .is_none()
    );
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
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        detail
            .runs
            .iter()
            .map(|r| r.id().as_str())
            .collect::<Vec<_>>(),
        ["run-failed", "run-awaiting"]
    );
    assert_eq!(detail.runs[0].last_error(), Some("rejected"));
    assert_eq!(detail.runs[1].status(), RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[1].workspace_closed_at(), Some(1700000000));
    assert_eq!(
        detail.events[0].run_id.as_ref().map(RunId::as_str),
        Some("run-failed")
    );
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
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Running
    );
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-running");
    assert_eq!(
        queue
            .next_awaiting_integration()
            .unwrap()
            .unwrap()
            .id()
            .as_str(),
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
    assert!(
        queue
            .transition(TaskId::new(3), TaskAction::Cancel)
            .is_err()
    );
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
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // A v6 supervisor that was running has no registration; its lease is intact.
    assert!(queue.supervisors().unwrap().is_empty());
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-running");
    assert_eq!(leases[0].token, "tok");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::Running
    );

    // Registration: one row per token, a parallel limit of at least one,
    // heartbeat refreshed with the leases, removed only by deregistration.
    let before = queue.heartbeat("tok").unwrap();
    assert_eq!(before, 1);
    let registered = queue.register_supervisor("sv", 4243, 2, VERSION).unwrap();
    assert_eq!(registered.token, "sv");
    assert_eq!(registered.pid, 4243);
    assert_eq!(registered.parallel, 2);
    assert!(registered.started_at > 1700000000);
    assert_eq!(registered.heartbeat_at, registered.started_at);
    // The process records its own build; `up` reads it back to decide
    // whether that supervisor is one of its own (ADR-0014).
    assert_eq!(registered.binary_version.as_deref(), Some(VERSION));
    assert!(queue.register_supervisor("sv", 4243, 2, VERSION).is_err());
    assert!(queue.register_supervisor("zero", 4244, 0, VERSION).is_err());
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
    assert_eq!(listed[0].binary_version.as_deref(), Some(VERSION));
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

    // A registration a pre-0010 binary wrote has no version at all, which
    // is not this binary's version either, so `up` replaces it like any
    // other mismatch.
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO supervisors(token,pid,parallel) VALUES ('old',4245,1)",
        [],
    )
    .unwrap();
    drop(raw);
    let old = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token == "old")
        .unwrap();
    assert_eq!(old.binary_version, None);
    assert!(queue.deregister_supervisor("old").unwrap());

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
    let mut queue = migrated(&path);
    // 0007 (supervisors), 0008 (goals), 0009 (supervisor mode), 0010
    // (supervisor binary version), 0011 (session workspaces), 0012
    // (queue-level backend failures), 0013 (goal draft), 0014 (asks) and
    // 0015 (required evidence), 0016 (observer events and task-less
    // blocked asks), 0017 (the stuck_exit ask), 0018 (task paths), 0019
    // (goal dependencies), 0020 (task priority), 0021 (proposals) and 0022
    // (follow-up triage: task leases, follow_up_depth, the follow_up ask),
    // 0023 (planner sessions), 0024 (the schema floor), 0025 (the stalled
    // ask), 0026 (the search index) and 0027 (plan review) are applied
    // together.
    assert_eq!(SqliteQueue::SCHEMA_VERSION, 27);
    assert_eq!(queue.schema_version().unwrap(), 27);
    assert_eq!(
        queue
            .session_workspace(dagq::domain::SessionRole::Inbox)
            .unwrap(),
        None
    );
    let landed = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(landed.task.title(), "landed");
    assert_eq!(landed.task.description(), "why");
    assert_eq!(landed.task.goal_id(), None);
    assert_eq!(landed.task.context(), "");
    assert!(landed.task.paths().is_empty());
    assert_eq!(landed.task.priority(), Priority::Normal);
    assert_eq!(landed.task.status(), TaskStatus::Completed);
    assert_eq!(landed.runs.len(), 1);
    assert_eq!(landed.runs[0].status(), RunStatus::Integrated);
    // Events keep their IDs, order and run reference across the rebuild.
    assert_eq!(
        landed
            .events
            .iter()
            .map(|e| (
                e.id.as_i64(),
                e.task_id.map(TaskId::as_i64),
                e.run_id.as_ref().map(RunId::as_str)
            ))
            .collect::<Vec<_>>(),
        [(1, Some(1), None), (2, Some(1), Some("run-landed"))]
    );
    let waiting = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(waiting.task.goal_id(), None);
    assert_eq!(waiting.task.context(), "");
    // The task dependency survives every migration; no goal dependency appears.
    assert_eq!(waiting.dependencies, [TaskId::new(1)]);
    assert!(waiting.goal_dependencies.is_empty());
    assert_eq!(waiting.events[0].id, EventId::new(3));
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
    assert!(queue.list_goals().unwrap().is_empty());
    // New goals and events continue the sequences; foreign keys are enforced.
    let goal = queue.add_goal(new_goal("after")).unwrap();
    assert_eq!(goal.id(), GoalId::new(1));
    assert_eq!(
        queue.show_goal(goal.id()).unwrap().events[0].id,
        EventId::new(4)
    );
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
    // Only a failed backend call, the observer's own events and the events
    // of a task-less ask may belong to neither a task nor a goal.
    for kind in [
        "backend_call_failed",
        "observe_started",
        "observe_finished",
        "ask_opened",
        "ask_answered",
    ] {
        raw.execute(
            "INSERT INTO run_events(kind,payload) VALUES (?1,'{}')",
            [kind],
        )
        .unwrap();
    }
    // Only a blocked ask may belong to no task, and then to no run either.
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by) VALUES ('blocked','slots idle','observer')",
        [],
    )
    .unwrap();
    assert!(
        raw.execute(
            "INSERT INTO asks(kind,question,asked_by) VALUES ('decide','x','observer')",
            []
        )
        .is_err()
    );
    // 0017: the supervisor's stuck_exit ask about a run.
    raw.execute(
        "INSERT INTO asks(kind,task_id,run_id,question,asked_by)
         VALUES ('stuck_exit',1,'run-landed','session did not exit','supervisor')",
        [],
    )
    .unwrap();
    assert!(
        raw.execute(
            "INSERT INTO run_events(run_id,kind,payload) VALUES ('run-landed','backend_call_failed','{}')",
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
fn goals_of_a_version_12_queue_migrate_as_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v12.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_queue.sql"),
        include_str!("../migrations/0002_supervisor.sql"),
        include_str!("../migrations/0003_workspace_close.sql"),
        include_str!("../migrations/0004_integration.sql"),
        include_str!("../migrations/0005_run_leases.sql"),
        include_str!("../migrations/0006_merge_queue.sql"),
        include_str!("../migrations/0007_supervisors.sql"),
        include_str!("../migrations/0008_goals.sql"),
        include_str!("../migrations/0009_supervisor_mode.sql"),
        include_str!("../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../migrations/0011_session_workspaces.sql"),
        include_str!("../migrations/0012_queue_events.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 12).unwrap();
    raw.execute_batch(
        "INSERT INTO goals(title) VALUES ('existing');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id)
         VALUES ('in goal','','','[]','ready',1);",
    )
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // 0015: an existing task requires no evidence.
    assert!(
        queue
            .show(TaskId::new(1))
            .unwrap()
            .task
            .required_evidence()
            .is_empty()
    );
    let goal = queue.show_goal(GoalId::new(1)).unwrap().goal;
    assert_eq!(goal.status(), GoalStatus::Open);
    assert_eq!(queue.list_goals().unwrap()[0].status, GoalStatus::Open);
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(1));
    let raw = Connection::open(&path).unwrap();
    assert!(
        raw.execute("UPDATE goals SET status='closed' WHERE id=1", [])
            .is_err()
    );
}

#[test]
fn draft_goal_tasks_are_not_candidates_until_the_goal_is_ready() {
    let (dir, mut queue) = fixture();
    let draft = queue
        .add_goal(NewGoal {
            draft: true,
            ..new_goal("proposal")
        })
        .unwrap();
    assert_eq!(draft.status(), GoalStatus::Draft);
    let task = queue
        .add(NewTask {
            goal_id: Some(draft.id()),
            ..new_task("proposed")
        })
        .unwrap();
    let plain = queue.add(new_task("plain")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    queue
        .transition(plain.id(), TaskAction::BypassReview)
        .unwrap();
    let ids = |queue: &SqliteQueue| {
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&queue), [plain.id()]);
    let graph = queue.graph_input().unwrap();
    assert_eq!(graph.candidates, [plain.id()]);
    assert_eq!(graph.tasks[0].goal_status, Some(GoalStatus::Draft));
    assert_eq!(graph.tasks[1].goal_status, None);
    // The supervisor's claim skips the draft goal's task as well.
    let claimed = match queue.claim(&base()).unwrap() {
        ClaimOutcome::Claimed { run } => run.task_id(),
        ClaimOutcome::NoReadyTask => panic!("plain task is claimable"),
    };
    assert_eq!(claimed, plain.id());
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));

    let opened = queue.ready_goal(draft.id()).unwrap();
    assert_eq!(opened.status(), GoalStatus::Open);
    assert_eq!(ids(&queue), [task.id()]);
    let events = queue.show_goal(draft.id()).unwrap().events;
    assert_eq!(events.last().unwrap().kind, "goal_status_changed");
    assert_eq!(
        events.last().unwrap().payload,
        serde_json::json!({"from": "draft", "to": "open"})
    );
    assert_eq!(
        queue.ready_goal(draft.id()).unwrap_err().to_string(),
        format!("goal {} is not a draft", draft.id())
    );
    assert!(queue.ready_goal(GoalId::new(99)).is_err());
    let abandoned = queue
        .add_goal(NewGoal {
            draft: true,
            ..new_goal("rejected")
        })
        .unwrap();
    queue
        .close_goal(abandoned.id(), GoalVerdict::Abandoned)
        .unwrap();
    assert!(queue.ready_goal(abandoned.id()).is_err());
    drop(dir);
}

#[test]
fn notes_attach_to_tasks_runs_and_goals_and_page_by_cursor() {
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("observed")).unwrap();
    let task = queue
        .add(NewTask {
            goal_id: Some(goal.id()),
            ..new_task("in goal")
        })
        .unwrap();
    let other = queue.add(new_task("elsewhere")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let run = match queue.claim(&base()).unwrap() {
        ClaimOutcome::Claimed { run } => run,
        ClaimOutcome::NoReadyTask => panic!("task is claimable"),
    };
    let note = |target: NoteTarget, text: &str| NewNote {
        target,
        text: text.into(),
        kind: None,
        by: "observer".into(),
    };
    let on_goal = queue
        .add_note(note(NoteTarget::Goal(goal.id()), "goal note"))
        .unwrap();
    assert_eq!(on_goal.kind, "observation");
    assert_eq!(on_goal.goal_id, Some(goal.id()));
    assert_eq!(on_goal.task_id, None);
    assert_eq!(
        on_goal.payload,
        serde_json::json!({"text": "goal note", "kind": "note", "by": "observer"})
    );
    let on_run = queue
        .add_note(NewNote {
            kind: Some("slow".into()),
            ..note(NoteTarget::Run(run.id().clone()), "run note")
        })
        .unwrap();
    assert_eq!(on_run.task_id, Some(task.id()));
    assert_eq!(
        on_run.run_id.as_ref().map(RunId::as_str),
        Some(run.id().as_str())
    );
    let on_other = queue
        .add_note(note(NoteTarget::Task(other.id()), "other note"))
        .unwrap();
    assert!(
        queue
            .add_note(note(NoteTarget::Run(RunId::new("missing").unwrap()), "x"))
            .is_err()
    );
    assert!(
        queue
            .add_note(note(NoteTarget::Task(TaskId::new(99)), "x"))
            .is_err()
    );
    assert!(
        queue
            .add_note(note(NoteTarget::Goal(GoalId::new(99)), "x"))
            .is_err()
    );
    assert!(
        queue
            .add_note(note(NoteTarget::Goal(goal.id()), " "))
            .is_err()
    );

    let ids = |page: &NotePage| page.notes.iter().map(|n| n.id).collect::<Vec<_>>();
    let all = queue
        .notes(&NoteQuery {
            limit: 10,
            ..NoteQuery::default()
        })
        .unwrap();
    assert_eq!(ids(&all), [on_goal.id, on_run.id, on_other.id]);
    assert_eq!(all.cursor, on_other.id);
    let latest = queue
        .notes(&NoteQuery {
            limit: 2,
            ..NoteQuery::default()
        })
        .unwrap();
    assert_eq!(ids(&latest), [on_run.id, on_other.id]);
    let in_goal = queue
        .notes(&NoteQuery {
            goal_id: Some(goal.id()),
            limit: 10,
            ..NoteQuery::default()
        })
        .unwrap();
    assert_eq!(ids(&in_goal), [on_goal.id, on_run.id]);
    let of_task = queue
        .notes(&NoteQuery {
            task_id: Some(task.id()),
            limit: 10,
            ..NoteQuery::default()
        })
        .unwrap();
    assert_eq!(ids(&of_task), [on_run.id]);
    let after = queue
        .notes(&NoteQuery {
            since: Some(on_goal.id),
            limit: 1,
            ..NoteQuery::default()
        })
        .unwrap();
    assert_eq!(ids(&after), [on_run.id]);
    assert_eq!(after.cursor, on_run.id);
    let none = queue
        .notes(&NoteQuery {
            since: Some(on_other.id),
            limit: 1,
            ..NoteQuery::default()
        })
        .unwrap();
    assert!(none.notes.is_empty());
    assert_eq!(none.cursor, on_other.id);
    assert!(queue.notes(&NoteQuery::default()).is_err());
}

#[test]
fn goal_close_verdicts_depend_on_task_statuses() {
    let (dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("feature")).unwrap();
    assert!(!goal.is_closed());
    assert_eq!(goal.doc(), Some("docs/adr/0009-goal-groups-tasks.md"));
    let mut spec = new_task("first");
    spec.goal_id = Some(goal.id());
    let a = queue.add(spec).unwrap();
    assert_eq!(a.goal_id(), Some(goal.id()));
    let mut spec = new_task("second");
    spec.goal_id = Some(goal.id());
    let b = queue.add(spec).unwrap().id();
    // Draft tasks block `achieved` but not `abandoned`.
    assert!(queue.close_goal(goal.id(), GoalVerdict::Achieved).is_err());
    queue.transition(a.id(), TaskAction::BypassReview).unwrap();
    queue.claim(&base()).unwrap();
    // An in-progress task blocks both verdicts.
    let error = format!(
        "{:#}",
        queue
            .close_goal(goal.id(), GoalVerdict::Abandoned)
            .unwrap_err()
    );
    assert!(error.contains("1 task(s) in_progress"), "{error}");
    assert!(queue.close_goal(goal.id(), GoalVerdict::Achieved).is_err());
    assert!(!queue.show_goal(goal.id()).unwrap().closed);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [a.id()])
        .unwrap();
    assert!(queue.close_goal(goal.id(), GoalVerdict::Achieved).is_err());
    queue.transition(b, TaskAction::Cancel).unwrap();
    let closed = queue.close_goal(goal.id(), GoalVerdict::Achieved).unwrap();
    assert!(closed.is_closed());
    assert_eq!(closed.verdict(), Some(GoalVerdict::Achieved));
    assert!(closed.closed_at().is_some());
    assert!(queue.close_goal(goal.id(), GoalVerdict::Abandoned).is_err());
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
    spec.goal_id = Some(other.id());
    let c = queue.add(spec).unwrap().id();
    queue.transition(c, TaskAction::BypassReview).unwrap();
    let abandoned = queue
        .close_goal(other.id(), GoalVerdict::Abandoned)
        .unwrap();
    assert_eq!(abandoned.verdict(), Some(GoalVerdict::Abandoned));
    assert_eq!(queue.show(c).unwrap().task.status(), TaskStatus::Ready);
    assert!(queue.show_goal(GoalId::new(99)).is_err());
    assert!(
        queue
            .close_goal(GoalId::new(99), GoalVerdict::Achieved)
            .is_err()
    );
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
    let open = queue.add_goal(new_goal("open")).unwrap().id();
    let closed = queue.add_goal(new_goal("closed")).unwrap().id();
    queue.close_goal(closed, GoalVerdict::Achieved).unwrap();
    let mut spec = new_task("into closed goal");
    spec.goal_id = Some(closed);
    assert!(queue.add(spec).is_err());
    let mut spec = new_task("into missing goal");
    spec.goal_id = Some(GoalId::new(99));
    assert!(queue.add(spec).is_err());
    let mut spec = new_task("invalid goal id");
    spec.goal_id = Some(GoalId::new(0));
    assert!(queue.add(spec).is_err());
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 0);

    let task = queue.add(new_task("movable")).unwrap().id();
    assert_eq!(
        queue.set_goal(task, Some(open)).unwrap().goal_id(),
        Some(open)
    );
    assert!(queue.set_goal(task, Some(closed)).is_err());
    assert!(queue.set_goal(task, Some(GoalId::new(99))).is_err());
    assert_eq!(queue.show(task).unwrap().task.goal_id(), Some(open));
    queue.transition(task, TaskAction::BypassReview).unwrap();
    assert_eq!(queue.set_goal(task, None).unwrap().goal_id(), None);
    assert_eq!(
        queue.set_goal(task, Some(open)).unwrap().goal_id(),
        Some(open)
    );
    queue.claim(&base()).unwrap();
    assert!(queue.set_goal(task, None).is_err());
    assert!(queue.set_goal(task, Some(open)).is_err());
    assert_eq!(queue.show(task).unwrap().task.goal_id(), Some(open));
    assert!(queue.set_goal(TaskId::new(99), Some(open)).is_err());
    let canceled = queue.add(new_task("canceled")).unwrap().id();
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

/// `add --paths` stores the globs once each; `set_paths` replaces them on a
/// draft or ready task only, records `task_paths_changed` when they change,
/// and an invalid glob is refused either way (ADR-0029).
#[test]
fn paths_are_stored_and_replaced_while_the_task_is_editable() {
    let (_dir, mut queue) = fixture();
    let globs = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let mut spec = new_task("absolute");
    spec.paths = globs(&["/docs/**"]);
    let error = queue.add(spec).unwrap_err().to_string();
    assert!(
        error.contains("invalid --paths glob \"/docs/**\""),
        "{error}"
    );
    let mut spec = new_task("docs only");
    spec.paths = globs(&["docs/**", "*.md", "docs/**"]);
    let task = queue.add(spec).unwrap();
    assert_eq!(task.paths(), globs(&["docs/**", "*.md"]));
    assert_eq!(queue.show(task.id()).unwrap().task.paths(), task.paths());
    // No change, no event.
    queue
        .set_paths(task.id(), globs(&["docs/**", "*.md"]))
        .unwrap();
    assert!(queue.set_paths(task.id(), globs(&["../x"])).is_err());
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let widened = queue
        .set_paths(task.id(), globs(&["docs/**", "src/**"]))
        .unwrap();
    assert_eq!(widened.paths(), globs(&["docs/**", "src/**"]));
    assert!(
        queue
            .set_paths(task.id(), Vec::new())
            .unwrap()
            .paths()
            .is_empty()
    );
    let changes: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "task_paths_changed")
        .map(|e| (e.run_id, e.payload))
        .collect();
    assert_eq!(
        changes,
        [
            (
                None,
                serde_json::json!({"from": ["docs/**", "*.md"], "to": ["docs/**", "src/**"]})
            ),
            (
                None,
                serde_json::json!({"from": ["docs/**", "src/**"], "to": []})
            ),
        ]
    );
    // Once claimed, the run keeps the scope it started with.
    queue.claim(&base()).unwrap();
    let error = queue
        .set_paths(task.id(), globs(&["docs/**"]))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "the paths can only be changed for draft, submitted or ready tasks"
    );
    assert!(queue.set_paths(TaskId::new(99), Vec::new()).is_err());
}

/// `edit_task` replaces the given fields of a draft task, records
/// `task_edited` with the old and new value of each field that changed (no
/// event when nothing does), and refuses a task that is no longer a draft
/// (ADR-0041 decision 9).
#[test]
fn edit_task_replaces_draft_fields_and_records_the_change() {
    let (_dir, mut queue) = fixture();
    let task = queue.add(new_task("first title")).unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), TaskEdit::default())
            .unwrap_err()
            .to_string(),
        "task edit changes nothing"
    );
    let edited = queue
        .edit_task(
            task.id(),
            TaskEdit {
                title: Some("second title".into()),
                verification_commands: Some(vec!["cargo fmt --all --check".into()]),
                required_evidence: Some(vec![EvidenceCheck::E2e]),
                paths: Some(vec!["docs/**".into()]),
                context: Some("why".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
    assert_eq!(edited.title(), "second title");
    assert_eq!(edited.description(), "A small development task");
    let shown = queue.show(task.id()).unwrap().task;
    assert_eq!(shown.verification_commands(), ["cargo fmt --all --check"]);
    assert_eq!(shown.required_evidence(), [EvidenceCheck::E2e]);
    assert_eq!(shown.paths(), ["docs/**"]);
    assert_eq!(shown.context(), "why");
    // The same values again change nothing and record nothing.
    queue
        .edit_task(
            task.id(),
            TaskEdit {
                title: Some("second title".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
    let edits: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "task_edited")
        .map(|e| (e.run_id, e.payload))
        .collect();
    assert_eq!(
        edits,
        [(
            None,
            serde_json::json!({
                "from": {
                    "title": "first title",
                    "verification_commands": ["cargo test"],
                    "required_evidence": [],
                    "paths": [],
                    "context": "",
                },
                "to": {
                    "title": "second title",
                    "verification_commands": ["cargo fmt --all --check"],
                    "required_evidence": ["e2e"],
                    "paths": ["docs/**"],
                    "context": "why",
                },
            })
        )]
    );
    // A bad value is refused before the task is read.
    assert!(
        queue
            .edit_task(
                TaskId::new(99),
                TaskEdit {
                    paths: Some(vec!["../x".into()]),
                    ..TaskEdit::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("invalid --paths glob")
    );
    let change = || TaskEdit {
        description: Some("late".into()),
        ..TaskEdit::default()
    };
    assert!(queue.edit_task(TaskId::new(99), change()).is_err());
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), change())
            .unwrap_err()
            .to_string(),
        format!(
            "task {} is ready; only a draft or submitted task can be edited",
            task.id()
        )
    );
    queue.claim(&base()).unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), change())
            .unwrap_err()
            .to_string(),
        format!(
            "task {} is in_progress; only a draft or submitted task can be edited",
            task.id()
        )
    );
    assert_eq!(
        queue.show(task.id()).unwrap().task.description(),
        "A small development task"
    );
}

/// `add` stores the priority and `set_priority` changes it on a draft or
/// ready task only, recording `task_priority_changed` when it changes; the
/// column refuses anything outside 0..=4 (ADR-0040 decision 4).
#[test]
fn priority_is_stored_changed_while_editable_and_checked_by_the_schema() {
    let (dir, mut queue) = fixture();
    let mut spec = new_task("urgent");
    spec.priority = Priority::Urgent;
    let task = queue.add(spec).unwrap();
    assert_eq!(task.priority(), Priority::Urgent);
    assert_eq!(
        queue.show(task.id()).unwrap().task.priority(),
        Priority::Urgent
    );
    // No change, no event.
    queue.set_priority(task.id(), Priority::Urgent).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let low = queue.set_priority(task.id(), Priority::Low).unwrap();
    assert_eq!(low.priority(), Priority::Low);
    let changes: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "task_priority_changed")
        .map(|e| e.payload)
        .collect();
    assert_eq!(
        changes,
        [serde_json::json!({"from": "urgent", "to": "low"})]
    );
    queue.claim(&base()).unwrap();
    assert_eq!(
        queue
            .set_priority(task.id(), Priority::Interrupt)
            .unwrap_err()
            .to_string(),
        "the priority can only be changed for draft, submitted or ready tasks"
    );
    assert!(queue.set_priority(TaskId::new(99), Priority::Low).is_err());

    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    for value in [-1, 5] {
        let error = raw
            .execute("UPDATE tasks SET priority=?1 WHERE id=1", [value])
            .unwrap_err()
            .to_string();
        assert!(error.contains("CHECK constraint failed"), "{error}");
    }
    raw.execute("UPDATE tasks SET priority=4 WHERE id=1", [])
        .unwrap();
    assert_eq!(
        queue.show(task.id()).unwrap().task.priority(),
        Priority::Interrupt
    );
}

/// `candidates`, `graph` and successive claims follow one order: highest
/// effective priority, then most unblocks, then lowest ID. A ready task
/// passes its priority to what it waits for; a draft task and a task of a
/// draft goal do not.
#[test]
fn candidates_graph_and_claims_share_the_priority_order() {
    let (_dir, mut queue) = fixture();
    let mut draft_goal = new_goal("parked goal");
    draft_goal.draft = true;
    let draft_goal = queue.add_goal(draft_goal).unwrap().id();
    let mut add = |title: &str, priority: Priority, depends_on: &[TaskId], goal: Option<GoalId>| {
        let mut spec = new_task(title);
        spec.priority = priority;
        spec.dependencies = depends_on.to_vec();
        spec.goal_id = goal;
        queue.add(spec).unwrap().id()
    };
    let plain = add("plain", Priority::Normal, &[], None);
    let releasing = add("releasing", Priority::Normal, &[], None);
    let parked = add("parked", Priority::Interrupt, &[releasing], None);
    let low = add("low", Priority::Low, &[], None);
    let lifted = add("lifted", Priority::Normal, &[], None);
    let waiter = add("waiter", Priority::Urgent, &[lifted], None);
    let high = add("high", Priority::High, &[], None);
    let in_draft_goal = add(
        "in draft goal",
        Priority::Interrupt,
        &[low],
        Some(draft_goal),
    );
    for id in [plain, releasing, low, lifted, waiter, high, in_draft_goal] {
        queue.transition(id, TaskAction::BypassReview).unwrap();
    }
    assert_eq!(queue.show(parked).unwrap().task.status(), TaskStatus::Draft);

    let expected = [lifted, high, releasing, plain, low];
    let candidates: Vec<TaskId> = queue.candidates().unwrap().iter().map(|t| t.id()).collect();
    assert_eq!(candidates, expected);
    let graph = dependency_graph(queue.graph_input().unwrap(), None);
    assert_eq!(graph.candidates, expected);
    let node = |id: TaskId| graph.tasks.iter().find(|n| n.id == id).unwrap();
    assert_eq!(node(lifted).priority, Priority::Normal);
    assert_eq!(node(lifted).effective_priority, Priority::Urgent);
    assert_eq!(node(releasing).effective_priority, Priority::Normal);
    assert_eq!(node(low).effective_priority, Priority::Low);
    assert_eq!(node(parked).effective_priority, Priority::Interrupt);

    let mut claimed = Vec::new();
    while let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() {
        claimed.push(run.task_id());
    }
    assert_eq!(claimed, expected);
}

#[test]
fn goal_events_are_recorded_without_a_run() {
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("tracked")).unwrap();
    assert!(queue.edit_goal(goal.id(), GoalEdit::default()).is_err());
    assert!(
        queue
            .edit_goal(
                goal.id(),
                GoalEdit {
                    title: Some(" ".into()),
                    ..GoalEdit::default()
                }
            )
            .is_err()
    );
    let edited = queue
        .edit_goal(
            goal.id(),
            GoalEdit {
                title: Some("renamed".into()),
                doc: Some(String::new()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    assert_eq!(edited.title(), "renamed");
    assert_eq!(edited.doc(), None);
    assert_eq!(edited.acceptance(), goal.acceptance());
    let task = queue.add(new_task("member")).unwrap().id();
    queue.set_goal(task, Some(goal.id())).unwrap();
    queue.set_goal(task, Some(goal.id())).unwrap(); // Unchanged: no event.
    queue.set_goal(task, None).unwrap();
    queue.close_goal(goal.id(), GoalVerdict::Achieved).unwrap();
    assert!(
        queue
            .edit_goal(GoalId::new(99), GoalEdit::default())
            .is_err()
    );

    let detail = queue.show_goal(goal.id()).unwrap();
    assert!(detail.closed);
    assert!(detail.tasks.is_empty());
    assert_eq!(
        detail
            .events
            .iter()
            .map(|e| (e.kind.as_str(), e.goal_id, e.task_id, e.run_id.clone()))
            .collect::<Vec<_>>(),
        [
            ("goal_created", Some(goal.id()), None, None),
            ("goal_updated", Some(goal.id()), None, None),
            ("goal_closed", Some(goal.id()), None, None),
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
    assert_eq!(task_events[1].payload["to"], goal.id().as_i64());
    assert_eq!(task_events[2].payload["from"], goal.id().as_i64());
    assert!(task_events[2].payload["to"].is_null());
}

fn listed_ids(queue: &SqliteQueue, query: &TaskQuery) -> Vec<TaskId> {
    let page = queue.list(query).unwrap();
    page.tasks.iter().map(|task| task.id).collect()
}

#[test]
fn list_defaults_to_unfinished_tasks_newest_first_with_compact_items() {
    let (dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("grouped")).unwrap().id();
    let a = queue.add(new_task("landed")).unwrap().id();
    let b = queue.add(new_task("dropped")).unwrap().id();
    let mut spec = new_task("claimed");
    spec.dependencies = vec![a];
    spec.goal_id = Some(goal);
    let c = queue.add(spec).unwrap().id();
    let d = queue.add(new_task("waiting")).unwrap().id();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [a])
        .unwrap();
    queue.transition(b, TaskAction::Cancel).unwrap();
    queue.transition(c, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    queue.transition(d, TaskAction::BypassReview).unwrap();

    let page = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.next, None);
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        serde_json::json!({
            "tasks": [
                {"id": d, "status": "ready", "priority": "normal", "title": "waiting",
                 "goal_id": null,
                 "dependencies": [], "goal_dependencies": [], "latest_run": null},
                {"id": c, "status": "in_progress", "priority": "normal", "title": "claimed",
                 "goal_id": goal,
                 "dependencies": [a], "goal_dependencies": [],
                 "latest_run": {"id": run.id(), "status": "claimed"}},
            ],
            "next": null,
            "total": 2,
        })
    );

    let all = TaskQuery {
        status: StatusFilter::Any,
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &all), vec![d, c, b, a]);
    let only = TaskQuery {
        status: StatusFilter::Only(vec![TaskStatus::Ready, TaskStatus::Canceled]),
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &only), vec![d, b]);
    let of_goal = TaskQuery {
        goal_id: Some(goal),
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &of_goal), vec![c]);
    // Status and goal combine with AND.
    let ready_of_goal = TaskQuery {
        status: StatusFilter::Only(vec![TaskStatus::Ready]),
        goal_id: Some(goal),
        ..TaskQuery::default()
    };
    let page = queue.list(&ready_of_goal).unwrap();
    assert!(page.tasks.is_empty());
    assert_eq!(page.total, 0);
}

#[test]
fn list_full_items_carry_every_task_field() {
    let (_dir, mut queue) = fixture();
    let mut spec = new_task("detailed");
    spec.context = "why it exists".into();
    let task = queue.add(spec).unwrap();
    let full = TaskQuery {
        full: true,
        ..TaskQuery::default()
    };
    let item = serde_json::to_value(&queue.list(&full).unwrap().tasks[0]).unwrap();
    let mut expected = serde_json::to_value(&task).unwrap();
    expected["dependencies"] = serde_json::json!([]);
    expected["goal_dependencies"] = serde_json::json!([]);
    expected["latest_run"] = serde_json::Value::Null;
    assert_eq!(item, expected);
}

#[test]
fn list_pages_by_limit_with_next_as_the_following_before() {
    let (_dir, mut queue) = fixture();
    let ids: Vec<TaskId> = (0..21)
        .map(|n| queue.add(new_task(&format!("task {n}"))).unwrap().id())
        .collect();
    let newest_first: Vec<TaskId> = ids.iter().rev().copied().collect();

    // limit + 1 tasks: the extra one is the next page.
    let first = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(first.total, 21);
    assert_eq!(first.tasks.len(), 20);
    assert_eq!(
        first.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        newest_first[..20]
    );
    assert_eq!(first.next, Some(ids[0]));
    let second = queue
        .list(&TaskQuery {
            before: first.next,
            ..TaskQuery::default()
        })
        .unwrap();
    assert_eq!(
        second.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    assert_eq!(second.next, None);
    assert_eq!(second.total, 21, "total ignores the page");

    // Exactly limit tasks: no next page.
    queue.transition(ids[0], TaskAction::Cancel).unwrap();
    let exact = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(exact.tasks.len(), 20);
    assert_eq!(exact.next, None);
    assert_eq!(exact.total, 20);

    let small = TaskQuery {
        limit: 3,
        before: Some(ids[10]),
        ..TaskQuery::default()
    };
    let page = queue.list(&small).unwrap();
    assert_eq!(
        page.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[10], ids[9], ids[8]]
    );
    assert_eq!(page.next, Some(ids[7]));
    let zero = TaskQuery {
        limit: 0,
        ..TaskQuery::default()
    };
    assert!(queue.list(&zero).is_err());
}

/// A clock stopped at one instant.
struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn system_time(&self) -> SystemTime {
        self.0
    }
}

/// IDs handed out in order.
struct FixedIds(Mutex<Vec<&'static str>>);

impl IdGenerator for FixedIds {
    fn uuid(&self) -> String {
        self.0.lock().unwrap().remove(0).to_owned()
    }
}

#[test]
fn claim_takes_the_run_id_and_its_time_from_the_injected_generators() {
    const RUN: &str = "22222222-2222-4222-8222-222222222222";
    let (_dir, queue) = fixture();
    let at = UNIX_EPOCH + Duration::from_millis(1_709_164_800_042);
    let mut queue = queue.with_generators(Generators {
        clock: Arc::new(FixedClock(at)),
        ids: Arc::new(FixedIds(Mutex::new(vec![RUN]))),
    });
    let task = queue.add(new_task("fixed")).unwrap();
    assert_eq!(task.created_at(), "2024-02-29T00:00:00.042Z");
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    assert_eq!(run.id(), &RunId::new(RUN).unwrap());
    assert_eq!(run.created_at(), "2024-02-29T00:00:00.042Z");
    let task = queue.show(task.id()).unwrap().task;
    assert_eq!(task.updated_at(), "2024-02-29T00:00:00.042Z");
    let goal = queue.add_goal(new_goal("fixed goal")).unwrap();
    let goal = queue.close_goal(goal.id(), GoalVerdict::Abandoned).unwrap();
    assert_eq!(goal.closed_at(), Some("2024-02-29T00:00:00.042Z"));
}

#[test]
fn injected_timestamps_have_the_form_sqlite_gives_its_timestamp_columns() {
    let conn = Connection::open_in_memory().unwrap();
    for secs in [
        0_i64,
        951_782_400,
        1_709_164_800,
        1_900_000_000,
        4_102_444_799,
    ] {
        let sqlite: String = conn
            .query_row(
                "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', ?1, 'unixepoch')",
                [secs],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_secs(secs as u64)),
            sqlite
        );
    }
}

fn person(workspace: &str) -> PlannerOwner {
    PlannerOwner {
        origin: PlannerOrigin::Person,
        workspace_id: Some(workspace.into()),
    }
}

fn submission(tasks: &[TaskId], goals: &[GoalId], proposal: Option<i64>) -> Submission {
    Submission {
        tasks: tasks.to_vec(),
        goals: goals.to_vec(),
        proposal: proposal.map(ProposalId::new),
        owner: person("W1"),
    }
}

fn status_of(queue: &mut SqliteQueue, id: TaskId) -> TaskStatus {
    queue.show(id).unwrap().task.status()
}

#[test]
fn submitted_tasks_wait_for_plan_review_which_readies_or_sends_them_back() {
    let (_dir, mut queue) = fixture();
    let goal = queue
        .add_goal(NewGoal {
            draft: true,
            ..new_goal("planned")
        })
        .unwrap()
        .id();
    let mut in_goal = new_task("in goal");
    in_goal.goal_id = Some(goal);
    let a = queue.add(in_goal).unwrap().id();
    let b = queue.add(new_task("alone")).unwrap().id();
    let later = queue.add(new_task("joins later")).unwrap().id();

    let proposal = queue.submit(submission(&[b], &[goal], None)).unwrap();
    assert_eq!(proposal.id(), ProposalId::new(1));
    assert_eq!(proposal.status(), ProposalStatus::Submitted);
    assert_eq!(proposal.task_ids(), [a, b]);
    assert_eq!(proposal.goal_ids(), [goal]);
    assert_eq!(proposal.owner(), &person("W1"));
    for id in [a, b] {
        assert_eq!(status_of(&mut queue, id), TaskStatus::Submitted);
    }
    assert_eq!(status_of(&mut queue, later), TaskStatus::Draft);
    let events = queue.show(a).unwrap().events;
    assert!(
        events
            .iter()
            .any(|e| e.kind == "task_submitted"
                && e.payload == serde_json::json!({"proposal_id": 1}))
    );
    assert!(
        queue
            .show_goal(goal)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "goal_submitted")
    );
    assert_eq!(
        queue.list_goals().unwrap()[0].tasks.submitted,
        1,
        "the goal counts its submitted task"
    );

    // Nothing claims or lists a submitted task as a candidate, but the graph
    // and the open list still show it.
    assert!(queue.candidates().unwrap().is_empty());
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    let graph = dependency_graph(queue.graph_input().unwrap(), None);
    let graph = serde_json::to_value(graph).unwrap();
    assert!(
        graph["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == a.as_i64() && t["status"] == "submitted")
    );
    let open = queue
        .list(&TaskQuery {
            status: StatusFilter::Open,
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(open.total, 3);

    // A plain ready is refused; the membership is exclusive while active.
    assert_eq!(
        queue
            .transition(a, TaskAction::Ready)
            .unwrap_err()
            .to_string(),
        "a submitted task becomes ready through plan review (submit it); \
         pass --bypass-review to skip the review"
    );
    assert_eq!(
        queue
            .transition(later, TaskAction::Ready)
            .unwrap_err()
            .to_string(),
        "a draft task becomes ready through plan review (submit it); \
         pass --bypass-review to skip the review"
    );
    queue.transition(b, TaskAction::Draft).unwrap();
    assert_eq!(
        queue
            .submit(submission(&[b], &[], None))
            .unwrap_err()
            .to_string(),
        format!("task {b} already belongs to proposal 1")
    );
    assert_eq!(
        queue
            .submit(submission(&[later], &[goal], None))
            .unwrap_err()
            .to_string(),
        format!("goal {goal} already belongs to proposal 1")
    );
    assert_eq!(
        queue
            .submit(submission(&[later], &[], Some(1)))
            .unwrap_err()
            .to_string(),
        "proposal 1 is submitted, not revising"
    );
    // A submitted task keeps its content editable.
    queue
        .edit_task(
            a,
            TaskEdit {
                acceptance: Some("sharper".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();

    // Plan review sends it back: the submitted task returns to draft, and the
    // planner submits it again with the drafts it holds and a new one.
    let revising = queue.send_back_proposal(ProposalId::new(1)).unwrap();
    assert_eq!(revising.status(), ProposalStatus::Revising);
    assert_eq!(revising.revise_count(), 1);
    assert_eq!(status_of(&mut queue, a), TaskStatus::Draft);
    assert_eq!(queue.proposals(false).unwrap().len(), 1);
    let again = queue
        .submit(Submission {
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
            ..submission(&[later], &[], Some(1))
        })
        .unwrap();
    assert_eq!(again.id(), ProposalId::new(1));
    assert_eq!(again.status(), ProposalStatus::Submitted);
    assert_eq!(again.task_ids(), [a, b, later]);
    assert_eq!(again.owner().origin, PlannerOrigin::Runtime);
    for id in [a, b, later] {
        assert_eq!(status_of(&mut queue, id), TaskStatus::Submitted);
    }

    // The plan-review path: every submitted task becomes ready and the draft
    // goal opens.
    let accepted = queue.approve_proposal(ProposalId::new(1)).unwrap();
    assert_eq!(accepted.status(), ProposalStatus::Accepted);
    for id in [a, b, later] {
        assert_eq!(status_of(&mut queue, id), TaskStatus::Ready);
    }
    assert!(!queue.show_goal(goal).unwrap().goal.is_draft());
    assert_eq!(queue.candidates().unwrap().len(), 3);
    assert_eq!(
        queue
            .approve_proposal(ProposalId::new(1))
            .unwrap_err()
            .to_string(),
        "proposal 1 is accepted, not submitted"
    );
    assert!(queue.send_back_proposal(ProposalId::new(1)).is_err());
    assert!(queue.proposals(false).unwrap().is_empty());
    assert_eq!(queue.proposals(true).unwrap().len(), 1);
    assert_eq!(
        queue
            .show_proposal(ProposalId::new(1))
            .unwrap()
            .task_ids()
            .len(),
        3
    );
    assert!(queue.show_proposal(ProposalId::new(9)).is_err());

    // An accepted proposal no longer holds its members; a person may bypass
    // plan review, which is recorded.
    queue.transition(a, TaskAction::Draft).unwrap();
    let second = queue.submit(submission(&[a], &[], None)).unwrap();
    assert_eq!(second.id(), ProposalId::new(2));
    let bypassed = queue.transition(a, TaskAction::BypassReview).unwrap();
    assert_eq!(bypassed.status(), TaskStatus::Ready);
    let events = queue.show(a).unwrap().events;
    assert!(
        events.iter().any(|e| e.kind == "review_bypassed"
            && e.payload == serde_json::json!({"from": "submitted"}))
    );
    // Approving the proposal leaves the bypassed task as it is.
    queue.approve_proposal(ProposalId::new(2)).unwrap();
    assert_eq!(status_of(&mut queue, a), TaskStatus::Ready);
}

#[test]
fn submit_needs_a_draft_task_and_an_open_goal() {
    let (_dir, mut queue) = fixture();
    let empty = queue.add_goal(new_goal("nothing yet")).unwrap().id();
    assert_eq!(
        queue
            .submit(submission(&[], &[empty], None))
            .unwrap_err()
            .to_string(),
        "a proposal needs at least one draft task"
    );
    queue.close_goal(empty, GoalVerdict::Abandoned).unwrap();
    assert!(
        queue
            .submit(submission(&[], &[empty], None))
            .unwrap_err()
            .to_string()
            .starts_with(&format!("goal {empty} is closed"))
    );
    let task = queue.add(new_task("ready")).unwrap().id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    assert_eq!(
        queue
            .submit(submission(&[task], &[], None))
            .unwrap_err()
            .to_string(),
        "cannot apply Submit to task in ready state"
    );
    assert!(
        queue
            .submit(submission(&[TaskId::new(99)], &[], None))
            .is_err()
    );
    assert!(queue.submit(submission(&[task], &[], Some(5))).is_err());
    assert_eq!(
        queue
            .submit(submission(&[TaskId::new(0)], &[], None))
            .unwrap_err()
            .to_string(),
        "task ID must be positive"
    );
    // A failed submit leaves nothing behind.
    assert!(queue.proposals(true).unwrap().is_empty());
}

#[test]
fn migration_to_v21_keeps_drafts_and_the_task_id_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v20.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_queue.sql"),
        include_str!("../migrations/0002_supervisor.sql"),
        include_str!("../migrations/0003_workspace_close.sql"),
        include_str!("../migrations/0004_integration.sql"),
        include_str!("../migrations/0005_run_leases.sql"),
        include_str!("../migrations/0006_merge_queue.sql"),
        include_str!("../migrations/0007_supervisors.sql"),
        include_str!("../migrations/0008_goals.sql"),
        include_str!("../migrations/0009_supervisor_mode.sql"),
        include_str!("../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../migrations/0011_session_workspaces.sql"),
        include_str!("../migrations/0012_queue_events.sql"),
        include_str!("../migrations/0013_goal_draft.sql"),
        include_str!("../migrations/0014_asks.sql"),
        include_str!("../migrations/0015_task_required_evidence.sql"),
        include_str!("../migrations/0016_observer.sql"),
        include_str!("../migrations/0017_stuck_exit_ask.sql"),
        include_str!("../migrations/0018_task_paths.sql"),
        include_str!("../migrations/0019_task_goal_dependencies.sql"),
        include_str!("../migrations/0020_task_priority.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 20).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO goals(title) VALUES ('existing');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id,
                           context,required_evidence,paths,priority)
         VALUES ('kept draft','d','a','[\"cargo test\"]','draft',1,'c','[\"e2e\"]','[\"src/**\"]',3);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('landed','','','[]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-landed',2,'integrated','claude','claude','{BASE}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('waiting','','','[]','ready');
         INSERT INTO task_dependencies(task_id,predecessor_id) VALUES (3,2);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('gone','','','[]','draft');
         DELETE FROM tasks WHERE id=4;"
    ))
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    let kept = queue.show(TaskId::new(1)).unwrap().task;
    assert_eq!(kept.status(), TaskStatus::Draft);
    assert_eq!(
        (kept.context(), kept.paths(), kept.priority()),
        ("c", &["src/**".to_owned()][..], Priority::Urgent)
    );
    assert_eq!(kept.required_evidence(), [EvidenceCheck::E2e]);
    assert_eq!(kept.goal_id(), Some(GoalId::new(1)));
    assert_eq!(
        queue.show(TaskId::new(3)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert_eq!(queue.candidates().unwrap().len(), 1);
    assert!(queue.proposals(true).unwrap().is_empty());
    // The deleted task's ID is never handed out again.
    assert_eq!(queue.add(new_task("new")).unwrap().id(), TaskId::new(5));
    let raw = Connection::open(&path).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
             VALUES ('x','','','[]','bogus')",
            []
        )
        .is_err()
    );
    let violations: i64 = raw
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

fn search(queue: &SqliteQueue, terms: &str, adjust: impl FnOnce(&mut SearchQuery)) -> Vec<String> {
    let mut query = SearchQuery {
        terms: terms.into(),
        limit: 20,
        ..SearchQuery::default()
    };
    adjust(&mut query);
    let page = queue.search(&query).unwrap();
    assert_eq!(page.total, page.hits.len());
    page.hits
        .iter()
        .map(|hit| {
            let id = match &hit.id {
                SearchRef::Id(id) => id.to_string(),
                SearchRef::Commit(sha) => sha.clone(),
            };
            format!(
                "{} {id} {} {}: {}",
                hit.kind.as_str(),
                hit.status.as_deref().unwrap_or("-"),
                hit.field,
                hit.excerpt
            )
        })
        .collect()
}

#[test]
fn search_finds_every_kind_in_every_status_and_follows_edits() {
    let (dir, mut queue) = fixture();
    let goal = queue
        .add_goal(NewGoal {
            constraints: "埋め込みは入れない".into(),
            ..new_goal("重複を見つける")
        })
        .unwrap()
        .id();
    let task = queue
        .add(NewTask {
            description: "FTS5 の trigram で src/infrastructure/search.rs を足す".into(),
            goal_id: Some(goal),
            ..new_task("runtime: 全文検索を入れる")
        })
        .unwrap()
        .id();
    let other = queue
        .add(new_task("Find duplicate tasks by their test names"))
        .unwrap()
        .id();
    queue.transition(other, TaskAction::Cancel).unwrap();
    for (target, text) in [
        (NoteTarget::Task(task), "ID の衝突に注意する"),
        (NoteTarget::Goal(goal), "goal の観察メモ"),
    ] {
        queue
            .add_note(NewNote {
                target,
                text: text.into(),
                kind: None,
                by: "human".into(),
            })
            .unwrap();
    }
    assert_eq!(
        search(&queue, "全文検索", |_| {}),
        ["task 1 draft title: runtime: «全文検索»を入れる"]
    );
    assert_eq!(
        search(&queue, "埋め込み", |_| {}),
        ["goal 1 open constraints: «埋め込み»は入れない"]
    );
    assert_eq!(
        search(&queue, "search.rs", |_| {}),
        ["task 1 draft description: …am で src/infrastructure/«search.rs» を足す"]
    );
    // A canceled task is found, and --status keeps to the statuses given.
    assert_eq!(
        search(&queue, "DUPLICATE", |_| {}),
        ["task 2 canceled title: Find «duplicate» tasks by their test nam…"]
    );
    assert!(search(&queue, "duplicate", |q| q.statuses = vec!["draft".into()]).is_empty());
    // Terms under three characters are matched too, all of them.
    assert_eq!(
        search(&queue, "ID", |_| {}),
        ["note 5 draft text: «ID» の衝突に注意する"]
    );
    assert_eq!(
        search(&queue, "観察 goal", |_| {}),
        ["note 6 open text: «goal» の観察メモ"]
    );
    assert!(search(&queue, "ID 観察", |_| {}).is_empty());
    assert_eq!(
        search(&queue, "trigram OR 観察メモ", |q| q.kinds =
            vec![SearchKind::Note])
        .len(),
        1
    );
    assert_eq!(
        search(&queue, "を入れる OR 見つける OR 観察メモ", |_| {}).len(),
        3
    );
    assert_eq!(
        search(
            &queue,
            "を入れる OR 見つける OR 観察メモ OR 注意する OR test",
            |q| { q.goal_id = Some(goal) }
        )
        .len(),
        4
    );
    assert!(
        queue
            .search(&SearchQuery {
                terms: "ID OR x".into(),
                limit: 1,
                ..SearchQuery::default()
            })
            .unwrap_err()
            .to_string()
            .contains("cannot be combined")
    );

    // Edits replace what is indexed; the new status reaches the notes.
    queue
        .edit_task(
            task,
            TaskEdit {
                description: Some("SQLite の索引を使う".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
    assert!(search(&queue, "trigram", |_| {}).is_empty());
    assert_eq!(search(&queue, "sqlite", |_| {}).len(), 1);
    queue
        .edit_goal(
            goal,
            GoalEdit {
                constraints: Some("意味の検索は後回し".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    assert!(search(&queue, "埋め込み", |_| {}).is_empty());
    assert_eq!(search(&queue, "後回し", |_| {}).len(), 1);
    queue.transition(task, TaskAction::Cancel).unwrap();
    queue.close_goal(goal, GoalVerdict::Abandoned).unwrap();
    let mut closed = search(&queue, "注意する OR 観察メモ OR 後回し", |_| {});
    closed.sort();
    assert_eq!(
        closed,
        [
            "goal 1 abandoned constraints: 意味の検索は«後回し»",
            "note 5 canceled text: ID の衝突に«注意する»",
            "note 6 abandoned text: goal の«観察メモ»",
        ]
    );

    // --full adds every field and the score.
    let full = queue
        .search(&SearchQuery {
            terms: "後回し".into(),
            limit: 5,
            full: true,
            ..SearchQuery::default()
        })
        .unwrap();
    let hit = &full.hits[0];
    assert!(hit.score.is_some());
    assert_eq!(
        hit.fields.as_ref().unwrap()["constraints"],
        "意味の検索は後回し"
    );

    // What an older binary writes is indexed by the triggers alike.
    drop(queue);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "INSERT INTO tasks(title,description,acceptance,verification_commands)
         VALUES ('older binary の登録','','','[]')",
        [],
    )
    .unwrap();
    let queue = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    assert_eq!(
        search(&queue, "older", |_| {}),
        ["task 3 draft title: «older» binary の登録"]
    );
}

#[test]
fn migration_indexes_the_existing_rows_and_landings_record_their_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..25] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 25;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 25);
         INSERT INTO goals(title, constraints, updated_at)
         VALUES ('古い goal', '互換を宣言する', '2026-09-01T00:00:00.000Z');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id,
                           updated_at)
         VALUES ('古い task','着地済みの変更','','[]','completed',1,'2026-09-02T00:00:00.000Z'),
                ('second','','','[]','completed',NULL,'2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'integrated','claude','claude','{BASE}'),
                ('run-2',2,'integrated','claude','claude','{BASE}');
         INSERT INTO run_events(task_id,goal_id,run_id,kind,payload) VALUES
           (1,NULL,'run-1','observation','{{\"text\":\"古いメモ\",\"kind\":\"note\",\"by\":\"human\"}}'),
           (1,NULL,'run-1','run_integrated',
            '{{\"result_commit\":\"aaaa\",\"message\":\"docs: 古い task\\n\\nsummary を書いた\",\"git_common_dir\":\"/repo/.git\"}}'),
           (2,NULL,'run-2','run_integrated','{{\"result_commit\":\"bbbb\"}}');"
    ))
    .unwrap();
    let report = SqliteQueue::migrate(&path, None, 0).unwrap();
    assert_eq!(
        report
            .applied
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        [(26, true), (27, false)]
    );
    // 0027 (plan review) is applied with it and is breaking: a copy is
    // taken and the floor rises to it.
    assert!(report.backup.is_some());
    assert_eq!(report.floor, 27);
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(
        search(&queue, "古い", |_| {}),
        [
            "commit aaaa completed message: docs: «古い» task\n\nsummary を書いた",
            "note 1 completed text: «古い»メモ",
            "task 1 completed title: «古い» task",
            "goal 1 open title: «古い» goal",
        ]
    );
    let page = queue
        .search(&SearchQuery {
            terms: "summary".into(),
            kinds: vec![SearchKind::Commit],
            goal_id: Some(GoalId::new(1)),
            limit: 5,
            ..SearchQuery::default()
        })
        .unwrap();
    let hit = &page.hits[0];
    assert_eq!(
        (hit.task_id, hit.run_id.as_deref(), hit.title.as_str()),
        (Some(1), Some("run-1"), "docs: 古い task")
    );
    // The landing recorded without its message is filled from Git.
    let seen = Mutex::new(Vec::new());
    let filled = queue
        .fill_commit_messages(|dir, commit| {
            seen.lock()
                .unwrap()
                .push((dir.map(str::to_owned), commit.to_owned()));
            Some("fix: 読み直した message".into())
        })
        .unwrap();
    assert_eq!(filled, 1);
    assert_eq!(seen.into_inner().unwrap(), [(None, "bbbb".to_owned())]);
    assert_eq!(
        search(&queue, "読み直した", |_| {}),
        ["commit bbbb completed message: fix: «読み直した» message"]
    );
    assert_eq!(queue.fill_commit_messages(|_, _| None).unwrap(), 0);

    // A later landing is recorded with the event that records it.
    drop(queue);
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('third','','','[]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-3',3,'integrated','claude','claude','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES
           (3,'run-3','run_integrated','{{\"result_commit\":\"cccc\",\"message\":\"feat: 新しい着地\"}}');"
    ))
    .unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(
        search(&queue, "新しい着地", |_| {}),
        ["commit cccc completed message: feat: «新しい着地»"]
    );
}
