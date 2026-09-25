//! The proposal aggregate (ADR-0041 decision 7): goals and tasks a planner
//! submits together for plan review, tied to the planner that owns them.
//! Plan review checks a proposal as one unit and sends a revise back to its
//! owner. A task or a goal belongs to one active proposal at a time.

use serde::{Deserialize, Serialize};

use super::{DomainError, GoalId, PlannerOrigin, ProposalId, ProposalStatus, TaskId, require};

/// The planner that owns a proposal: whether a person or the runtime
/// opened it, and its cmux workspace (`CMUX_WORKSPACE_ID`), absent when the
/// planner submitted from outside cmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerOwner {
    pub origin: PlannerOrigin,
    pub workspace_id: Option<String>,
}

/// `dagq submit`: the tasks to bundle, the goals whose draft tasks join
/// them, and the proposal to submit again after a revise (`None`: a new
/// one).
#[derive(Debug, Clone)]
pub struct Submission {
    pub tasks: Vec<TaskId>,
    pub goals: Vec<GoalId>,
    pub proposal: Option<ProposalId>,
    pub owner: PlannerOwner,
}

impl Submission {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(self.tasks.iter().all(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "task ID" }
        })?;
        require(self.goals.iter().all(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "goal ID" }
        })?;
        require(self.proposal.is_none_or(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId {
                field: "proposal ID",
            }
        })?;
        require(
            self.owner
                .workspace_id
                .as_deref()
                .is_none_or(|id| !id.trim().is_empty()),
            || DomainError::Blank {
                field: "planner workspace ID",
            },
        )
    }
}

/// A stored proposal as the store reads it back.
#[derive(Debug, Clone)]
pub struct ProposalRecord {
    pub id: ProposalId,
    pub status: ProposalStatus,
    pub owner: PlannerOwner,
    pub submitted_at: String,
    pub revise_count: u32,
    pub task_ids: Vec<TaskId>,
    pub goal_ids: Vec<GoalId>,
    pub created_at: String,
    pub updated_at: String,
}

/// Goals and tasks under plan review together. `Serialize` is the JSON the
/// CLI prints; a proposal is built by [`Proposal::submit`] or
/// [`Proposal::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    id: ProposalId,
    status: ProposalStatus,
    owner: PlannerOwner,
    /// When it was last submitted: plan review takes proposals in this
    /// order (ADR-0041 decision 15).
    submitted_at: String,
    /// How many times plan review sent it back (decision 11).
    revise_count: u32,
    /// The member tasks, ascending.
    task_ids: Vec<TaskId>,
    /// The member goals, ascending.
    goal_ids: Vec<GoalId>,
    created_at: String,
    updated_at: String,
}

impl ProposalStatus {
    /// Whether it still holds its members: waiting for plan review or being
    /// revised by its planner.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Submitted | Self::Revising)
    }
}

