//! Queue tests: goals, set-goal, goal events and verdicts, notes and findings.
use crate::common;

use dagq::{
    application::{TaskQuery, TaskStore},
    domain::{
        ClaimOutcome, GoalEdit, GoalId, GoalStatus, GoalVerdict, NewGoal, NewNote, NewTask,
        NotePage, NoteQuery, NoteTarget, RunId, TaskAction, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

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

/// A finding on a run keeps the run's task and records its events there; a
/// finding on a goal records them on the goal; a missing target is refused
/// (ADR-0044 decision 18).
#[test]
fn findings_on_a_run_or_a_goal_ride_on_their_target() {
    use dagq::domain::{FindingQuery, FindingStatus, FindingTarget, NewFinding};
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("observed")).unwrap();
    let task = queue.add(new_task("slow")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    let finding = |target: FindingTarget| NewFinding {
        kind: "wait".into(),
        target,
        subject: String::new(),
        summary: "waits".into(),
        detail: Some("long".into()),
        impact: None,
        evidence: Vec::new(),
        propose: None,
        by: "observer".into(),
    };
    let on_run = queue
        .record_finding(finding(FindingTarget::Run(run.id().clone())))
        .unwrap();
    assert!(on_run.created);
    assert_eq!(
        (
            on_run.finding.task_id,
            on_run.finding.run_id.clone(),
            on_run.finding.goal_id
        ),
        (Some(task.id()), Some(run.id().clone()), None)
    );
    assert_eq!(on_run.finding.detail, "long");
    let on_goal = queue
        .record_finding(finding(FindingTarget::Goal(goal.id())))
        .unwrap();
    assert_eq!(on_goal.finding.goal_id, Some(goal.id()));
    let task_events = serde_json::to_value(queue.show(task.id()).unwrap()).unwrap();
    assert!(
        task_events.to_string().contains("finding_recorded"),
        "{task_events}"
    );
    for missing in [
        FindingTarget::Run(RunId::new("missing").unwrap()),
        FindingTarget::Goal(GoalId::new(99)),
        FindingTarget::Task(TaskId::new(99)),
    ] {
        assert!(queue.record_finding(finding(missing)).is_err());
    }
    let listed = queue
        .findings(&FindingQuery {
            target: Some(FindingTarget::Run(run.id().clone())),
            ..FindingQuery::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].finding.id, on_run.finding.id);
    let dismissed = queue
        .set_finding_status(
            on_goal.finding.id,
            FindingStatus::Dismissed,
            "known",
            "planner",
        )
        .unwrap();
    assert_eq!(dismissed.status, FindingStatus::Dismissed);
    assert!(
        queue
            .set_finding_status(on_goal.finding.id, FindingStatus::Resolved, " ", "planner")
            .is_err()
    );
    assert!(
        queue
            .findings(&FindingQuery {
                id: Some(dagq::domain::FindingId::new(99)),
                ..FindingQuery::default()
            })
            .is_err()
    );
}
