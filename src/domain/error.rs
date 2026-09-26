use std::fmt;

use super::{
    AskKind, AskReason, CheckStatus, FindingId, FindingStatus, GoalId, GoalVerdict, ProposalId,
    ProposalStatus, ReceiptResult, RunId, RunStatus, TaskAction, TaskId, TaskStatus,
};

/// A business rejection by the domain: an invalid value, a transition the
/// task's status does not allow, or a condition that does not hold. Each
/// variant carries only what its message needs, and `Display` is the message
/// the CLI prints and the runtime writes to `last_error`. I/O failures are not
/// domain errors; the layers that perform I/O convert this at their boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A stored or given string is not a value of the enum `kind`.
    UnknownValue {
        kind: &'static str,
        value: String,
    },
    /// A manual transition of an in-progress task that still owns an unfinished run.
    TaskHasUnfinishedRun {
        action: TaskAction,
    },
    /// `action` is not allowed from `status`.
    TransitionNotAllowed {
        status: TaskStatus,
        action: TaskAction,
    },
    /// A goal that is closed takes no task (`add --goal`, `set-goal`).
    GoalClosed {
        goal_id: GoalId,
        verdict: Option<GoalVerdict>,
    },
    /// A stored goal with a close time but no verdict, or the reverse.
    GoalCloseInconsistent {
        goal_id: GoalId,
    },
    /// A draft or submitted task made ready by hand without the bypass:
    /// only plan review readies it (ADR-0041 decision 8).
    ReadyNeedsPlanReview {
        status: TaskStatus,
    },
    /// `what` of a task that is neither a draft, submitted nor ready.
    TaskNotEditable {
        what: &'static str,
    },
    /// `dagq edit` of a task whose status keeps its content (ADR-0041
    /// decision 9): only a draft or a submitted task is edited.
    TaskContentNotEditable {
        task_id: TaskId,
        status: TaskStatus,
    },
    /// A dependency of a task on itself.
    SelfDependency,
    /// The predecessor already depends on the task, directly or not, where
    /// a task also waits for the goals it depends on and a goal for its
    /// tasks (ADR-0038).
    DependencyCycle {
        task_id: TaskId,
        predecessor_id: TaskId,
    },
    /// A dependency of a task on the goal it belongs to: the goal waits for
    /// the task, so it is a dependency on itself (ADR-0038).
    OwnGoalDependency {
        goal_id: GoalId,
    },
    /// The goal already waits for the task, directly or not.
    GoalDependencyCycle {
        task_id: TaskId,
        goal_id: GoalId,
    },
    /// Moving the task into the goal would make the goal wait for a task
    /// that already waits for the goal, directly or not.
    GoalMembershipCycle {
        task_id: TaskId,
        goal_id: GoalId,
    },
    /// A proposal without a task (`submit`).
    EmptyProposal,
    /// A `search` query the index cannot run (ADR-0046 decision 1).
    SearchQuery {
        reason: String,
    },
    /// A task already in another proposal that is submitted or revising.
    TaskInOtherProposal {
        task_id: TaskId,
        proposal_id: ProposalId,
    },
    /// A goal already in another proposal that is submitted or revising.
    GoalInOtherProposal {
        goal_id: GoalId,
        proposal_id: ProposalId,
    },
    /// A proposal command its status does not allow.
    ProposalNotInStatus {
        proposal_id: ProposalId,
        status: ProposalStatus,
        expected: ProposalStatus,
    },
    /// A proposal that no longer holds its members is not withdrawn.
    ProposalNotActive {
        proposal_id: ProposalId,
        status: ProposalStatus,
    },
    /// A required text field is blank.
    Blank {
        field: &'static str,
    },
    /// An ID field is zero or negative.
    NonPositiveId {
        field: &'static str,
    },
    /// A goal records its verdict once.
    GoalAlreadyClosed {
        goal_id: GoalId,
        verdict: Option<GoalVerdict>,
    },
    /// Tasks in `blocking` (status and count) do not allow `verdict`.
    GoalCloseBlocked {
        goal_id: GoalId,
        verdict: GoalVerdict,
        blocking: Vec<(TaskStatus, usize)>,
    },
    /// The receipt text is not a completion receipt; `reason` is the parser's.
    MalformedReceipt {
        reason: String,
    },
    ReceiptRunMismatch {
        receipt_run_id: String,
        run_id: RunId,
    },
    /// The agent itself reported the run as not succeeded.
    AgentReportedResult {
        result: ReceiptResult,
        summary: String,
    },
    /// The receipt reports the check `check` as failed.
    ReceiptCheckFailed {
        check: &'static str,
        evidence_or_reason: String,
    },
    /// The receipt claims `status` for `check` without evidence or reason.
    ReceiptCheckUnexplained {
        check: &'static str,
        status: CheckStatus,
    },
    /// `field` is not a full Git object ID.
    InvalidCommit {
        field: &'static str,
    },
    FollowUpsNotArray,
    /// The run has no run directory yet.
    MissingRunDirectory,
    /// `goal ready` on a goal that is not a draft.
    GoalNotDraft {
        goal_id: GoalId,
    },
    /// A note kind that is not a lowercase slug.
    InvalidNoteKind {
        kind: String,
    },
    /// A finding's kind is not a lowercase slug.
    InvalidFindingKind {
        kind: String,
    },
    /// A finding status change its status does not allow.
    FindingNotInStatus {
        finding_id: FindingId,
        status: FindingStatus,
        to: FindingStatus,
    },
    /// Only a `blocked` ask may name a finding (ADR-0044 decision 23).
    AskFindingNotBlocked {
        kind: AskKind,
    },
    /// An ask of `kind` names neither a task nor a run; only `blocked` may.
    AskWithoutTarget {
        kind: AskKind,
    },
    /// An ask of the automatic update names a task (ADR-0073 decision 17):
    /// it is about the queue's binary.
    UpdateAskWithTarget {
        kind: AskKind,
    },
    /// A `queue_hold` ask without an authentication or cost reason, or
    /// another ask with one (ADR-0073 decision 22).
    AskKindReason {
        kind: AskKind,
        reason: AskReason,
    },
    /// An event on no task, goal or run whose kind is not a queue event
    /// (ADR-0073 decision 22).
    EventWithoutTarget {
        kind: String,
    },
    /// An authentication or cost ask registered like any other: those are
    /// one per queue, opened by the runtime (ADR-0047 decision 42).
    AskHoldsTheQueue {
        /// The reason it was registered with.
        reason: AskReason,
    },
    /// A queue-wide ask with a reason other than authentication or cost.
    HoldWithoutQueueReason {
        reason: AskReason,
    },
    /// A claim of a task that is not ready.
    TaskNotClaimable {
        task_id: TaskId,
        status: TaskStatus,
    },
    /// A new run for a task that is not in progress.
    RunOfUnclaimedTask {
        task_id: TaskId,
        status: TaskStatus,
    },
    /// The run command `operation` is not allowed from `status`.
    RunTransitionNotAllowed {
        status: RunStatus,
        operation: &'static str,
    },
    /// A stored run breaks an invariant every saved run keeps.
    RunInconsistent {
        run_id: RunId,
        reason: &'static str,
    },
    /// A `--paths` glob a task may not declare.
    InvalidPathGlob {
        glob: String,
        reason: &'static str,
    },
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownValue { kind, value } => write!(f, "unknown {kind}: {value}"),
            Self::TaskHasUnfinishedRun { action } => write!(
                f,
                "task has an unfinished run; recover or integrate it before applying {action:?}"
            ),
            Self::TransitionNotAllowed { status, action } => write!(
                f,
                "cannot apply {action:?} to task in {} state",
                status.as_str()
            ),
            Self::ReadyNeedsPlanReview { status } => write!(
                f,
                "a {} task becomes ready through plan review (submit it); \
                 pass --bypass-review to skip the review",
                status.as_str()
            ),
            Self::GoalClosed { goal_id, verdict } => write!(
                f,
                "goal {goal_id} is closed as {}; create a new goal for further work",
                verdict.map_or("?", GoalVerdict::as_str)
            ),
            Self::GoalCloseInconsistent { goal_id } => write!(
                f,
                "goal {goal_id} has a close time without a verdict or a verdict without a close time"
            ),
            Self::TaskNotEditable { what } => {
                write!(
                    f,
                    "{what} can only be changed for draft, submitted or ready tasks"
                )
            }
            Self::TaskContentNotEditable { task_id, status } => write!(
                f,
                "task {task_id} is {}; only a draft or submitted task can be edited",
                status.as_str()
            ),
            Self::SelfDependency => f.write_str("a task cannot depend on itself"),
            Self::DependencyCycle {
                task_id,
                predecessor_id,
            } => write!(
                f,
                "dependency {task_id} -> {predecessor_id} would create a cycle"
            ),
            Self::OwnGoalDependency { goal_id } => write!(
                f,
                "a task cannot depend on its own goal {goal_id}; the goal already waits for it"
            ),
            Self::GoalDependencyCycle { task_id, goal_id } => write!(
                f,
                "dependency {task_id} -> goal {goal_id} would create a cycle"
            ),
            Self::GoalMembershipCycle { task_id, goal_id } => write!(
                f,
                "moving task {task_id} to goal {goal_id} would create a cycle: the task already waits for the goal"
            ),
            Self::EmptyProposal => f.write_str("a proposal needs at least one draft task"),
            Self::SearchQuery { reason } => write!(f, "invalid search query: {reason}"),
            Self::TaskInOtherProposal {
                task_id,
                proposal_id,
            } => write!(
                f,
                "task {task_id} already belongs to proposal {proposal_id}"
            ),
            Self::GoalInOtherProposal {
                goal_id,
                proposal_id,
            } => write!(
                f,
                "goal {goal_id} already belongs to proposal {proposal_id}"
            ),
            Self::ProposalNotInStatus {
                proposal_id,
                status,
                expected,
            } => write!(
                f,
                "proposal {proposal_id} is {}, not {}",
                status.as_str(),
                expected.as_str()
            ),
            Self::ProposalNotActive {
                proposal_id,
                status,
            } => write!(
                f,
                "proposal {proposal_id} is {}; only a submitted or revising proposal is withdrawn",
                status.as_str()
            ),
            Self::Blank { field } => write!(f, "{field} must not be blank"),
            Self::InvalidPathGlob { glob, reason } => {
                write!(f, "invalid --paths glob {glob:?}: {reason}")
            }
            Self::AskWithoutTarget { kind } => write!(
                f,
                "a {} ask needs a task or a run; only a blocked ask may have neither",
                kind.as_str()
            ),
            Self::UpdateAskWithTarget { kind } => write!(
                f,
                "a {kind} ask is about the queue's binary and names no task or run"
            ),
            Self::AskKindReason { kind, reason } => write!(
                f,
                "a {kind} ask may not have the reason {}: only a queue_hold ask is for authentication or cost, and it always is",
                reason.as_str()
            ),
            Self::EventWithoutTarget { kind } => write!(
                f,
                "a {kind} event needs a task, a goal or a run; it is not an event of the queue itself"
            ),
            Self::AskHoldsTheQueue { reason } => write!(
                f,
                "authentication and cost asks are queue_hold asks the runtime opens, one per queue (ADR-0047 decision 42; this one is for {}); ask with --because scope, discard or recovery_failed, or leave a note",
                reason.as_str()
            ),
            Self::HoldWithoutQueueReason { reason } => write!(
                f,
                "a queue_hold ask is for authentication or cost, not {}",
                reason.as_str()
            ),
            Self::NonPositiveId { field } => write!(f, "{field} must be positive"),
            Self::GoalAlreadyClosed { goal_id, verdict } => write!(
                f,
                "goal {goal_id} is already closed as {}",
                verdict.map_or("?", GoalVerdict::as_str)
            ),
            Self::GoalCloseBlocked {
                goal_id,
                verdict,
                blocking,
            } => write!(
                f,
                "goal {goal_id} cannot be closed as {}: {}",
                verdict.as_str(),
                blocking
                    .iter()
                    .map(|(status, n)| format!("{n} task(s) {}", status.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::MalformedReceipt { reason } => {
                write!(f, "receipt is not a valid completion receipt: {reason}")
            }
            Self::ReceiptRunMismatch {
                receipt_run_id,
                run_id,
            } => write!(
                f,
                "receipt run_id {receipt_run_id} does not match run {run_id}"
            ),
            Self::AgentReportedResult { result, summary } => {
                write!(f, "agent reported result {}: {summary}", result.as_str())
            }
            Self::ReceiptCheckFailed {
                check,
                evidence_or_reason,
            } => write!(f, "receipt reports {check} as failed: {evidence_or_reason}"),
            Self::ReceiptCheckUnexplained { check, status } => write!(
                f,
                "receipt {check} is {} without evidence or reason",
                status.as_str()
            ),
            Self::InvalidCommit { field } => write!(
                f,
                "{field}: must be a full 40- or 64-character hexadecimal Git object ID"
            ),
            Self::FollowUpsNotArray => f.write_str("receipt follow_ups must be an array"),
            Self::MissingRunDirectory => f.write_str("missing run directory"),
            Self::TaskNotClaimable { task_id, status } => {
                write!(f, "task {task_id} is {}, not ready", status.as_str())
            }
            Self::RunOfUnclaimedTask { task_id, status } => write!(
                f,
                "task {task_id} is {}; a run starts only for a claimed task",
                status.as_str()
            ),
            Self::RunTransitionNotAllowed { status, operation } => {
                write!(f, "cannot {operation} a run in {} state", status.as_str())
            }
            Self::RunInconsistent { run_id, reason } => write!(f, "run {run_id} {reason}"),
            Self::GoalNotDraft { goal_id } => write!(f, "goal {goal_id} is not a draft"),
            Self::InvalidNoteKind { kind } => write!(
                f,
                "note kind {kind:?} must be a slug of lowercase letters, digits, '-' and '_'"
            ),
            Self::InvalidFindingKind { kind } => write!(
                f,
                "finding kind {kind:?} must be a slug of lowercase letters, digits, '-' and '_'"
            ),
            Self::FindingNotInStatus {
                finding_id,
                status,
                to,
            } => write!(
                f,
                "finding {finding_id} is {}; it cannot become {}",
                status.as_str(),
                to.as_str()
            ),
            Self::AskFindingNotBlocked { kind } => write!(
                f,
                "only a blocked ask may name a finding, not {}",
                kind.as_str()
            ),
        }
    }
}

impl std::error::Error for DomainError {}

/// Fails with `error()` unless `condition` holds.
pub(super) fn require(
    condition: bool,
    error: impl FnOnce() -> DomainError,
) -> Result<(), DomainError> {
    if condition { Ok(()) } else { Err(error()) }
}
