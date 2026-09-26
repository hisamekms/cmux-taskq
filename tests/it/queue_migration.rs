//! Queue tests: what each migration does to the rows of an older queue.
use crate::common;

use std::sync::Mutex;

use dagq::{
    VERSION,
    application::TaskStore,
    domain::search::{SearchKind, SearchQuery},
    domain::{
        EventId, EvidenceCheck, GoalId, GoalStatus, Priority, RunId, RunStatus, SupervisorMode,
        TaskAction, TaskId, TaskKind, TaskStatus,
    },
    infrastructure::{
        schema::{MIGRATIONS, floor_for},
        sqlite::SqliteQueue,
    },
};
use rusqlite::Connection;

use common::queue::*;

#[test]
fn migration_to_v5_rebuilds_runs_moves_the_lease_and_keeps_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(include_str!("../../migrations/0001_queue.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../../migrations/0002_supervisor.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../../migrations/0003_workspace_close.sql"))
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
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
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
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
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
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
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
    // Every migration from 0007 (supervisors) on is applied together, up to
    // the last one the binary lists (ADR-0067 decision 1).
    assert_eq!(SqliteQueue::SCHEMA_VERSION, MIGRATIONS.len() as i64);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
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
    // Which events may belong to neither a task nor a goal is the write
    // port's rule since the kinds were opened (ADR-0073 decision 22).
    raw.execute(
        "INSERT INTO run_events(kind,payload) VALUES ('orphan','{}')",
        [],
    )
    .unwrap();
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
    // Which asks may belong to no task is the write port's rule too.
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category)
         VALUES ('blocked','slots idle','observer','scope')",
        [],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category)
         VALUES ('decide','x','observer','recovery_failed')",
        [],
    )
    .unwrap();
    // 0029: every ask has a reason; that authentication and cost are the
    // queue_hold asks' alone is the write port's rule now.
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category,affected)
         VALUES ('queue_hold','log in','supervisor','authentication','[\"run-landed\"]')",
        [],
    )
    .unwrap();
    for (kind, reason, accepted) in [
        ("queue_hold", "scope", true),
        ("blocked", "cost", true),
        ("blocked", "bogus", false),
    ] {
        assert_eq!(
            raw.execute(
                "INSERT INTO asks(kind,question,asked_by,reason_category) VALUES (?1,'x','observer',?2)",
                [kind, reason]
            )
            .is_ok(),
            accepted,
            "{kind} {reason}"
        );
    }
    assert!(
        raw.execute(
            "INSERT INTO asks(kind,question,asked_by) VALUES ('blocked','x','observer')",
            []
        )
        .is_err()
    );
    // 0017: the supervisor's stuck_exit ask about a run.
    raw.execute(
        "INSERT INTO asks(kind,task_id,run_id,question,asked_by,reason_category)
         VALUES ('stuck_exit',1,'run-landed','session did not exit','supervisor','recovery_failed')",
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
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
        include_str!("../../migrations/0007_supervisors.sql"),
        include_str!("../../migrations/0008_goals.sql"),
        include_str!("../../migrations/0009_supervisor_mode.sql"),
        include_str!("../../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../../migrations/0011_session_workspaces.sql"),
        include_str!("../../migrations/0012_queue_events.sql"),
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
fn migration_to_v21_keeps_drafts_and_the_task_id_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v20.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
        include_str!("../../migrations/0007_supervisors.sql"),
        include_str!("../../migrations/0008_goals.sql"),
        include_str!("../../migrations/0009_supervisor_mode.sql"),
        include_str!("../../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../../migrations/0011_session_workspaces.sql"),
        include_str!("../../migrations/0012_queue_events.sql"),
        include_str!("../../migrations/0013_goal_draft.sql"),
        include_str!("../../migrations/0014_asks.sql"),
        include_str!("../../migrations/0015_task_required_evidence.sql"),
        include_str!("../../migrations/0016_observer.sql"),
        include_str!("../../migrations/0017_stuck_exit_ask.sql"),
        include_str!("../../migrations/0018_task_paths.sql"),
        include_str!("../../migrations/0019_task_goal_dependencies.sql"),
        include_str!("../../migrations/0020_task_priority.sql"),
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

#[test]
fn migration_to_v28_gives_the_follow_up_drafts_already_queued_their_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..27] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 27;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 27);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('source','','a','[]','completed','2026-09-02T00:00:00.000Z'),
                ('follow','later','','[]','draft','2026-09-02T00:00:00.000Z'),
                ('mine','','','[]','draft','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'integrated','claude','claude','{BASE}');
         INSERT INTO task_leases(task_id,supervisor_token,reason,heartbeat_at)
         VALUES (2,'old','follow_up_triage',1);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES
           (1,'run-1','follow_up_registered','{{\"task_id\":2,\"title\":\"follow\",\"index\":0}}'),
           (1,'run-1','follow_up_registered','{{\"task_id\":null,\"index\":1,\"skipped\":\"x\"}}');"
    ))
    .unwrap();
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    // The follow_up draft from before gets a planner like a new one; the
    // person's draft does not.
    let (origin, material) = queue.draft_origin(TaskId::new(2)).unwrap().unwrap();
    assert_eq!(origin, dagq::domain::DraftOrigin::FollowUp);
    assert_eq!(
        material,
        serde_json::json!({"source_task_id": 1, "source_run_id": "run-1", "index": 0})
    );
    assert!(queue.draft_origin(TaskId::new(3)).unwrap().is_none());
    let targets: Vec<TaskId> = queue
        .planner_drafts()
        .unwrap()
        .iter()
        .map(|t| t.task.id())
        .collect();
    assert_eq!(targets, [TaskId::new(2)]);
    // The follow-up triage's leases are gone with it.
    let leases: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='task_leases'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leases, 0);
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
        pending_after(25)
    );
    assert_eq!(
        report.applied[..6]
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        [
            (26, true),
            (27, false),
            (28, false),
            (29, false),
            (30, false),
            (31, true)
        ]
    );
    // 0027 (plan review), 0028 (draft planners), 0029 (ask reasons) and 0030
    // (findings) are applied with it and are breaking: a copy is taken and the
    // floor rises to the last breaking one. 0031 (the supervisor handoff) is
    // compatible.
    assert!(report.backup.is_some());
    assert_eq!(report.floor, floor_for(SqliteQueue::SCHEMA_VERSION));
    assert!(report.floor >= 30);
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

