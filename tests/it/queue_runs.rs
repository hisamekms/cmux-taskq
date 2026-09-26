//! Queue tests: claims and runs, the database's guards on them, the injected
//! clock and IDs, and the asks that runs open.
use crate::common;

use std::{
    sync::{Arc, Barrier, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dagq::{
    application::{Clock, Generators, IdGenerator, TaskStore, timestamp},
    domain::{
        ClaimOutcome, CommitSha, EventId, GoalVerdict, RunId, TaskAction, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

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
    let _waiting = common::within(common::STEP_LIMIT, "the claiming threads to return");
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

/// Authentication and cost asks are one per queue, reason and subject
/// (ADR-0047 decision 42): a second run that hits the same login joins the
/// open ask, which lists both runs and records `ask_updated` on the one that
/// joined; the same run joining again changes nothing; another subject or an
/// answered ask opens a new one.
#[test]
fn a_login_that_stops_several_runs_is_one_ask_that_lists_them() {
    use dagq::domain::{AskKind, AskReason, HOLD_OPTIONS, NewAsk, NewHold};
    let (_dir, mut queue) = fixture();
    let mut runs = Vec::new();
    for title in ["one", "two"] {
        let task = queue.add(new_task(title)).unwrap();
        queue
            .transition(task.id(), TaskAction::BypassReview)
            .unwrap();
        let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
            panic!()
        };
        runs.push(run.id().clone());
    }
    let hold = |run: &RunId| NewHold {
        reason_category: AskReason::Authentication,
        subject: None,
        run_id: Some(run.clone()),
        question: "Log in again.".into(),
        options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
        asked_by: "supervisor".into(),
    };
    let first = queue.hold(hold(&runs[0])).unwrap();
    assert!(first.created && first.joined);
    assert_eq!(first.ask.kind, AskKind::QueueHold);
    assert_eq!(first.ask.task_id, None);
    assert_eq!(first.ask.run_id, None);
    assert_eq!(first.ask.affected, [runs[0].as_str()]);
    let second = queue.hold(hold(&runs[1])).unwrap();
    assert!(!second.created && second.joined);
    assert_eq!(second.ask.id, first.ask.id);
    assert_eq!(second.ask.affected, [runs[0].as_str(), runs[1].as_str()]);
    assert_eq!(
        second.ask.question,
        format!("Log in again.\n\nAffected runs: {}, {}", runs[0], runs[1])
    );
    let again = queue.hold(hold(&runs[0])).unwrap();
    assert!(!again.created && !again.joined);
    assert_eq!(queue.hold_of(&runs[1]).unwrap().unwrap().id, first.ask.id);
    let open = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    // One ask_opened on the queue, one ask_updated on the run that joined.
    let events = queue
        .events_between(
            EventId::new(0),
            queue.latest_event_id().unwrap(),
            &dagq::domain::EventFilter::default(),
            100,
        )
        .unwrap();
    let opened: Vec<_> = events.iter().filter(|e| e.kind == "ask_opened").collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].payload["reason_category"], "authentication");
    assert_eq!(opened[0].task_id, None);
    let updated: Vec<_> = events.iter().filter(|e| e.kind == "ask_updated").collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].run_id.as_ref(), Some(&runs[1]));
    assert_eq!(updated[0].payload["affected"].as_array().unwrap().len(), 2);
    // A cost ask is another hold, and so is the usage limit's next to the disk's.
    let usage = queue
        .hold(NewHold {
            reason_category: AskReason::Cost,
            subject: Some("usage_limit".into()),
            ..hold(&runs[0])
        })
        .unwrap();
    assert!(usage.created);
    let disk = queue
        .hold(NewHold {
            reason_category: AskReason::Cost,
            subject: Some("disk".into()),
            ..hold(&runs[0])
        })
        .unwrap();
    assert!(disk.created && disk.ask.id != usage.ask.id);
    // Once answered, the next login that runs out opens a new ask.
    let answered = queue.answer(first.ask.id, "done").unwrap();
    assert_eq!(answered.reason_category, AskReason::Authentication);
    // `stats` counts the holds, on neither a task nor a run, by reason
    // (task 439): the login answered, the two cost holds still open.
    let latest = queue.latest_event_id().unwrap();
    let events = queue
        .events_between(
            EventId::new(0),
            latest,
            &dagq::domain::EventFilter::default(),
            100,
        )
        .unwrap();
    let asks = dagq::domain::stats::asks::asks(&events, EventId::new(0), latest, 0, |_| true);
    let reasons = &asks.by_reason_category;
    assert_eq!(
        (
            reasons["authentication"].opened,
            reasons["authentication"].answered,
            reasons["authentication"].open
        ),
        (1, 1, 0)
    );
    assert_eq!((reasons["cost"].opened, reasons["cost"].open), (2, 2));
    assert!(queue.hold_of(&runs[1]).unwrap().is_none());
    assert!(queue.hold(hold(&runs[1])).unwrap().created);
    // A hold is for authentication or cost only, and those are no other ask.
    assert!(
        queue
            .hold(NewHold {
                reason_category: AskReason::Scope,
                ..hold(&runs[0])
            })
            .is_err()
    );
    let error = queue
        .ask(NewAsk {
            kind: AskKind::WorkerQuestion,
            task_id: None,
            run_id: Some(runs[0].clone()),
            question: "logged out?".into(),
            options: Vec::new(),
            asked_by: "worker".into(),
            reason_category: AskReason::Authentication,
            finding_id: None,
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("queue_hold asks the runtime opens"),
        "{error}"
    );
}
