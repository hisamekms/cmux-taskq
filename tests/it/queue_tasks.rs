//! Queue tests: editing a task (paths, fields, priority), the priority order,
//! and `list`.
use crate::common;

use dagq::{
    application::{StatusFilter, TaskQuery, TaskStore, dependency_graph},
    domain::{
        ClaimOutcome, EvidenceCheck, GoalId, Priority, TaskAction, TaskEdit, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

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
                {"id": d, "status": "ready", "priority": "normal", "kind": null, "title": "waiting",
                 "goal_id": null,
                 "dependencies": [], "goal_dependencies": [], "latest_run": null},
                {"id": c, "status": "in_progress", "priority": "normal", "kind": null,
                 "title": "claimed",
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