#[test]
fn migration_to_v29_gives_every_ask_the_reason_of_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..28] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 28;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 28);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'running','claude','claude','{BASE}');
         INSERT INTO asks(kind,task_id,run_id,question,asked_by) VALUES
           ('approve_landing',1,'run-1','land?','supervisor'),
           ('worker_question',1,'run-1','which?','worker'),
           ('stalled',1,'run-1','idle','supervisor'),
           ('decide',1,NULL,'retry?','supervisor');
         INSERT INTO asks(kind,question,asked_by) VALUES ('blocked','slots idle','observer');"
    ))
    .unwrap();
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    let reasons: Vec<(String, String)> = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .map(|a| {
            (
                a.kind.as_str().to_owned(),
                a.reason_category.as_str().to_owned(),
            )
        })
        .collect();
    let pairs: Vec<(&str, &str)> = reasons
        .iter()
        .map(|(k, r)| (k.as_str(), r.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("approve_landing", "scope"),
            ("worker_question", "scope"),
            ("stalled", "recovery_failed"),
            ("decide", "recovery_failed"),
            ("blocked", "scope"),
        ]
    );
}

/// Goal 21: the task kind is an addition. The tasks of an older queue have
/// none after the migration, which raises no floor, and an older binary's
/// insert that does not name the column leaves it null too.
#[test]
fn migration_adding_the_task_kind_keeps_older_tasks_without_one() {
    // Found by its statement, not its number, which a landing may change.
    let at = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("ALTER TABLE tasks ADD COLUMN kind"))
        .unwrap();
    let before = i64::try_from(at).unwrap();
    assert_eq!(floor_for(before + 1), floor_for(before));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..at] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('runtime: older','','','[]','draft');",
        floor = floor_for(before),
    ))
    .unwrap();
    drop(raw);
    // A compatible step: `migrated` expects the backup of a breaking one.
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().task.kind(), None);
    let mut kinded = new_task("docs");
    kinded.kind = Some(TaskKind::Docs);
    let added = queue.add(kinded).unwrap();
    assert_eq!(added.kind(), Some(TaskKind::Docs));
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
             VALUES ('older binary','','','[]','draft')",
            [],
        )
        .unwrap();
    assert_eq!(queue.show(TaskId::new(3)).unwrap().task.kind(), None);
    // A kind a newer binary added reads as none; the task still restores.
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE tasks SET kind='later' WHERE id=3", [])
        .unwrap();
    assert_eq!(queue.show(TaskId::new(3)).unwrap().task.kind(), None);
    assert_eq!(queue.list(&Default::default()).unwrap().total, 3);
}