impl Proposal {
    /// A proposal submitted now as `id` by `owner` with its members; it
    /// holds at least one task.
    pub fn submit(
        id: ProposalId,
        owner: PlannerOwner,
        task_ids: Vec<TaskId>,
        goal_ids: Vec<GoalId>,
        now: String,
    ) -> Result<Self, DomainError> {
        require(id.as_i64() > 0, || DomainError::NonPositiveId {
            field: "proposal ID",
        })?;
        require_tasks(&task_ids)?;
        Ok(Self {
            id,
            status: ProposalStatus::Submitted,
            owner,
            submitted_at: now.clone(),
            revise_count: 0,
            task_ids: sorted(task_ids),
            goal_ids: sorted(goal_ids),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub fn restore(record: ProposalRecord) -> Result<Self, DomainError> {
        require(record.id.as_i64() > 0, || DomainError::NonPositiveId {
            field: "proposal ID",
        })?;
        Ok(Self {
            id: record.id,
            status: record.status,
            owner: record.owner,
            submitted_at: record.submitted_at,
            revise_count: record.revise_count,
            task_ids: sorted(record.task_ids),
            goal_ids: sorted(record.goal_ids),
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    pub fn id(&self) -> ProposalId {
        self.id
    }

    pub fn status(&self) -> ProposalStatus {
        self.status
    }

    pub fn owner(&self) -> &PlannerOwner {
        &self.owner
    }

    pub fn submitted_at(&self) -> &str {
        &self.submitted_at
    }

    pub fn revise_count(&self) -> u32 {
        self.revise_count
    }

    pub fn task_ids(&self) -> &[TaskId] {
        &self.task_ids
    }

    pub fn goal_ids(&self) -> &[GoalId] {
        &self.goal_ids
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

fn sorted<T: Ord>(mut ids: Vec<T>) -> Vec<T> {
    ids.sort();
    ids.dedup();
    ids
}

fn require_tasks(task_ids: &[TaskId]) -> Result<(), DomainError> {
    require(!task_ids.is_empty(), || DomainError::EmptyProposal)
}

fn require_status(proposal: &Proposal, expected: ProposalStatus) -> Result<(), DomainError> {
    require(proposal.status == expected, || {
        DomainError::ProposalNotInStatus {
            proposal_id: proposal.id,
            status: proposal.status,
            expected,
        }
    })
}

/// Submit a proposal plan review sent back again, now owned by `owner` and
/// holding `task_ids` and `goal_ids` (what it held plus what joined).
pub fn resubmit(
    mut proposal: Proposal,
    owner: PlannerOwner,
    task_ids: Vec<TaskId>,
    goal_ids: Vec<GoalId>,
    now: String,
) -> Result<Proposal, DomainError> {
    require_status(&proposal, ProposalStatus::Revising)?;
    require_tasks(&task_ids)?;
    proposal.status = ProposalStatus::Submitted;
    proposal.owner = owner;
    proposal.task_ids = sorted(task_ids);
    proposal.goal_ids = sorted(goal_ids);
    proposal.submitted_at = now.clone();
    proposal.updated_at = now;
    Ok(proposal)
}

/// Plan review passed the proposal (decision 11): its submitted tasks
/// become ready.
pub fn accept(mut proposal: Proposal, now: String) -> Result<Proposal, DomainError> {
    require_status(&proposal, ProposalStatus::Submitted)?;
    proposal.status = ProposalStatus::Accepted;
    proposal.updated_at = now;
    Ok(proposal)
}

/// Plan review takes a submitted proposal it held (its job failed, or its
/// concern was not answered with one of the options) again, as it is, in
/// the order of this new submission.
pub fn retry(mut proposal: Proposal, now: String) -> Result<Proposal, DomainError> {
    require_status(&proposal, ProposalStatus::Submitted)?;
    proposal.submitted_at = now.clone();
    proposal.updated_at = now;
    Ok(proposal)
}

/// A person answered the plan review's `approve_plan` ask with `cancel`
/// (decision 11): the proposal ends with its tasks canceled.
pub fn cancel(mut proposal: Proposal, now: String) -> Result<Proposal, DomainError> {
    require_status(&proposal, ProposalStatus::Submitted)?;
    proposal.status = ProposalStatus::Canceled;
    proposal.updated_at = now;
    Ok(proposal)
}

/// Its planner (or a person) withdraws a submitted or revising proposal:
/// it ends as `canceled` without plan review, and its members are released,
/// the submitted tasks back to draft, free to join another proposal.
pub fn withdraw(mut proposal: Proposal, now: String) -> Result<Proposal, DomainError> {
    require(proposal.status.is_active(), || {
        DomainError::ProposalNotActive {
            proposal_id: proposal.id,
            status: proposal.status,
        }
    })?;
    proposal.status = ProposalStatus::Canceled;
    proposal.updated_at = now;
    Ok(proposal)
}

/// A proposal of its own for ready tasks plan review found have to change
/// (decision 14): it starts out sent back, owned by the runtime's planner
/// that will fix and submit it, with no revise counted against it.
pub fn reopen(id: ProposalId, task_ids: Vec<TaskId>, now: String) -> Result<Proposal, DomainError> {
    let mut proposal = Proposal::submit(
        id,
        PlannerOwner {
            origin: PlannerOrigin::Runtime,
            workspace_id: None,
        },
        task_ids,
        Vec::new(),
        now,
    )?;
    proposal.status = ProposalStatus::Revising;
    Ok(proposal)
}

/// Plan review sent the proposal back to its planner (decision 11): its
/// tasks return to draft until the planner submits it again.
pub fn send_back(mut proposal: Proposal, now: String) -> Result<Proposal, DomainError> {
    require_status(&proposal, ProposalStatus::Submitted)?;
    proposal.status = ProposalStatus::Revising;
    proposal.revise_count += 1;
    proposal.updated_at = now;
    Ok(proposal)
}

/// Whether task `task_id`, now in `current` (its proposal and that
/// proposal's status), may join proposal `target` (`None`: a new one): only
/// when it is in no active proposal or already in `target`.
pub fn check_task_joins(
    task_id: TaskId,
    current: Option<(ProposalId, ProposalStatus)>,
    target: Option<ProposalId>,
) -> Result<(), DomainError> {
    match current {
        Some((proposal_id, status)) if status.is_active() && Some(proposal_id) != target => {
            Err(DomainError::TaskInOtherProposal {
                task_id,
                proposal_id,
            })
        }
        _ => Ok(()),
    }
}

/// [`check_task_joins`] for a goal.
pub fn check_goal_joins(
    goal_id: GoalId,
    current: Option<(ProposalId, ProposalStatus)>,
    target: Option<ProposalId>,
) -> Result<(), DomainError> {
    match current {
        Some((proposal_id, status)) if status.is_active() && Some(proposal_id) != target => {
            Err(DomainError::GoalInOtherProposal {
                goal_id,
                proposal_id,
            })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(origin: PlannerOrigin) -> PlannerOwner {
        PlannerOwner {
            origin,
            workspace_id: Some("W1".into()),
        }
    }

    fn submitted() -> Proposal {
        Proposal::submit(
            ProposalId::new(2),
            owner(PlannerOrigin::Person),
            vec![TaskId::new(5), TaskId::new(3), TaskId::new(5)],
            vec![GoalId::new(1)],
            "t0".into(),
        )
        .unwrap()
    }

    #[test]
    fn a_submitted_proposal_holds_its_members_once_in_order() {
        let proposal = submitted();
        assert_eq!(proposal.id(), ProposalId::new(2));
        assert_eq!(proposal.status(), ProposalStatus::Submitted);
        assert_eq!(proposal.task_ids(), [TaskId::new(3), TaskId::new(5)]);
        assert_eq!(proposal.goal_ids(), [GoalId::new(1)]);
        assert_eq!(proposal.owner(), &owner(PlannerOrigin::Person));
        assert_eq!(proposal.revise_count(), 0);
        assert_eq!(
            (
                proposal.submitted_at(),
                proposal.created_at(),
                proposal.updated_at()
            ),
            ("t0", "t0", "t0")
        );
        assert_eq!(
            serde_json::to_value(&proposal).unwrap(),
            serde_json::json!({
                "id": 2, "status": "submitted",
                "owner": {"origin": "person", "workspace_id": "W1"},
                "submitted_at": "t0", "revise_count": 0, "task_ids": [3, 5], "goal_ids": [1],
                "created_at": "t0", "updated_at": "t0",
            })
        );
        let empty = Proposal::submit(
            ProposalId::new(2),
            owner(PlannerOrigin::Person),
            Vec::new(),
            vec![GoalId::new(1)],
            "t0".into(),
        );
        assert_eq!(
            empty.unwrap_err().to_string(),
            "a proposal needs at least one draft task"
        );
        assert!(
            Proposal::submit(
                ProposalId::new(0),
                owner(PlannerOrigin::Person),
                vec![TaskId::new(1)],
                Vec::new(),
                "t0".into()
            )
            .is_err()
        );
    }

    #[test]
    fn plan_review_accepts_or_sends_back_and_the_planner_resubmits() {
        let accepted = accept(submitted(), "t1".into()).unwrap();
        assert_eq!(accepted.status(), ProposalStatus::Accepted);
        assert_eq!(accepted.updated_at(), "t1");
        assert!(!accepted.status().is_active());
        assert_eq!(
            accept(accepted.clone(), "t2".into())
                .unwrap_err()
                .to_string(),
            "proposal 2 is accepted, not submitted"
        );
        assert!(send_back(accepted, "t2".into()).is_err());

        let revising = send_back(submitted(), "t1".into()).unwrap();
        assert_eq!(revising.status(), ProposalStatus::Revising);
        assert_eq!(revising.revise_count(), 1);
        assert!(revising.status().is_active());
        let again = resubmit(
            revising.clone(),
            owner(PlannerOrigin::Runtime),
            vec![TaskId::new(7), TaskId::new(3)],
            Vec::new(),
            "t2".into(),
        )
        .unwrap();
        assert_eq!(again.status(), ProposalStatus::Submitted);
        assert_eq!(again.submitted_at(), "t2");
        assert_eq!(again.revise_count(), 1);
        assert_eq!(again.owner().origin, PlannerOrigin::Runtime);
        assert_eq!(again.task_ids(), [TaskId::new(3), TaskId::new(7)]);
        assert!(again.goal_ids().is_empty());
        assert_eq!(
            resubmit(
                again,
                owner(PlannerOrigin::Person),
                vec![TaskId::new(3)],
                Vec::new(),
                "t3".into()
            )
            .unwrap_err(),
            DomainError::ProposalNotInStatus {
                proposal_id: ProposalId::new(2),
                status: ProposalStatus::Submitted,
                expected: ProposalStatus::Revising,
            }
        );
        assert_eq!(
            resubmit(
                revising,
                owner(PlannerOrigin::Person),
                Vec::new(),
                Vec::new(),
                "t3".into()
            )
            .unwrap_err(),
            DomainError::EmptyProposal
        );
    }

    #[test]
    fn a_person_cancels_a_submitted_proposal_and_a_reopened_one_starts_sent_back() {
        let retried = retry(submitted(), "t9".into()).unwrap();
        assert_eq!(retried.submitted_at(), "t9");
        assert_eq!(retried.status(), ProposalStatus::Submitted);
        assert!(retry(accept(submitted(), "t1".into()).unwrap(), "t2".into()).is_err());
        let canceled = cancel(submitted(), "t1".into()).unwrap();
        assert_eq!(canceled.status(), ProposalStatus::Canceled);
        assert!(cancel(canceled, "t2".into()).is_err());
        let reopened = reopen(ProposalId::new(7), vec![TaskId::new(4)], "t3".into()).unwrap();
        assert_eq!(reopened.status(), ProposalStatus::Revising);
        assert_eq!(reopened.revise_count(), 0);
        assert_eq!(reopened.owner().origin, PlannerOrigin::Runtime);
        assert_eq!(reopened.owner().workspace_id, None);
        assert_eq!(reopened.task_ids(), [TaskId::new(4)]);
        assert!(reopen(ProposalId::new(7), Vec::new(), "t3".into()).is_err());
    }

    #[test]
    fn a_submitted_or_revising_proposal_is_withdrawn_and_releases_its_members() {
        let withdrawn = withdraw(submitted(), "t1".into()).unwrap();
        assert_eq!(withdrawn.status(), ProposalStatus::Canceled);
        assert_eq!(withdrawn.updated_at(), "t1");
        assert!(!withdrawn.status().is_active());
        check_task_joins(
            TaskId::new(3),
            Some((withdrawn.id(), withdrawn.status())),
            None,
        )
        .unwrap();
        let revising = send_back(submitted(), "t1".into()).unwrap();
        let withdrawn = withdraw(revising, "t2".into()).unwrap();
        assert_eq!(withdrawn.status(), ProposalStatus::Canceled);
        assert_eq!(withdrawn.revise_count(), 1);
        assert_eq!(
            withdraw(withdrawn, "t3".into()).unwrap_err().to_string(),
            "proposal 2 is canceled; only a submitted or revising proposal is withdrawn"
        );
        assert!(withdraw(accept(submitted(), "t1".into()).unwrap(), "t2".into()).is_err());
    }

    #[test]
    fn restore_keeps_the_stored_state() {
        let record = ProposalRecord {
            id: ProposalId::new(4),
            status: ProposalStatus::Canceled,
            owner: owner(PlannerOrigin::Runtime),
            submitted_at: "s".into(),
            revise_count: 2,
            task_ids: vec![TaskId::new(9), TaskId::new(8)],
            goal_ids: Vec::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
        };
        let proposal = Proposal::restore(record.clone()).unwrap();
        assert_eq!(proposal.status(), ProposalStatus::Canceled);
        assert_eq!(proposal.revise_count(), 2);
        assert_eq!(proposal.task_ids(), [TaskId::new(8), TaskId::new(9)]);
        assert!(
            Proposal::restore(ProposalRecord {
                id: ProposalId::new(0),
                ..record
            })
            .is_err()
        );
    }

    #[test]
    fn a_member_joins_only_when_no_other_active_proposal_holds_it() {
        let task = TaskId::new(3);
        let goal = GoalId::new(1);
        let (one, two) = (ProposalId::new(1), ProposalId::new(2));
        check_task_joins(task, None, None).unwrap();
        check_task_joins(task, Some((one, ProposalStatus::Accepted)), None).unwrap();
        check_task_joins(task, Some((one, ProposalStatus::Revising)), Some(one)).unwrap();
        assert_eq!(
            check_task_joins(task, Some((one, ProposalStatus::Submitted)), Some(two))
                .unwrap_err()
                .to_string(),
            "task 3 already belongs to proposal 1"
        );
        check_goal_joins(goal, Some((one, ProposalStatus::Canceled)), None).unwrap();
        assert_eq!(
            check_goal_joins(goal, Some((one, ProposalStatus::Revising)), None)
                .unwrap_err()
                .to_string(),
            "goal 1 already belongs to proposal 1"
        );
    }

    #[test]
    fn a_submission_names_positive_ids_and_a_real_workspace() {
        let submission = Submission {
            tasks: vec![TaskId::new(1)],
            goals: vec![GoalId::new(1)],
            proposal: Some(ProposalId::new(1)),
            owner: owner(PlannerOrigin::Person),
        };
        submission.validate().unwrap();
        for (broken, message) in [
            (
                Submission {
                    tasks: vec![TaskId::new(0)],
                    ..submission.clone()
                },
                "task ID must be positive",
            ),
            (
                Submission {
                    goals: vec![GoalId::new(-1)],
                    ..submission.clone()
                },
                "goal ID must be positive",
            ),
            (
                Submission {
                    proposal: Some(ProposalId::new(0)),
                    ..submission.clone()
                },
                "proposal ID must be positive",
            ),
            (
                Submission {
                    owner: PlannerOwner {
                        origin: PlannerOrigin::Person,
                        workspace_id: Some(" ".into()),
                    },
                    ..submission.clone()
                },
                "planner workspace ID must not be blank",
            ),
        ] {
            assert_eq!(broken.validate().unwrap_err().to_string(), message);
        }
    }
}
