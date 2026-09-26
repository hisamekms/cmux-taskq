//! Queue tests: submitting proposals, plan review's verdicts and withdrawal.
use crate::common;

use dagq::{
    application::{StatusFilter, TaskQuery, TaskStore, dependency_graph},
    domain::{
        ClaimOutcome, GoalId, GoalVerdict, NewGoal, PlannerOrigin, PlannerOwner, ProposalId,
        ProposalStatus, Submission, TaskAction, TaskEdit, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};

use common::queue::*;

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

/// A submitted or revising proposal is withdrawn: it ends as canceled, its
/// submitted tasks return to draft, and its tasks and goals join another
/// proposal.
#[test]
fn a_withdrawn_proposal_releases_its_goals_and_tasks_as_drafts() {
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

    // A submitted one.
    let first = queue.submit(submission(&[b], &[goal], None)).unwrap();
    let withdrawn = queue.withdraw_proposal(first.id()).unwrap();
    assert_eq!(withdrawn.status(), ProposalStatus::Canceled);
    assert_eq!(withdrawn.task_ids(), [a, b]);
    for id in [a, b] {
        assert_eq!(status_of(&mut queue, id), TaskStatus::Draft);
        assert!(queue.show(id).unwrap().events.iter().any(|e| {
            e.kind == "proposal_withdrawn"
                && e.payload == serde_json::json!({"proposal_id": 1, "from": "submitted"})
        }));
    }
    assert!(
        queue
            .show_goal(goal)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "proposal_withdrawn")
    );
    assert!(queue.proposals(false).unwrap().is_empty());
    assert_eq!(
        queue.withdraw_proposal(first.id()).unwrap_err().to_string(),
        "proposal 1 is canceled; only a submitted or revising proposal is withdrawn"
    );
    assert!(queue.submit(submission(&[a], &[], Some(1))).is_err());

    // Its members join another proposal, which plan review sends back; the
    // revising one is withdrawn with its drafts, whatever it held.
    let second = queue.submit(submission(&[b], &[goal], None)).unwrap();
    assert_eq!(second.id(), ProposalId::new(2));
    assert_eq!(second.task_ids(), [a, b]);
    queue.send_back_proposal(second.id()).unwrap();
    queue.transition(b, TaskAction::Cancel).unwrap();
    let withdrawn = queue.withdraw_proposal(second.id()).unwrap();
    assert_eq!(withdrawn.status(), ProposalStatus::Canceled);
    assert_eq!(withdrawn.revise_count(), 1);
    assert_eq!(status_of(&mut queue, a), TaskStatus::Draft);
    assert_eq!(status_of(&mut queue, b), TaskStatus::Canceled);
    assert!(queue.show(a).unwrap().events.iter().any(|e| {
        e.kind == "proposal_withdrawn"
            && e.payload == serde_json::json!({"proposal_id": 2, "from": "revising"})
    }));

    // A third proposal takes the goal and its draft, and passes.
    let third = queue.submit(submission(&[], &[goal], None)).unwrap();
    assert_eq!(third.task_ids(), [a]);
    assert_eq!(third.goal_ids(), [goal]);
    queue.approve_proposal(third.id()).unwrap();
    assert_eq!(status_of(&mut queue, a), TaskStatus::Ready);
    assert!(queue.withdraw_proposal(third.id()).is_err());
    assert!(queue.withdraw_proposal(ProposalId::new(9)).is_err());
}

/// A goal abandoned while its task waits for plan review keeps the task
/// out of `ready`: approving the proposal returns it to draft instead, so
/// it is never claimed.
#[test]
fn approving_a_proposal_withholds_the_tasks_of_a_closed_goal() {
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("dropped")).unwrap().id();
    let mut in_goal = new_task("in goal");
    in_goal.goal_id = Some(goal);
    let a = queue.add(in_goal).unwrap().id();
    let b = queue.add(new_task("alone")).unwrap().id();
    let proposal = queue.submit(submission(&[a, b], &[], None)).unwrap();
    queue.close_goal(goal, GoalVerdict::Abandoned).unwrap();

    let accepted = queue.approve_proposal(proposal.id()).unwrap();
    assert_eq!(accepted.status(), ProposalStatus::Accepted);
    assert_eq!(status_of(&mut queue, a), TaskStatus::Draft);
    assert_eq!(status_of(&mut queue, b), TaskStatus::Ready);
    let ready: Vec<TaskId> = queue
        .candidates()
        .unwrap()
        .iter()
        .map(|task| task.id())
        .collect();
    assert_eq!(ready, [b]);
    assert!(queue.show(a).unwrap().events.iter().any(|e| {
        e.kind == "approve_withheld"
            && e.payload
                == serde_json::json!({"proposal_id": 1, "goal_id": goal, "verdict": "abandoned"})
    }));
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