/// Task 325: the answerer and the chosen option of an ask are additions.
/// An ask answered before the migration reads with both null (unknown),
/// shown as null in its JSON, and an older binary's answer that names
/// neither leaves them null too.
#[test]
fn migration_adding_the_answerer_keeps_older_answers_unknown() {
    let at = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("ALTER TABLE asks ADD COLUMN answered_by"))
        .unwrap();
    let before = i64::try_from(at).unwrap();
    assert_eq!(floor_for(before + 1), floor_for(before));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..at] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('t','','','[]','draft');
         INSERT INTO asks(kind,task_id,question,options,asked_by,reason_category,answer,answered_at)
         VALUES ('decide',1,'retry?','[\"retry\",\"cancel\"]','supervisor','recovery_failed','retry',1),
                ('decide',1,'again?','[\"retry\"]','supervisor','recovery_failed',NULL,NULL);",
        floor = floor_for(before),
    ))
    .unwrap();
    drop(raw);
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    let older = queue.read_ask(dagq::domain::AskId::new(1)).unwrap();
    assert_eq!(older.answer.as_deref(), Some("retry"));
    assert_eq!((older.answered_by, older.option_index), (None, None));
    let json = serde_json::to_value(queue.read_ask(dagq::domain::AskId::new(1)).unwrap()).unwrap();
    assert!(
        json["answered_by"].is_null() && json["option_index"].is_null(),
        "{json}"
    );
    // An older binary answers without naming the columns.
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE asks SET answer='retry', answered_at=2 WHERE id=2",
            [],
        )
        .unwrap();
    let answered = queue.read_ask(dagq::domain::AskId::new(2)).unwrap();
    assert_eq!((answered.answered_by, answered.option_index), (None, None));
    // This binary records both.
    let asked = queue
        .ask(dagq::domain::NewAsk {
            kind: dagq::domain::AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which?".into(),
            options: vec!["retry".into(), "cancel".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    let answered = queue.answer_as(asked.id, " cancel ", "inbox").unwrap();
    assert_eq!(answered.answered_by.as_deref(), Some("inbox"));
    assert_eq!(answered.option_index, Some(1));
}

#[test]
fn migration_opening_the_kinds_keeps_rows_and_moves_their_rules_to_the_write_port() {
    use dagq::domain::{AskKind, AskReason, NewAsk};
    // ADR-0073 decisions 19-23, found by what it creates so a renumbering
    // on landing does not move it.
    let open = MIGRATIONS
        .iter()
        .position(|m| m.contains("CREATE TABLE asks_v39"))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..open] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {open};
         UPDATE schema_floor SET floor = {floor};
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'running','claude','claude','{BASE}');
         INSERT INTO asks(id,kind,task_id,run_id,question,asked_by,reason_category,
                          answer,answered_at,answered_by,option_index) VALUES
           (7,'worker_question',1,'run-1','which?','worker','scope','a',5,'inbox',0);
         INSERT INTO asks(id,kind,question,asked_by,reason_category,subject,affected) VALUES
           (9,'queue_hold','log in','supervisor','authentication','login','[\"run-1\"]');
         INSERT INTO run_events(id,kind,payload) VALUES (40,'mark_recorded','{{}}');
         INSERT INTO run_events(id,task_id,run_id,kind,payload)
         VALUES (41,1,'run-1','agent_started','{{}}');",
        floor = floor_for(open as i64),
    ))
    .unwrap();
    // Before it, the queue refuses a kind it does not list.
    assert!(
        raw.execute(
            "INSERT INTO run_events(kind,payload) VALUES ('later','{}')",
            []
        )
        .is_err()
    );
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    let kinds: Vec<(i64, String)> = raw
        .prepare("SELECT id, kind FROM asks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        kinds,
        [(7, "worker_question".into()), (9, "queue_hold".into())]
    );
    let answered = queue.read_ask(dagq::domain::AskId::new(7)).unwrap();
    assert_eq!(
        (answered.answered_by.as_deref(), answered.option_index),
        (Some("inbox"), Some(0))
    );
    let events: Vec<i64> = raw
        .prepare("SELECT id FROM run_events WHERE id IN (40, 41) ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(events, [40, 41]);
    let checks: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('asks','run_events')
             AND (sql LIKE '%kind IN%' OR sql LIKE '%kind =%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(checks, 0);
    let one_open: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'asks_open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(one_open, 1);

    // The queue takes a kind a newer binary writes, and this one reads it.
    raw.execute_batch(
        "INSERT INTO asks(kind,task_id,question,asked_by,reason_category)
         VALUES ('later_kind',1,'what now?','supervisor','scope');
         INSERT INTO run_events(kind,payload) VALUES ('later_event','{}');",
    )
    .unwrap();
    let later = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|a| a.question == "what now?")
        .unwrap();
    assert_eq!(later.kind, AskKind::Other("later_kind".into()));
    assert!(!later.kind.is_known());

    // The write port keeps the rules the CHECKs held.
    let error = queue
        .record_queue_event("task_created", serde_json::json!({}))
        .unwrap_err();
    assert!(error.to_string().contains("needs a task, a goal or a run"));
    queue
        .record_queue_event("mark_recorded", serde_json::json!({}))
        .unwrap();
    let ask = |kind: AskKind, task: Option<i64>, run: Option<&str>| NewAsk {
        kind,
        task_id: task.map(TaskId::new),
        run_id: run.map(|r| RunId::try_from(r).unwrap()),
        question: "which?".into(),
        options: vec![],
        asked_by: "supervisor".into(),
        reason_category: AskReason::Scope,
        finding_id: None,
    };
    let refused = [
        ask(AskKind::Other("later_kind".into()), Some(1), None),
        ask(AskKind::QueueHold, None, None),
        ask(AskKind::Decide, None, None),
    ];
    for new in refused {
        assert!(queue.ask(new.clone()).is_err(), "{:?}", new.kind);
    }
    assert!(
        queue
            .ask(ask(AskKind::Blocked, None, None))
            .unwrap()
            .created
    );
    assert!(
        queue
            .ask(ask(AskKind::Decide, Some(1), None))
            .unwrap()
            .created
    );
}
