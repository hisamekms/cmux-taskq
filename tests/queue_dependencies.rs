//! Queue tests: task dependencies, goal dependencies and candidates, and
//! how dependency changes serialize with claims.
mod common;

use std::sync::{Arc, Barrier};

use dagq::{
    application::{TaskQuery, TaskStore},
    domain::{
        ClaimOutcome, CommitSha, GoalId, GoalVerdict, Provider, RunId, RunStatus, TaskAction,
        TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

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
    let _waiting = common::within(common::STEP_LIMIT, "the editing threads to return");
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
    let _waiting = common::within(common::STEP_LIMIT, "the claim and the edit to return");
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
