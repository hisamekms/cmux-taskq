//! Plan review (ADR-0041 decisions 10, 11, 14, 15): the verdict the
//! headless job prints about one proposal, the fixes it may make itself,
//! the order the supervisor takes submitted proposals in, and the answers
//! a person gives to its `approve_plan` ask.

use serde::{Deserialize, Serialize};

use super::{AskId, DomainError, Priority, ProposalId, TaskId, parse_json_object};

// What plan review decided (ADR-0041 decision 11): `pass` makes the
// proposal's tasks ready, `revise` sends it back to its planner, `concern`
// waits for a person in an `approve_plan` ask.
string_enum!(PlanReviewDecision {
    Pass => "pass",
    Revise => "revise",
    Concern => "concern",
});

/// A fix plan review makes itself (ADR-0041 decision 11). Nothing else is
/// the job's to change: rewriting a task, splitting it, removing a
/// dependency or raising a priority is the planner's, through `revise`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanReviewAction {
    /// `task_id` (a task of the proposal) waits for `depends_on`.
    AddDependency { task_id: TaskId, depends_on: TaskId },
    /// `task_id` (a task of the proposal) gets the lower `priority`.
    LowerPriority { task_id: TaskId, priority: Priority },
    /// `task_id` (a task of the proposal) repeats `duplicate_of`, another
    /// task, so it is canceled. Only an obvious duplicate: a doubtful one is
    /// a `concern`.
    CancelDuplicate {
        task_id: TaskId,
        duplicate_of: TaskId,
    },
}

impl PlanReviewAction {
    /// The task of the proposal the action changes.
    pub fn task_id(&self) -> TaskId {
        match self {
            Self::AddDependency { task_id, .. }
            | Self::LowerPriority { task_id, .. }
            | Self::CancelDuplicate { task_id, .. } => *task_id,
        }
    }
}

/// A task already `ready`, outside the proposal, that has to change for
/// the proposal to hold (ADR-0041 decision 14): the runtime takes it out
/// of `ready` and a planner of its own fixes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reopen {
    pub task_id: TaskId,
    pub reason: String,
}

/// What the headless plan review prints on stdout: one JSON object.
/// `actions` are applied on `pass` only; `reopen` whatever the verdict;
/// `precedents` name asks a person answered before about the same kind of
/// finding, which the runtime quotes to the planner or the person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReviewVerdict {
    pub verdict: PlanReviewDecision,
    pub reasons: Vec<String>,
    pub summary: String,
    #[serde(default)]
    pub actions: Vec<PlanReviewAction>,
    #[serde(default)]
    pub reopen: Vec<Reopen>,
    #[serde(default)]
    pub precedents: Vec<AskId>,
}

impl PlanReviewVerdict {
    /// The verdict in the job's stdout, found the way the run review's is.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        parse_json_object(stdout)
            .map_err(|error| format!("the plan review printed no verdict JSON: {error}"))
    }
}

/// How many times plan review sends one proposal back to its planner; a
/// later review that does not pass is a `concern` (ADR-0041 decision 11,
/// the same bound as a run's review).
pub const MAX_PLAN_REVISES: u32 = super::MAX_REVISE_ATTEMPTS as u32;

/// The options of the `approve_plan` ask a `concern` opens, which the
/// supervisor applies once answered (ADR-0041 decision 11).
pub const PLAN_OPTIONS: &[&str] = &["ready", "send_back", "cancel"];

/// Who opens the `approve_plan` asks.
pub const PLAN_REVIEW_ASKER: &str = "plan_review";

/// A person's answer to an `approve_plan` ask the supervisor applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAnswer {
    /// Make the proposal's tasks ready as they are.
    Ready,
    /// Send the proposal back to its planner, with the person's reason
    /// when they gave one (`send_back: <reason>`).
    SendBack(Option<String>),
    /// Cancel the proposal's tasks.
    Cancel,
}

impl PlanAnswer {
    /// One of [`PLAN_OPTIONS`], `send_back` optionally followed by `:` and
    /// the person's reason; anything else is a person's to read.
    pub fn parse(answer: &str) -> Option<Self> {
        let answer = answer.trim();
        match answer {
            "ready" => return Some(Self::Ready),
            "cancel" => return Some(Self::Cancel),
            "send_back" => return Some(Self::SendBack(None)),
            _ => {}
        }
        let reason = answer.strip_prefix("send_back")?.trim_start();
        let reason = reason.strip_prefix(':')?.trim();
        Some(Self::SendBack(
            (!reason.is_empty()).then(|| reason.to_owned()),
        ))
    }
}

/// A proposal waiting for plan review, as the supervisor orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewCandidate {
    pub proposal_id: ProposalId,
    pub submitted_at: String,
    /// One of its submitted tasks has the `interrupt` priority.
    pub interrupt: bool,
}

/// The proposal plan review takes next (ADR-0041 decision 15): one with an
/// `interrupt` task first, then the oldest submission, then the lowest ID.
pub fn next_to_review(candidates: &[PlanReviewCandidate]) -> Option<ProposalId> {
    candidates
        .iter()
        .min_by(|a, b| {
            (!a.interrupt, &a.submitted_at, a.proposal_id).cmp(&(
                !b.interrupt,
                &b.submitted_at,
                b.proposal_id,
            ))
        })
        .map(|candidate| candidate.proposal_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_parses_with_its_actions_reopen_and_precedents() {
        let verdict = PlanReviewVerdict::parse(
            r#"Here it is: {"verdict": "pass", "reasons": [], "summary": "fine",
               "actions": [{"action": "add_dependency", "task_id": 3, "depends_on": 1},
                           {"action": "lower_priority", "task_id": 3, "priority": "low"},
                           {"action": "cancel_duplicate", "task_id": 4, "duplicate_of": 2}],
               "reopen": [{"task_id": 9, "reason": "clashes"}], "precedents": [30]}"#,
        )
        .unwrap();
        assert_eq!(verdict.verdict, PlanReviewDecision::Pass);
        assert_eq!(
            verdict.actions,
            [
                PlanReviewAction::AddDependency {
                    task_id: TaskId::new(3),
                    depends_on: TaskId::new(1)
                },
                PlanReviewAction::LowerPriority {
                    task_id: TaskId::new(3),
                    priority: Priority::Low
                },
                PlanReviewAction::CancelDuplicate {
                    task_id: TaskId::new(4),
                    duplicate_of: TaskId::new(2)
                },
            ]
        );
        assert_eq!(
            verdict
                .actions
                .iter()
                .map(PlanReviewAction::task_id)
                .collect::<Vec<_>>(),
            [TaskId::new(3), TaskId::new(3), TaskId::new(4)]
        );
        assert_eq!(verdict.reopen[0].task_id, TaskId::new(9));
        assert_eq!(verdict.precedents, [AskId::new(30)]);
        let bare =
            PlanReviewVerdict::parse(r#"{"verdict":"revise","reasons":["x"],"summary":"s"}"#)
                .unwrap();
        assert!(bare.actions.is_empty() && bare.reopen.is_empty() && bare.precedents.is_empty());
        for broken in [
            "no json",
            r#"{"verdict":"maybe","reasons":[],"summary":""}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"","extra":1}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"","actions":[{"action":"rewrite","task_id":1}]}"#,
        ] {
            let error = PlanReviewVerdict::parse(broken).unwrap_err();
            assert!(error.starts_with("the plan review printed no verdict JSON"));
        }
    }

    #[test]
    fn an_answer_is_one_of_the_options_and_send_back_may_carry_a_reason() {
        assert_eq!(PlanAnswer::parse(" ready "), Some(PlanAnswer::Ready));
        assert_eq!(PlanAnswer::parse("cancel"), Some(PlanAnswer::Cancel));
        assert_eq!(
            PlanAnswer::parse("send_back"),
            Some(PlanAnswer::SendBack(None))
        );
        assert_eq!(
            PlanAnswer::parse("send_back: split it in two"),
            Some(PlanAnswer::SendBack(Some("split it in two".into())))
        );
        assert_eq!(
            PlanAnswer::parse("send_back:  "),
            Some(PlanAnswer::SendBack(None))
        );
        for other in [
            "",
            "land",
            "send_backwards",
            "ready now",
            "send_back reason",
        ] {
            assert_eq!(PlanAnswer::parse(other), None, "{other}");
        }
    }

    #[test]
    fn interrupt_proposals_come_first_then_the_oldest_submission() {
        let candidate = |id: i64, at: &str, interrupt: bool| PlanReviewCandidate {
            proposal_id: ProposalId::new(id),
            submitted_at: at.into(),
            interrupt,
        };
        assert_eq!(next_to_review(&[]), None);
        let plain = [candidate(3, "t2", false), candidate(2, "t1", false)];
        assert_eq!(next_to_review(&plain), Some(ProposalId::new(2)));
        let urgent = [
            candidate(1, "t0", false),
            candidate(5, "t9", true),
            candidate(4, "t9", true),
        ];
        assert_eq!(next_to_review(&urgent), Some(ProposalId::new(4)));
    }
}
