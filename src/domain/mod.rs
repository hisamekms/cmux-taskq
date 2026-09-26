use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = DomainError;
            fn from_str(value: &str) -> Result<Self, DomainError> {
                match value {
                    $($value => Ok(Self::$variant)),+,
                    _ => Err(DomainError::UnknownValue {
                        kind: stringify!($name),
                        value: value.to_owned(),
                    }),
                }
            }
        }
    };
}

// The known kinds of an open-ended kind column (ADR-0073 decision 21): like
// `string_enum!`, plus `Other` for a value a newer binary wrote. `FromStr`
// still rejects an unknown value, for what this binary is asked to write.
macro_rules! known_ask_kinds {
    ($name:ident { $($(#[$meta:meta])* $variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $($(#[$meta])* $variant,)+
            /// A kind this binary does not know, as stored.
            Other(String),
        }

        impl $name {
            /// The kind as stored: a known kind's name, or an unknown one
            /// verbatim.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Other(value) => value,
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = DomainError;
            fn from_str(value: &str) -> Result<Self, DomainError> {
                match value {
                    $($value => Ok(Self::$variant)),+,
                    _ => Err(DomainError::UnknownValue {
                        kind: stringify!($name),
                        value: value.to_owned(),
                    }),
                }
            }
        }
    };
}

// `submitted` waits for plan review (ADR-0041 decision 8): only plan review,
// a person's explicit bypass and a retry of the same task make a task
// `ready`.
string_enum!(TaskStatus {
    Draft => "draft",
    Submitted => "submitted",
    Ready => "ready",
    InProgress => "in_progress",
    Completed => "completed",
    Canceled => "canceled",
});

// What a task changes (goal 21, ADR-0029 decision 6): the kinds of change
// AGENTS.md pairs `--paths` and `--verify` for. `docs` is documents (an ADR
// included), `plugin` the plugin's skills and documents, `runtime` the
// crate (`src/`, `tests/`, `migrations/`), `ci` the scripts and CI
// configuration. A task registered before the kind existed has none.
string_enum!(TaskKind {
    Docs => "docs",
    Plugin => "plugin",
    Runtime => "runtime",
    Ci => "ci",
});

string_enum!(RunStatus {
    Claimed => "claimed",
    Starting => "starting",
    Running => "running",
    Validating => "validating",
    AwaitingIntegration => "awaiting_integration",
    Integrating => "integrating",
    NeedsSession => "needs_session",
    Integrated => "integrated",
    Succeeded => "succeeded",
    Failed => "failed",
    Interrupted => "interrupted",
});

string_enum!(Provider { Claude => "claude" });

// How `up` started a supervisor (ADR-0011). `Launchd` is the resident
// LaunchAgent; `InCmux` is the fallback that runs `supervise` inside the cmux
// workspace `[<repo>]supervisor`, which nothing restarts. A registration
// without a mode was started by hand.
string_enum!(SupervisorMode {
    Launchd => "launchd",
    InCmux => "in_cmux",
});

// The part a cmux workspace plays for a queue, carried in its `DAGQ_ROLE`
// environment variable and its description (ADR-0026). The five roles of
// ADR-0041 decision 1 are the supervisor, the worker, the planner, the
// inbox and the observer; `up` opens the inbox's workspace, and planners
// open on demand (`dagq plan`, or the runtime). `Observer` is the periodic
// job: it has no workspace, and the
// CLI refuses queue changes from its environment. `Reviewer` is the
// environment of the supervisor's headless review and triage jobs.
string_enum!(SessionRole {
    Supervisor => "supervisor",
    Worker => "worker",
    Planner => "planner",
    Inbox => "inbox",
    Observer => "observer",
    Reviewer => "reviewer",
});

// Whether a goal's tasks may run (ADR-0024 decision 5). A `draft` goal is a
// proposal, typically the observer's: its tasks are not candidates until
// `goal ready` opens it. Existing goals are `open`. Closing is independent
// and recorded in the verdict.
string_enum!(GoalStatus {
    Draft => "draft",
    Open => "open",
});

// How a goal was closed. Apart from draft/open, a goal has no state machine:
// it is open until one close records the verdict, and its progress derives
// from its tasks.
string_enum!(GoalVerdict {
    Achieved => "achieved",
    Abandoned => "abandoned",
});

// What an ask (ADR-0022) waits for a person to decide. Only questions that
// need an answer are asks; a notice is an attention. The kinds are not
// enumerated in the queue (ADR-0073 decision 19): a newer binary may write
// one this binary does not know, which reads as `Other` (decision 21).
known_ask_kinds!(AskKind {
    ApproveLanding => "approve_landing",
    AnswerPrompt => "answer_prompt",
    Decide => "decide",
    WorkerQuestion => "worker_question",
    // A threshold crossing the observer raises (ADR-0024 decision 4); the
    // one kind that may belong to no task.
    Blocked => "blocked",
    // A session that did not answer `/exit` within the exit timeout: the
    // supervisor asks the inbox to clear what holds it and send `/exit`,
    // and closes the ask itself once the session exits.
    StuckExit => "stuck_exit",
    // The retired follow-up triage's ask about a follow_up draft (ADR-0037):
    // no longer opened, kept so the asks it may have left still read. A
    // person acts on its answer.
    FollowUp => "follow_up",
    // A worker's session idle without a receipt past its one nudge
    // (ADR-0043 decision 1): the supervisor asks the inbox whether to wait
    // or step in, and closes the ask itself once the session moves on.
    Stalled => "stalled",
    // A plan review that needs a person (ADR-0041 decision 11): a `concern`,
    // or a proposal sent back too often. It belongs to the first task of
    // the proposal and no run, and the supervisor applies its answer.
    ApprovePlan => "approve_plan",
    // A planner of the runtime's that needs a person (ADR-0041 decision
    // 13): about the draft (or the proposal's task) it works on and no run.
    // The supervisor types the answer into that planner's workspace, as it
    // does a `worker_question`'s into a worker's.
    PlannerQuestion => "planner_question",
    // An authentication or cost ask (ADR-0047 decision 42): one open per
    // queue, reason and subject, about no task or run. The runs it holds
    // are its `affected`; a run that hits the same wall joins it.
    QueueHold => "queue_hold",
    // The automatic update of the fixed binary failed (ADR-0073 decisions
    // 13 and 17): about no task or run, one open at a time; the supervisor
    // applies its answer, one of [`UPDATE_FAILED_OPTIONS`].
    UpdateFailed => "update_failed",
    // A build of the automatic update brings a breaking migration and was
    // not installed (ADR-0073 decision 17): about no task or run, one open
    // at a time; a person installs it with the drain or leaves it.
    ApproveUpdate => "approve_update",
});

impl AskKind {
    /// The kind of a stored ask: never fails, since a newer binary may have
    /// written a kind this one does not know (ADR-0073 decision 21). The
    /// CLI parses with [`str::parse`], which accepts known kinds only
    /// (decision 20).
    pub fn read(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|_| Self::Other(value.to_owned()))
    }

    /// This binary knows the kind and may act on its answer; an `Other`
    /// ask is only shown and answered (ADR-0073 decision 21).
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }

    /// An ask of the automatic update (ADR-0073 decision 17): about the
    /// queue's binary, never a task or a run.
    pub fn is_update(&self) -> bool {
        matches!(self, Self::UpdateFailed | Self::ApproveUpdate)
    }
}

impl fmt::Display for AskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for AskKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AskKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::read(&String::deserialize(deserializer)?))
    }
}

// Why an ask needs a person (ADR-0047 decision 41). Every ask carries one;
// what fits none of them is no ask (a note, a receipt or a finding).
// Adding a value needs an ADR.
string_enum!(AskReason {
    // A login or an authentication that ran out.
    Authentication => "authentication",
    // Cost and resources: a usage limit, the free disk space.
    Cost => "cost",
    // Disagrees with or changes the acceptance, the scope, an ADR or a
    // goal's decision.
    Scope => "scope",
    // Whether to throw work away.
    Discard => "discard",
    // The recovery job (or another job) could not fix it, or was unsure.
    RecoveryFailed => "recovery_failed",
});

impl AskReason {
    /// The reasons of a [`AskKind::QueueHold`] ask, one per queue and
    /// subject; every other ask has one of the rest.
    pub const fn holds_the_queue(self) -> bool {
        matches!(self, Self::Authentication | Self::Cost)
    }
}

// Where a proposal (ADR-0041 decision 7) stands: `submitted` waits for plan
// review, `revising` was sent back to its planner (its tasks are drafts
// again), `accepted` passed and made its tasks ready, `canceled` ended
// without passing: a person's `cancel` answer canceled its tasks, or its
// planner withdrew it and its tasks returned to draft. Neither `accepted`
// nor `canceled` holds its members any more.
string_enum!(ProposalStatus {
    Submitted => "submitted",
    Revising => "revising",
    Accepted => "accepted",
    Canceled => "canceled",
});

// Who opened the planner that owns a proposal (ADR-0041 decisions 7, 13): a
// person with `dagq plan`, or the runtime (a revise whose planner closed, a
// follow_up). The two differ in where a question for a person goes.
string_enum!(PlannerOrigin {
    Person => "person",
    Runtime => "runtime",
});

// How a planner session stands (see [`PlannerSession::state`]): `opening`
// before its wrapper registers, `working` or `idle` while its agent runs,
// `exited` once the agent exited in a workspace still open, `lost` when its
// wrapper died or went silent without recording an exit, `closed` once its
// workspace is gone.
string_enum!(PlannerState {
    Opening => "opening",
    Working => "working",
    Idle => "idle",
    Exited => "exited",
    Lost => "lost",
    Closed => "closed",
});

// The verdict of the supervisor's headless review (ADR-0023 decision 2,
// ADR-0027 decision 2): `pass` lands the run, `revise` goes back to the live
// worker session, `concern` waits for a person in an `approve_landing` ask.
string_enum!(ReviewDecision {
    Pass => "pass",
    Revise => "revise",
    Concern => "concern",
});

/// What the headless review prints on stdout: one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewVerdict {
    pub verdict: ReviewDecision,
    pub reasons: Vec<String>,
    pub summary: String,
}

impl ReviewVerdict {
    /// The verdict in the review's stdout: the whole text, or else the
    /// outermost `{...}` in it (a model may wrap the object in a fence or
    /// a sentence).
    pub fn parse(stdout: &str) -> Result<Self, String> {
        parse_json_object(stdout)
            .map_err(|error| format!("the review printed no verdict JSON: {error}"))
    }
}

/// How many times the supervisor sends a `revise` verdict back to the live
/// session of one run; a later review that does not pass is a `concern`
/// (ADR-0027 decision 2).
pub const MAX_REVISE_ATTEMPTS: usize = 2;

/// The options of the `approve_landing` ask a `concern` opens, which the
/// supervisor acts on once answered (ADR-0027, ADR-0022 decision 3).
pub const LANDING_OPTIONS: &[&str] = &["land", "send_back", "cancel"];

/// The whole text as one JSON object of `T`, or else the outermost `{...}`
/// in it (a model may wrap the object in a fence or a sentence).
pub(crate) fn parse_json_object<T: serde::de::DeserializeOwned>(
    stdout: &str,
) -> serde_json::Result<T> {
    let text = stdout.trim();
    serde_json::from_str::<T>(text).or_else(|error| match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => serde_json::from_str::<T>(&text[start..=end]),
        _ => Err(error),
    })
}

/// The options of the `decide` ask the recovery job of a `failed` or
/// `interrupted` run escalates to, which the supervisor acts on once
/// answered: `retry` and `cancel` move the task, `resume` the run.
pub const TRIAGE_OPTIONS: &[&str] = &["retry", "resume", "cancel"];

/// A task with this many `failed` or `interrupted` runs, the recovered one
/// included, is not retried by the recovery job: its `retry` becomes an
/// ask, so a failure that repeats reaches a person.
pub const TRIAGE_RETRY_FAILURES: usize = 2;

/// Where the recovery of a `failed` or `interrupted` run stands, from the
/// latest of its `resume_started`, `triage_finished` and `triage_failed`
/// (the recovery job's rounds keep the triage's event names, ADR-0047
/// decision 40): a run resumed since its last round is taken again when it
/// fails again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageState {
    /// Not taken yet: the supervisor starts its recovery job.
    Pending,
    /// The job's `wait` holds it until `until` (unix seconds): then the
    /// job runs again if the run is still where it was.
    Waiting { until: i64 },
    /// The recovery job failed: a person recovers the run by hand.
    Failed,
    /// The verdict was acted on (or its ask waits for a person).
    Finished,
}

/// `triage_decided`'s `action` when a person chose one of the recovery
/// job's own options: the run goes back to the job, whose next round reads
/// the answer.
pub const RECOVER_AGAIN: &str = "recover";

/// The last of the events that move the review of a run awaiting
/// integration along (ADR-0027): where a supervisor that adopts the run
/// picks the review up, and whether its live session was asked to fix
/// something ([`fix_requested`]).
pub fn review_anchor(events: &[RunEvent]) -> Option<&RunEvent> {
    events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            "validation_finished"
                | "review_started"
                | "review_finished"
                | "revise_requested"
                | "revise_unsent"
                | "revise_finished"
                | "conflict_precheck"
                | "conflict_resolved"
        )
    })
}

/// Whether the live session of a run awaiting integration was sent a
/// `revise` verdict or a conflict request it has not answered yet
/// (ADR-0027 decisions 2 and 4), and not asked to `/exit` since (a
/// request it will not fix ends in the `/exit`). Its worker may stop at a
/// `worker_question` there, whose answer the supervisor types as it does a
/// running worker's (task 238).
pub fn fix_requested(events: &[RunEvent]) -> bool {
    review_anchor(events).is_some_and(|anchor| {
        (anchor.kind == "revise_requested"
            || (anchor.kind == "conflict_precheck" && anchor.payload["requested"] == true))
            && !events
                .iter()
                .any(|e| e.id > anchor.id && e.kind == "exit_requested")
    })
}

/// Whether a `needs_session` run's resumed session is on: its latest
/// `resume_started` has neither a `resume_finished` nor an `exit_requested`
/// after it (ADR-0071 decision 17).
pub fn resume_in_progress(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rposition(|e| e.kind == "resume_started")
        .is_some_and(|start| {
            !events[start + 1..]
                .iter()
                .any(|e| matches!(e.kind.as_str(), "resume_finished" | "exit_requested"))
        })
}

/// Whether the supervisor types the answer of a run's `worker_question`
/// into its worker's terminal: the run is `running`, awaiting integration
/// while its live session fixes what it was asked to ([`fix_requested`]),
/// or `needs_session` while its resumed session is on
/// ([`resume_in_progress`]). The caller checks that a supervisor leases it.
pub fn session_takes_answers(status: RunStatus, events: &[RunEvent]) -> bool {
    status == RunStatus::Running
        || (status == RunStatus::AwaitingIntegration && fix_requested(events))
        || (status == RunStatus::NeedsSession && resume_in_progress(events))
}

pub fn triage_state(events: &[RunEvent]) -> TriageState {
    let last = events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            "resume_started" | "triage_finished" | "triage_failed"
        ) || (e.kind == "triage_decided" && e.payload["action"] == RECOVER_AGAIN)
    });
    match last {
        Some(e) if e.kind == "triage_finished" && e.payload["action"] == "wait" => {
            TriageState::Waiting {
                until: e.payload["recheck_at"].as_i64().unwrap_or(0),
            }
        }
        Some(e) if e.kind == "triage_finished" => TriageState::Finished,
        Some(e) if e.kind == "triage_failed" => TriageState::Failed,
        _ => TriageState::Pending,
    }
}

string_enum!(ReceiptResult {
    Succeeded => "succeeded",
    Failed => "failed",
});

string_enum!(CheckStatus {
    Passed => "passed",
    Failed => "failed",
    NotApplicable => "not_applicable",
});

// A receipt check a task can demand evidence for (ADR-0019 decision 5): the
// names of the receipt's `tests`, `e2e` and `subagent_review`.
string_enum!(EvidenceCheck {
    Tests => "tests",
    E2e => "e2e",
    SubagentReview => "subagent_review",
});

/// How urgently a person wants a task claimed (ADR-0040 decision 4), lowest
/// first so the derived `Ord` is the claim order's first key. The CLI and
/// the JSON use the names only; the queue stores `low`=0 … `interrupt`=4.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Later: a conditional proposal and the like.
    Low,
    #[default]
    Normal,
    /// Soon: groundwork other work builds on.
    High,
    /// A defect that stops the operation.
    Urgent,
    /// Ahead of every other ready task.
    Interrupt,
}

impl Priority {
    const ALL: [Self; 5] = [
        Self::Low,
        Self::Normal,
        Self::High,
        Self::Urgent,
        Self::Interrupt,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
            Self::Interrupt => "interrupt",
        }
    }

    /// The stored value: `low`=0 … `interrupt`=4.
    pub const fn as_i64(self) -> i64 {
        self as i64
    }

    /// The level stored as `value`.
    pub fn from_i64(value: i64) -> Result<Self, DomainError> {
        usize::try_from(value)
            .ok()
            .and_then(|index| Self::ALL.get(index).copied())
            .ok_or_else(|| DomainError::UnknownValue {
                kind: "Priority",
                value: value.to_string(),
            })
    }
}

impl std::str::FromStr for Priority {
    type Err = DomainError;
    /// Only the names; a number is not a level.
    fn from_str(value: &str) -> Result<Self, DomainError> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == value)
            .ok_or_else(|| DomainError::UnknownValue {
                kind: "Priority",
                value: value.to_owned(),
            })
    }
}

pub mod claim_defer;
pub mod claim_hold;
mod error;
pub mod finding;
pub mod follow_up;
pub mod goal;
pub mod ids;
mod input;
pub mod kpi;
pub mod lint;
pub mod marks;
pub mod measure;
pub mod plan_quality;
pub mod plan_review;
pub mod planner;
pub mod proposal;
pub mod reason;
pub mod recheck;
pub mod recovery;
pub mod related;
pub mod resume;
pub mod run;
pub mod run_env;
pub mod scope;
pub mod search;
pub mod sessions;
pub mod stall;
pub mod stats;
pub mod task;
pub mod timeline;
pub mod tokens;
pub mod transcript;
mod views;
pub mod waiting;
pub mod worktime;

pub use error::DomainError;
use error::require;
pub use finding::{
    Finding, FindingOutcome, FindingQuery, FindingStatus, FindingTarget, FindingUpdate,
    FindingView, Impact, NewFinding,
};
pub use follow_up::{DraftOrigin, DraftTarget, MAX_DRAFT_PLANNERS, PLANNER_QUESTION_OPTIONS};
pub use goal::Goal;
pub use ids::{AskId, CommitSha, EventId, FindingId, GoalId, PlannerId, ProposalId, RunId, TaskId};
pub use input::{GoalEdit, GoalRecord, NewGoal, NewTask, RunPlan, RunRecord, TaskEdit, TaskRecord};
pub use lint::{LintCode, LintInput, LintNode, LintViolation};
pub use plan_review::{
    MAX_PLAN_REVISES, PLAN_OPTIONS, PLAN_REVIEW_ASKER, PlanAnswer, PlanReviewAction,
    PlanReviewCandidate, PlanReviewDecision, PlanReviewVerdict, Reopen, next_to_review,
};
pub use planner::{IdleProbe, PlannerProbe, PlannerSession};
pub use proposal::{PlannerOwner, Proposal, ProposalRecord, Submission};
pub use reason::{Reason, ReasonCode};
pub use run::TaskRun;
pub use task::{Task, TaskAction};
pub use views::{
    ClaimOutcome, EventFilter, GoalDetail, GoalPredecessor, GoalSummary, GoalTask,
    IntegrationOutcome, Predecessor, Receipt, ReceiptCheck, RegisteredFollowUp, RunEvent, RunLease,
    RunPaths, RunProcess, SupervisorRegistration, TaskDetail, TaskStatusCounts,
    evidence_missing_reason,
};

/// A question for a person (ADR-0022): about a task, or one of its runs when
/// `run_id` is set; a `blocked` ask of the observer may be about neither
/// (ADR-0024 decision 4). It is open while `answered_at` and `closed_at` are
/// unset; once answered it waits for the inbox, where the person acts on
/// the answer, to close it (or for the runtime to apply it). Times are unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub id: AskId,
    pub kind: AskKind,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub question: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    /// The role of the session that registered it (`DAGQ_ROLE`).
    pub asked_by: String,
    /// Why a person is needed (ADR-0047 decision 41).
    pub reason_category: AskReason,
    /// What a `queue_hold` ask is about within its reason (the usage limit
    /// or the disk for `cost`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The runs a `queue_hold` ask holds, in the order they joined it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected: Vec<String>,
    pub created_at: i64,
    pub answered_at: Option<i64>,
    pub closed_at: Option<i64>,
    /// The finding a `blocked` ask raises (ADR-0044 decision 23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding_id: Option<FindingId>,
    /// Who answered it (task 325): [`ANSWERED_BY_PERSON`], the `DAGQ_ROLE`
    /// of the session that ran `answer`, or [`ANSWERED_BY_RUNTIME`]. `None`
    /// while open, and for an answer recorded before it was kept.
    #[serde(default)]
    pub answered_by: Option<String>,
    /// The index of the option the answer chose ([`option_index`]); `None`
    /// for a free answer, an open ask, or an answer recorded before it was
    /// kept.
    #[serde(default)]
    pub option_index: Option<i64>,
}

/// `answered_by` of an answer from a terminal with no `DAGQ_ROLE`.
pub const ANSWERED_BY_PERSON: &str = "person";
/// `answered_by` of an answer the runtime wrote itself (a withdrawn,
/// superseded or runtime-closed ask).
pub const ANSWERED_BY_RUNTIME: &str = "runtime";

/// The 0-based index of the option `answer` chooses: the first option
/// equal to the trimmed answer. `None` for a free answer.
pub fn option_index(options: &[String], answer: &str) -> Option<i64> {
    let answer = answer.trim();
    options
        .iter()
        .position(|option| option.trim() == answer)
        .and_then(|index| i64::try_from(index).ok())
}

impl Ask {
    /// Nobody answered or withdrew it yet.
    pub fn is_open(&self) -> bool {
        self.answered_at.is_none() && self.closed_at.is_none()
    }

    /// The session role that acts on it now: the inbox, both to answer an
    /// open ask and to read an answer nobody closed (ADR-0024 decision 6).
    /// A closed ask waits for nobody.
    pub fn waits_for(&self) -> Option<SessionRole> {
        self.closed_at.is_none().then_some(SessionRole::Inbox)
    }
}

/// An ask to register: `task_id` or `run_id` names what it is about (a run
/// implies its task). Only a `blocked` ask may name neither.
#[derive(Debug, Clone)]
pub struct NewAsk {
    pub kind: AskKind,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub question: String,
    pub options: Vec<String>,
    pub asked_by: String,
    /// Why a person is needed (ADR-0047 decision 41): `scope`, `discard`
    /// or `recovery_failed`. Authentication and cost are [`NewHold`]s.
    pub reason_category: AskReason,
    /// The finding a `blocked` ask raises; the one-open-ask rule then holds
    /// per finding (ADR-0044 decision 23).
    pub finding_id: Option<FindingId>,
}

impl NewAsk {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.question.trim().is_empty(), || DomainError::Blank {
            field: "question",
        })?;
        require(self.options.iter().all(|o| !o.trim().is_empty()), || {
            DomainError::Blank { field: "options" }
        })?;
        require(!self.asked_by.trim().is_empty(), || DomainError::Blank {
            field: "asked_by",
        })?;
        require(self.task_id.is_none_or(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "task ID" }
        })?;
        require(
            self.kind != AskKind::QueueHold && !self.reason_category.holds_the_queue(),
            || DomainError::AskHoldsTheQueue {
                reason: self.reason_category,
            },
        )?;
        require(
            self.finding_id.is_none() || self.kind == AskKind::Blocked,
            || DomainError::AskFindingNotBlocked {
                kind: self.kind.clone(),
            },
        )?;
        require(
            self.task_id.is_some() || self.run_id.is_some() || self.kind == AskKind::Blocked,
            || DomainError::AskWithoutTarget {
                kind: self.kind.clone(),
            },
        )
    }
}

/// The rules an ask's kind binds, checked where an ask is written
/// (ADR-0073 decisions 20 and 22) since the queue no longer enumerates
/// kinds: only a known kind is written; an ask about no task is a
/// `blocked`, `queue_hold` or update ask about no run; an update ask
/// (`update_failed`, `approve_update`) is about no task; and a `queue_hold`
/// ask, and only it, is for authentication or cost. A reader does not
/// check them.
pub fn check_ask_kind(
    kind: &AskKind,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    reason: AskReason,
) -> Result<(), DomainError> {
    if let AskKind::Other(value) = kind {
        return Err(DomainError::UnknownValue {
            kind: "AskKind",
            value: value.clone(),
        });
    }
    require(
        task_id.is_some()
            || ((matches!(kind, AskKind::Blocked | AskKind::QueueHold) || kind.is_update())
                && run_id.is_none()),
        || DomainError::AskWithoutTarget { kind: kind.clone() },
    )?;
    require(!kind.is_update() || task_id.is_none(), || {
        DomainError::UpdateAskWithTarget { kind: kind.clone() }
    })?;
    require(
        (*kind == AskKind::QueueHold) == reason.holds_the_queue(),
        || DomainError::AskKindReason {
            kind: kind.clone(),
            reason,
        },
    )
}

/// An authentication or cost ask to open, or to add a run to
/// (ADR-0047 decision 42): at most one `queue_hold` ask is open per
/// reason and subject, and a run that hits the same wall joins its
/// `affected` instead of opening another.
#[derive(Debug, Clone)]
pub struct NewHold {
    /// `authentication` or `cost`.
    pub reason_category: AskReason,
    pub subject: Option<String>,
    /// The run that hit it.
    pub run_id: RunId,
    /// What the person is asked, without the list of runs: the ask's
    /// question ends with the runs it holds, rewritten as runs join.
    pub question: String,
    pub options: Vec<String>,
    pub asked_by: String,
}

impl NewHold {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.question.trim().is_empty(), || DomainError::Blank {
            field: "question",
        })?;
        require(self.options.iter().all(|o| !o.trim().is_empty()), || {
            DomainError::Blank { field: "options" }
        })?;
        require(!self.asked_by.trim().is_empty(), || DomainError::Blank {
            field: "asked_by",
        })?;
        require(self.reason_category.holds_the_queue(), || {
            DomainError::HoldWithoutQueueReason {
                reason: self.reason_category,
            }
        })
    }

    /// The question of the ask holding `affected`.
    pub fn question_for(question: &str, affected: &[String]) -> String {
        format!(
            "{question}\n\n{HOLD_AFFECTED_HEADING}{}",
            affected.join(", ")
        )
    }
}

/// Where the question of a `queue_hold` ask lists the runs it holds.
pub const HOLD_AFFECTED_HEADING: &str = "Affected runs: ";

/// The options of an authentication or usage-limit ask (ADR-0047
/// decision 42): `done` once the person logged in or the limit is back,
/// `cancel_affected` to throw the held runs away.
pub const HOLD_OPTIONS: &[&str] = &["done", "cancel_affected"];

/// The options of the [`AskKind::UpdateFailed`] ask: `retry` builds
/// main's head again at the supervisor's next check; `skip` waits for the
/// next landing that changes the runtime.
pub const UPDATE_FAILED_OPTIONS: &[&str] = &["retry", "skip"];
/// The options of the [`AskKind::ApproveUpdate`] ask: a person installs
/// the build with the drain (`install`, by running the command the question
/// names) or leaves it (`skip`).
pub const APPROVE_UPDATE_OPTIONS: &[&str] = &["install", "skip"];

/// What [`NewHold`] did: opened the ask (`created`), added the run to the
/// open one (`joined`), or found the run already in it (neither).
#[derive(Debug, Clone, Serialize)]
pub struct HoldOutcome {
    #[serde(flatten)]
    pub ask: Ask,
    pub created: bool,
    pub joined: bool,
}

/// What `ask` returns: the open ask of the same (task, run, kind) when one
/// exists (`created: false`), or the one just registered.
#[derive(Debug, Clone, Serialize)]
pub struct AskOutcome {
    #[serde(flatten)]
    pub ask: Ask,
    pub created: bool,
}

/// Run event kind of a note (ADR-0024 decision 4): a free-form observation
/// attached to a task, a run or a goal, with payload `{text, kind, by}`.
pub const OBSERVATION_KIND: &str = "observation";
/// `kind` of a note registered without one.
pub const DEFAULT_NOTE_KIND: &str = "note";

/// What a note is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteTarget {
    Task(TaskId),
    Run(RunId),
    Goal(GoalId),
}

/// A note to record as an `observation` run event.
#[derive(Debug, Clone)]
pub struct NewNote {
    pub target: NoteTarget,
    pub text: String,
    /// A lowercase slug classifying the note; [`DEFAULT_NOTE_KIND`] when absent.
    pub kind: Option<String>,
    /// `DAGQ_ROLE` of the writer, or `human`.
    pub by: String,
}

impl NewNote {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.text.trim().is_empty(), || DomainError::Blank {
            field: "note text",
        })?;
        if let Some(kind) = &self.kind {
            require(
                !kind.is_empty()
                    && kind.len() <= 64
                    && kind.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                    }),
                || DomainError::InvalidNoteKind { kind: kind.clone() },
            )?;
        }
        Ok(())
    }

    /// The payload of the `observation` event.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "text": self.text,
            "kind": self.kind.as_deref().unwrap_or(DEFAULT_NOTE_KIND),
            "by": self.by,
        })
    }
}

/// Which notes `notes` lists: past `since` (oldest first), or the latest
/// `limit` without it, narrowed to a goal (its own notes and those of its
/// tasks and their runs) and/or a task (its own and its runs').
#[derive(Debug, Clone, Default)]
pub struct NoteQuery {
    pub goal_id: Option<GoalId>,
    pub task_id: Option<TaskId>,
    pub since: Option<EventId>,
    pub limit: usize,
}

/// One page of `notes`, oldest first; `cursor` is the last note's event id
/// (or `since` when the page is empty), to pass back as `--since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotePage {
    pub notes: Vec<RunEvent>,
    pub cursor: EventId,
}
/// The remote `integrate` pushes the landed `main` to (ADR-0019 decision 3).
pub const PUSH_REMOTE: &str = "origin";

string_enum!(PushResult {
    Pushed => "pushed",
    Skipped => "skipped",
    Failed => "failed",
});

/// What became of the push after a landing: `pushed` (`push_finished`),
/// `skipped` with its `reason` (`--no-push` or no such remote,
/// `push_skipped`) or `failed` with Git's `error` (`push_failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushReport {
    pub outcome: PushResult,
    pub remote: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Default for PushReport {
    fn default() -> Self {
        Self {
            outcome: PushResult::Skipped,
            remote: PUSH_REMOTE.to_owned(),
            error: None,
            reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_priority_is_a_name_ordered_low_to_interrupt_and_stored_as_0_to_4() {
        let names = ["low", "normal", "high", "urgent", "interrupt"];
        let levels: Vec<Priority> = names.iter().map(|name| name.parse().unwrap()).collect();
        assert!(levels.windows(2).all(|pair| pair[0] < pair[1]));
        for (value, level) in levels.iter().enumerate() {
            assert_eq!(level.as_i64(), value as i64);
            assert_eq!(Priority::from_i64(value as i64).unwrap(), *level);
            assert_eq!(level.as_str(), names[value]);
            assert_eq!(serde_json::to_value(level).unwrap(), names[value]);
        }
        assert_eq!(Priority::default(), Priority::Normal);
        for bad in ["1", "Normal", "", "critical"] {
            assert_eq!(
                bad.parse::<Priority>().unwrap_err().to_string(),
                format!("unknown Priority: {bad}")
            );
        }
        for bad in [-1, 5] {
            assert!(Priority::from_i64(bad).is_err());
        }
    }

    #[test]
    fn a_note_needs_text_and_a_slug_kind() {
        let note = |text: &str, kind: Option<&str>| NewNote {
            target: NoteTarget::Goal(GoalId::new(1)),
            text: text.into(),
            kind: kind.map(Into::into),
            by: "human".into(),
        };
        note("x", None).validate().unwrap();
        note("x", Some("slow-land_2")).validate().unwrap();
        assert_eq!(
            note(" ", None).validate().unwrap_err().to_string(),
            "note text must not be blank"
        );
        for kind in ["", "Upper", "a b", &"k".repeat(65)] {
            assert!(
                matches!(
                    note("x", Some(kind)).validate(),
                    Err(DomainError::InvalidNoteKind { .. })
                ),
                "{kind}"
            );
        }
        assert_eq!(
            note("x", None).payload(),
            serde_json::json!({"text": "x", "kind": "note", "by": "human"})
        );
    }

    #[test]
    fn rejections_carry_their_facts_and_keep_the_cli_messages() {
        assert_eq!(
            "bogus".parse::<TaskStatus>(),
            Err(DomainError::UnknownValue {
                kind: "TaskStatus",
                value: "bogus".into()
            })
        );
        assert_eq!(
            "x".parse::<GoalVerdict>().unwrap_err().to_string(),
            "unknown GoalVerdict: x"
        );
        let error = TaskStatus::Completed
            .transition(TaskAction::Ready, false)
            .unwrap_err();
        assert_eq!(
            error,
            DomainError::TransitionNotAllowed {
                status: TaskStatus::Completed,
                action: TaskAction::Ready
            }
        );
        assert_eq!(
            error.to_string(),
            "cannot apply Ready to task in completed state"
        );
        assert_eq!(
            TaskStatus::InProgress
                .transition(TaskAction::Cancel, true)
                .unwrap_err()
                .to_string(),
            "task has an unfinished run; recover or integrate it before applying Cancel"
        );
        let task = NewTask {
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: vec![TaskId::new(0)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        };
        assert_eq!(
            task.validate().unwrap_err().to_string(),
            "dependency IDs must be positive"
        );
        assert_eq!(
            CommitSha::parse("abc", "base commit")
                .unwrap_err()
                .to_string(),
            "base commit: must be a full 40- or 64-character hexadecimal Git object ID"
        );
        assert!(
            Receipt::parse("{}")
                .unwrap_err()
                .to_string()
                .starts_with("receipt is not a valid completion receipt: ")
        );
    }

    #[test]
    fn missing_evidence_is_a_required_check_not_passed_with_evidence() {
        let check = |status: &str, evidence: &str| serde_json::json!({"status": status, "evidence_or_reason": evidence});
        let receipt: Receipt = serde_json::from_value(serde_json::json!({
            "run_id": "r",
            "result": "succeeded",
            "commit": "0".repeat(40),
            "tests": check("passed", "ran"),
            "e2e": check("not_applicable", "no surface"),
            "subagent_review": check("passed", " "),
        }))
        .unwrap();
        assert!(receipt.missing_evidence(&[]).is_empty());
        assert!(receipt.missing_evidence(&[EvidenceCheck::Tests]).is_empty());
        // A blank or failed check fails the receipt unless it is required;
        // a required one is left to missing_evidence.
        assert!(receipt.check(&RunId::new("r").unwrap()).is_err());
        assert!(
            receipt
                .check_requiring(&RunId::new("r").unwrap(), &[EvidenceCheck::SubagentReview])
                .is_ok()
        );
        let mut failed = receipt.clone();
        failed.subagent_review.evidence_or_reason = "reviewed".into();
        failed.e2e.status = CheckStatus::Failed;
        assert_eq!(
            failed
                .check(&RunId::new("r").unwrap())
                .unwrap_err()
                .to_string(),
            "receipt reports e2e as failed: no surface"
        );
        assert!(
            failed
                .check_requiring(&RunId::new("r").unwrap(), &[EvidenceCheck::Tests])
                .is_err()
        );
        assert!(
            failed
                .check_requiring(&RunId::new("r").unwrap(), &[EvidenceCheck::E2e])
                .is_ok()
        );
        assert_eq!(
            failed.missing_evidence(&[EvidenceCheck::E2e]),
            [EvidenceCheck::E2e]
        );
        let missing = receipt.missing_evidence(&[
            EvidenceCheck::SubagentReview,
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
        ]);
        assert_eq!(missing, [EvidenceCheck::SubagentReview, EvidenceCheck::E2e]);
        assert_eq!(
            evidence_missing_reason(&missing),
            "evidence missing: subagent_review, e2e"
        );
        let task = NewTask {
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: vec![EvidenceCheck::E2e, EvidenceCheck::Tests, EvidenceCheck::E2e],
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        };
        assert_eq!(
            task.required_evidence(),
            [EvidenceCheck::E2e, EvidenceCheck::Tests]
        );
    }
}

/// A process whose heartbeat is older than this has no working process behind
/// it, whatever its PID says: the rule for leases, wrappers and supervisors.
pub const HEARTBEAT_TIMEOUT_SECS: i64 = 30;

/// What a person (through the inbox) does about an attention (ADR-0016). The
/// values are short fixed phrases, part of the public contract of `status`,
/// `events` and `watch`. A `needs_session` run the supervisor stopped
/// resuming and a dialog a session waits at are asks now (ADR-0024's
/// Consequences), so neither has a value of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionNext {
    ReviewAndIntegrate,
    RestartSupervisor,
    PushMain,
    RecoverRun,
    /// Not a person's to act on: the supervisor resumes the
    /// `needs_session` run itself (ADR-0019 decision 1), or hands it to a
    /// person as an ask once its resumes are used up.
    Resuming,
    AnswerAsk {
        ask_id: AskId,
    },
    ReadAnswer {
        ask_id: AskId,
    },
    /// Not a person's to act on: the supervisor types the answer of a
    /// `worker_question` into the worker's terminal once the worker is idle.
    DeliveringAnswer {
        ask_id: AskId,
    },
    /// The supervisor could not type the answer of a `worker_question` into
    /// the worker's terminal (it tries once), or the worker's session is gone.
    DeliverAnswer {
        ask_id: AskId,
    },
    /// Not a person's to act on: the supervisor holds the accepted run
    /// for its headless review and what follows from the verdict (ADR-0027).
    Reviewing,
    /// The headless review failed (`review_failed`): a person reviews the
    /// run and calls `integrate` by hand.
    ReviewByHand,
    /// Not a person's to act on: the supervisor lands, sends back or
    /// cancels the run as the answer of its `approve_landing` ask says.
    ApplyingAnswer {
        ask_id: AskId,
    },
    /// Not a person's to act on: the supervisor triages the `failed`
    /// or `interrupted` run and acts on the verdict (ADR-0024 decision 3).
    Triaging,
    /// The headless triage failed (`triage_failed`): a person decides
    /// whether to `ready` the task again, resume or cancel.
    TriageByHand,
    /// The recovery job of a live session's alert failed
    /// (`recovery_failed`, ADR-0047 decision 40): a person looks at the
    /// session and recovers it by hand, until the session moves on.
    RecoverByHand,
    /// The headless plan review of a proposal failed
    /// (`plan_review_failed`): a person readies its tasks with the bypass
    /// or has a planner fix and submit them again (ADR-0041 decision 17).
    PlanReviewByHand,
    /// The planner a revise went to did not submit its proposal again
    /// within the planner timeout (`planner_unresponsive`, ADR-0041
    /// decision 13): a person looks at its workspace.
    CheckPlanner,
    /// The runtime opened its planners for a draft the runtime or a job
    /// registered, and none decided it (`draft_planner_exhausted`,
    /// ADR-0041 decision 16): a person decides it in a planner of theirs.
    DecideDraft,
    /// A program `[run.env]` names is not found on the supervisor's PATH
    /// (`run_env_program_missing`, ADR-0049 decision 9): a person installs
    /// it or has a task take it out of `dagq.toml`; the supervisor claims
    /// nothing and lands nothing until then.
    InstallTool,
    /// The automatic update put a new binary in place of the supervisor's
    /// (`update_installed`, ADR-0073 decision 17): a notice the inbox
    /// passes on to the person, who acts on nothing.
    ReportUpdate,
}

/// How many times the supervisor resumes one `needs_session` run (one
/// `resume_started` each) before it leaves the run to a human (ADR-0019).
pub const MAX_RESUME_ATTEMPTS: usize = 3;

impl fmt::Display for AttentionNext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReviewAndIntegrate => f.write_str("review and integrate"),
            Self::RestartSupervisor => f.write_str("restart supervisor"),
            Self::PushMain => f.write_str("push main"),
            Self::RecoverRun => f.write_str("recover run"),
            Self::Resuming => f.write_str("resuming (runtime)"),
            Self::AnswerAsk { ask_id } => write!(f, "answer ask {ask_id}"),
            Self::ReadAnswer { ask_id } => {
                write!(f, "read the answer of ask {ask_id} and close it")
            }
            Self::DeliveringAnswer { ask_id } => {
                write!(f, "delivering the answer of ask {ask_id} (runtime)")
            }
            Self::DeliverAnswer { ask_id } => {
                write!(
                    f,
                    "send the answer of ask {ask_id} to the worker and close it"
                )
            }
            Self::Reviewing => f.write_str("reviewing (runtime)"),
            Self::ReviewByHand => f.write_str("review by hand"),
            Self::ApplyingAnswer { ask_id } => {
                write!(f, "applying the answer of ask {ask_id} (runtime)")
            }
            Self::Triaging => f.write_str("triaging (runtime)"),
            Self::TriageByHand => f.write_str("triage by hand"),
            Self::RecoverByHand => f.write_str("recover by hand"),
            Self::PlanReviewByHand => f.write_str("plan review by hand"),
            Self::CheckPlanner => f.write_str("check the planner"),
            Self::DecideDraft => f.write_str("decide the draft in a planner"),
            Self::InstallTool => f.write_str("install tool"),
            Self::ReportUpdate => f.write_str("report the update"),
        }
    }
}

impl Serialize for AttentionNext {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The `run_events` kinds that can mark an attention. The kind names are a
/// public contract (ADR-0016); whether one of these events is an attention
/// also depends on its payload, see [`event_attention`].
pub const ATTENTION_KINDS: &[&str] = &[
    "validation_finished",
    "supervision_finished",
    "integration_deferred",
    "integration_failed",
    "integration_error",
    "push_failed",
    "runtime_error",
    "resume_finished",
    "review_failed",
    "triage_failed",
    "recovery_failed",
    "plan_review_failed",
    "planner_unresponsive",
    "draft_planner_exhausted",
    run_env::RUN_ENV_PROGRAM_MISSING,
    UPDATE_INSTALLED,
    "ask_opened",
    "ask_answered",
    "ask_delivery_failed",
];

/// The steps of the automatic update of the fixed binary (ADR-0073
/// decision 17), queue events in `run_events` (task 496): the supervisor
/// started the job for a commit (`pid`, `base`, `supervisor`, `version`,
/// the logs).
pub const UPDATE_STARTED: &str = "update_started";
/// The job built the commit (`pid`, `binary`, `log`).
pub const UPDATE_BUILT: &str = "update_built";
/// The supervisor runs the new binary (`version`, `previous_version`,
/// `migrated`, `supervisors`).
pub const UPDATE_INSTALLED: &str = "update_installed";
/// The build, its check, the install or the watch failed, or the job died
/// (`stage`, `error`, `restored`, `supervisor`, `ask_id` of the
/// `update_failed` ask).
pub const UPDATE_FAILED: &str = "update_failed";
/// The job put the replaced binary back after a failed watch (`version`,
/// `restored_version`): the rollback, before its `update_failed`.
pub const UPDATE_RESTORED: &str = "update_restored";
/// The build brings a breaking migration and waits for a person
/// (`version`, `migrations`, `binary`, `command`, `ask_id` of the
/// `approve_update` ask).
pub const UPDATE_AWAITING_APPROVAL: &str = "update_awaiting_approval";
/// The supervisor applied a `skip` answer of an `update_failed` ask
/// (`ask_id`, `answer`).
pub const UPDATE_ANSWERED: &str = "update_answered";
/// A `retry` answer: the next check builds main's head again.
pub const UPDATE_RETRY: &str = "update_retry";

/// Every step of the automatic update, each with `commit` (the main commit
/// it is about) in its payload but for the answers.
pub const UPDATE_EVENT_KINDS: &[&str] = &[
    UPDATE_STARTED,
    UPDATE_BUILT,
    UPDATE_INSTALLED,
    UPDATE_FAILED,
    UPDATE_RESTORED,
    UPDATE_AWAITING_APPROVAL,
    UPDATE_ANSWERED,
    UPDATE_RETRY,
];

/// The event kinds that may belong to no task, goal or run: the queue's
/// own events. The queue enumerated them in a CHECK until migration 0039;
/// the write port checks them now (ADR-0073 decision 22).
pub const QUEUE_EVENT_KINDS: &[&str] = &[
    "backend_call_failed",
    "observe_started",
    "observe_finished",
    "ask_opened",
    "ask_answered",
    "ask_closed",
    "stall_config_loaded",
    "finding_recorded",
    "finding_updated",
    "finding_status_changed",
    "run_env_program_missing",
    "run_env_program_found",
    "session_opened",
    "session_closed",
    "session_turns",
    "supervisor_started",
    "supervisor_stopped",
    "run_env_changed",
    "mark_recorded",
    "mark_retracted",
    claim_hold::CLAIM_HELD,
    claim_hold::CLAIM_RESUMED,
    UPDATE_STARTED,
    UPDATE_BUILT,
    UPDATE_INSTALLED,
    UPDATE_FAILED,
    UPDATE_RESTORED,
    UPDATE_AWAITING_APPROVAL,
    UPDATE_ANSWERED,
    UPDATE_RETRY,
];

/// Whether an event of `kind` may be written with its task, goal and run
/// (ADR-0073 decision 22): one on none of them is a queue event. A reader
/// does not check it.
pub fn check_event_target(
    kind: &str,
    task_id: Option<TaskId>,
    goal_id: Option<GoalId>,
) -> Result<(), DomainError> {
    require(
        task_id.is_some() || goal_id.is_some() || QUEUE_EVENT_KINDS.contains(&kind),
        || DomainError::EventWithoutTarget {
            kind: kind.to_owned(),
        },
    )
}

/// The attention kinds an ask writes (ADR-0022): about the ask, even when it
/// names a run.
pub const ASK_EVENT_KINDS: &[&str] = &[
    "ask_opened",
    "ask_answered",
    "ask_delivered",
    "ask_delivery_failed",
    "ask_closed",
];

/// Whether a run event is a transition that stops at a person's judgment,
/// and what to do about it. The run comes to rest in
/// `status` (`awaiting_integration`, `needs_session`, `failed`), or the
/// session did not answer `/exit`. `integration_error` back to
/// `awaiting_integration` is not one: the `integrate` caller got the error.
/// `exit_request_timed_out` is not one: the supervisor raises it as a
/// `stuck_exit` ask, whose `ask_opened` is the attention, and an
/// `ask_answered` the runtime wrote when it closed such an ask itself
/// (`runtime_closed: true`) is none either.
/// `integration_rebase_aborted` is not one either: the landing goes on and
/// its outcome is its own event. A `runtime_error` is one only when the
/// supervisor released the run's lease with it (`lease_released: true`, the
/// abandon): nothing moves the run on until it is recovered. A
/// `runtime_error` recorded without releasing the lease is a note.
/// `prompt_waiting` is not one: the supervisor raises the dialog as an
/// `answer_prompt` ask, whose `ask_opened` is the attention. An
/// `integration_deferred` or `integration_error` into `needs_session` is
/// not one, and neither is the last `resume_finished` that leaves the run
/// `needs_session` (`exhausted`): the supervisor resumes the run, and once
/// its resumes are used up hands it to a person with a `decide` ask
/// (ADR-0024's Consequences). `resume_finished` is one when the resume put
/// the run where a person decides (`awaiting_integration` for an unapproved
/// run); a resolved run the supervisor goes on to land is not. A run that became `failed` (by validation, the session's
/// exit, a landing or a resume) is no attention either: the supervisor
/// triages it (ADR-0024 decision 3), and only `triage_failed` is one.
/// `validation_finished` into `awaiting_integration` is not one: the
/// supervisor reviews the run (ADR-0027); `review_failed` is, since the
/// run then waits for a review by hand, unless it carries the `ask_id` of
/// the `approve_landing` ask opened with it (task 328), whose
/// `ask_opened` is the attention.
/// `ask_opened` waits for the inbox's answer and
/// `ask_answered` for the person to act on it through the inbox,
/// except the answer of a `worker_question`, which the supervisor types into
/// the worker's terminal itself (`runtime_delivers: true`); its answer to a
/// run no longer running and its `ask_delivery_failed` are the inbox's.
/// The answer of an `approve_landing` ask the supervisor applies
/// (`runtime_delivers: true`: one of [`LANDING_OPTIONS`] for a run awaiting
/// integration) is not one either, nor that of the recovery job's `decide`
/// ask (`runtime_delivers: true`: one of the ask's options, [`TRIAGE_OPTIONS`]
/// or the job's own, for a `failed` or `interrupted` run).
pub fn event_attention(kind: &str, payload: &serde_json::Value) -> Option<AttentionNext> {
    let status = payload
        .get("status")
        .and_then(serde_json::Value::as_str)
        .and_then(|status| status.parse::<RunStatus>().ok());
    match (kind, status) {
        // The supervisor that validated the run reviews it and acts on the
        // verdict itself (ADR-0023 decision 2, ADR-0027).
        ("validation_finished", Some(RunStatus::AwaitingIntegration)) => None,
        // A failed review whose `approve_landing` ask was opened with it
        // waits in that ask (task 328); one without an ask is reviewed by
        // hand.
        ("review_failed", _) if payload.get("ask_id").is_some() => None,
        ("review_failed", _) => Some(AttentionNext::ReviewByHand),
        // The supervisor triages a failed run and acts on the verdict
        // (ADR-0024 decision 3); only a triage that failed is a person's.
        ("triage_failed", _) => Some(AttentionNext::TriageByHand),
        // The recovery job of a live session's alert failed (ADR-0047
        // decision 40): the session is a person's to recover by hand.
        ("recovery_failed", _) => Some(AttentionNext::RecoverByHand),
        // The supervisor reviews a submitted proposal and acts on the
        // verdict (ADR-0041 decision 11); only a plan review that failed
        // and a planner that did not answer a revise are a person's.
        ("plan_review_failed", _) => Some(AttentionNext::PlanReviewByHand),
        ("planner_unresponsive", _) => Some(AttentionNext::CheckPlanner),
        ("draft_planner_exhausted", _) => Some(AttentionNext::DecideDraft),
        ("push_failed", _) => Some(AttentionNext::PushMain),
        (run_env::RUN_ENV_PROGRAM_MISSING, _) => Some(AttentionNext::InstallTool),
        // The failure and the breaking build of the automatic update reach
        // the inbox as their asks; only the replaced binary is a notice.
        (UPDATE_INSTALLED, _) => Some(AttentionNext::ReportUpdate),
        ("runtime_error", _)
            if payload.get("lease_released") == Some(&serde_json::Value::Bool(true)) =>
        {
            Some(AttentionNext::RecoverRun)
        }
        ("resume_finished", Some(RunStatus::AwaitingIntegration)) => {
            Some(AttentionNext::ReviewAndIntegrate)
        }
        ("ask_opened", _) => ask_id(payload).map(|ask_id| AttentionNext::AnswerAsk { ask_id }),
        ("ask_answered", _)
            if payload.get("runtime_closed") == Some(&serde_json::Value::Bool(true)) =>
        {
            None
        }
        // The supervisor types the answer of a `worker_question` into the
        // worker's terminal, and that of a `planner_question` into the
        // planner's workspace (ADR-0041 decision 13).
        ("ask_answered", _)
            if matches!(
                payload.get("kind").and_then(serde_json::Value::as_str),
                Some(kind) if kind == AskKind::WorkerQuestion.as_str()
                    || kind == AskKind::PlannerQuestion.as_str()
            ) =>
        {
            match payload.get("runtime_delivers") {
                Some(serde_json::Value::Bool(false)) => {
                    ask_id(payload).map(|ask_id| AttentionNext::DeliverAnswer { ask_id })
                }
                _ => None,
            }
        }
        // An answer the supervisor applies itself: an `approve_landing` one,
        // a triage's `decide` one, a plan review's `approve_plan` one, or
        // that of the automatic update's failure (`update_failed`).
        ("ask_answered", _)
            if matches!(
                payload.get("kind").and_then(serde_json::Value::as_str),
                Some(kind) if kind == AskKind::ApproveLanding.as_str()
                    || kind == AskKind::Decide.as_str()
                    || kind == AskKind::ApprovePlan.as_str()
                    || kind == AskKind::UpdateFailed.as_str()
            ) && payload.get("runtime_delivers") == Some(&serde_json::Value::Bool(true)) =>
        {
            None
        }
        ("ask_answered", _) => ask_id(payload).map(|ask_id| AttentionNext::ReadAnswer { ask_id }),
        ("ask_delivery_failed", _) => {
            ask_id(payload).map(|ask_id| AttentionNext::DeliverAnswer { ask_id })
        }
        _ => None,
    }
}

fn ask_id(payload: &serde_json::Value) -> Option<AskId> {
    payload
        .get("ask_id")
        .and_then(serde_json::Value::as_i64)
        .map(AskId::new)
}

/// The session role attention is addressed to: every attention, the
/// supervisors' health included, is the inbox's, where a person sees it
/// (ADR-0024 decision 6). No attention is the planner's.
pub const ATTENTION_ROLE: SessionRole = SessionRole::Inbox;

/// Whether a run in `status` waits for a person or the supervisor now.
/// `exit_pending` is a run whose `/exit` request timed out with no session exit since: a
/// `running` one, or one the supervisor still holds after its validation
/// or review (ADR-0027). It is no attention of the run's, since its
/// `stuck_exit` ask is (and a dialog seen before the timeout is part of
/// that ask). `push_pending` is the `integrated` run whose push of `main`
/// failed with no successful push since, which the task being completed does
/// not end. `leased` is whether the run has a lease row, stale or not: an
/// unfinished run without one was given up by its owner (the supervisor's
/// abandon), and neither adoption, which takes only stale leases, nor
/// anything else moves it on until it is recovered. A stale lease is the
/// supervisor's attention, not the run's. A dialog a `running` run waits at
/// is no attention of the run's either: its `answer_prompt` ask is.
/// A `needs_session` run is the supervisor's in every case (ADR-0019
/// decision 1, ADR-0024's Consequences): it resumes the run, waits for a
/// session still alive to end first, or, with the resumes used up, hands
/// the run to a person as a `decide` ask. Nobody opens a session of their
/// own for it. An `awaiting_integration` run with a
/// lease is the supervisor's review (ADR-0027); without one it waits for a
/// person (a failed review, or a run validated before the review existed). The caller passes only the latest run of an `in_progress` task, so a
/// failed run stops counting once the task is retried or canceled.
pub fn run_attention(
    status: RunStatus,
    exit_pending: bool,
    push_pending: bool,
    leased: bool,
) -> Option<AttentionNext> {
    match status {
        RunStatus::Integrated if push_pending => Some(AttentionNext::PushMain),
        RunStatus::Claimed
        | RunStatus::Starting
        | RunStatus::Running
        | RunStatus::Validating
        | RunStatus::Integrating
            if !leased =>
        {
            Some(AttentionNext::RecoverRun)
        }
        // The supervisor asked the session to exit after the review's
        // verdict (or a failed validation) and waits for it (ADR-0027); its
        // `stuck_exit` ask is the attention, as for a running run.
        RunStatus::AwaitingIntegration | RunStatus::NeedsSession | RunStatus::Failed
            if exit_pending && leased =>
        {
            None
        }
        RunStatus::AwaitingIntegration if leased => Some(AttentionNext::Reviewing),
        RunStatus::AwaitingIntegration => Some(AttentionNext::ReviewAndIntegrate),
        RunStatus::NeedsSession => Some(AttentionNext::Resuming),
        // The caller tells a triage that failed or finished apart by the
        // run's events ([`triage_state`]); by the status alone, the
        // supervisor triages the run.
        RunStatus::Failed | RunStatus::Interrupted => Some(AttentionNext::Triaging),
        _ => None,
    }
}

/// One thing that waits for a person: a run (`run_id`,
/// `task_id`) or a supervisor (`pid`, or neither when none is registered).
/// `kind` is the run event that brought the run there, or
/// `supervisor_stale` / `supervisor_stopped`, which are derived from the
/// `supervisors` table and never written to `run_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attention {
    pub run_id: Option<RunId>,
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The ask of an `ask_opened` / `ask_answered` attention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<AskId>,
    /// Why that ask needs a person (ADR-0047 decision 41).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_category: Option<AskReason>,
    pub status: String,
    pub kind: String,
    pub last_error: Option<String>,
    /// The code of `last_error` (ADR-0034), when the event that set it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<ReasonCode>,
    pub next: AttentionNext,
}

/// Whether a process no longer works: its PID is dead or its heartbeat is
/// older than [`HEARTBEAT_TIMEOUT_SECS`].
pub fn heartbeat_stale(alive: bool, heartbeat_age_secs: i64) -> bool {
    !alive || heartbeat_age_secs > HEARTBEAT_TIMEOUT_SECS
}

/// The health of one registered supervisor that `watch` compares: a change
/// in the set of tokens, a PID, `alive` or `stale` wakes the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupervisorPulse {
    pub token: String,
    pub pid: u32,
    pub alive: bool,
    pub stale: bool,
}

impl SupervisorPulse {
    pub fn judge(registration: &SupervisorRegistration, alive: bool, now: i64) -> Self {
        Self {
            token: registration.token.clone(),
            pid: registration.pid,
            alive,
            stale: heartbeat_stale(alive, now - registration.heartbeat_at),
        }
    }
}

/// Supervisors that need a restart: every stale registration, or a queue
/// with no registration at all (stopped, or never started).
pub fn supervisor_attention(pulses: &[SupervisorPulse]) -> Vec<Attention> {
    let restart = |pid, status: &str, kind: &str| Attention {
        run_id: None,
        task_id: None,
        pid,
        ask_id: None,
        reason_category: None,
        status: status.into(),
        kind: kind.into(),
        last_error: None,
        last_error_code: None,
        next: AttentionNext::RestartSupervisor,
    };
    if pulses.is_empty() {
        return vec![restart(None, "stopped", "supervisor_stopped")];
    }
    pulses
        .iter()
        .filter(|pulse| pulse.stale)
        .map(|pulse| {
            let status = if pulse.alive { "stale" } else { "dead" };
            restart(Some(pulse.pid), status, "supervisor_stale")
        })
        .collect()
}

#[cfg(test)]
mod attention_tests {
    use super::*;
    use serde_json::json;

    /// The answer chooses the first option equal to it once trimmed; any
    /// other answer is free.
    #[test]
    fn an_answer_chooses_the_option_it_equals() {
        let options = vec!["land".to_owned(), " send_back ".to_owned()];
        assert_eq!(option_index(&options, "land"), Some(0));
        assert_eq!(option_index(&options, "send_back\n"), Some(1));
        assert_eq!(option_index(&options, "land it"), None);
        assert_eq!(option_index(&[], "land"), None);
    }

    #[test]
    fn asks_wait_for_the_inbox_until_closed() {
        let mut ask = Ask {
            id: AskId::new(1),
            kind: AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "q".into(),
            options: vec![],
            answer: None,
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            subject: None,
            affected: Vec::new(),
            created_at: 0,
            answered_at: None,
            answered_by: None,
            option_index: None,
            closed_at: None,
            finding_id: None,
        };
        assert!(ask.is_open());
        assert_eq!(ask.waits_for(), Some(SessionRole::Inbox));
        ask.answer = Some("a".into());
        ask.answered_at = Some(1);
        assert!(!ask.is_open());
        assert_eq!(ask.waits_for(), Some(SessionRole::Inbox));
        ask.closed_at = Some(2);
        assert_eq!(ask.waits_for(), None);
        assert_eq!(ATTENTION_ROLE, SessionRole::Inbox);
        assert_eq!(
            AttentionNext::ReadAnswer {
                ask_id: AskId::new(4)
            }
            .to_string(),
            "read the answer of ask 4 and close it"
        );
    }

    #[test]
    fn unknown_ask_kinds_are_read_but_not_parsed_or_written() {
        // ADR-0073 decisions 20 and 21.
        let later = AskKind::read("later_kind");
        assert_eq!(later, AskKind::Other("later_kind".into()));
        assert!(!later.is_known() && AskKind::read("decide").is_known());
        assert_eq!(later.as_str(), "later_kind");
        assert_eq!(later.to_string(), "later_kind");
        assert_eq!(json!(later), json!("later_kind"));
        assert_eq!(
            serde_json::from_value::<AskKind>(json!("stalled")).unwrap(),
            AskKind::Stalled
        );
        assert_eq!(
            serde_json::from_value::<AskKind>(json!("later_kind")).unwrap(),
            later
        );
        assert!("later_kind".parse::<AskKind>().is_err());

        let task = Some(TaskId::new(1));
        let run = RunId::new("run-1").unwrap();
        let scope = AskReason::Scope;
        assert_eq!(
            check_ask_kind(&later, task, None, scope)
                .unwrap_err()
                .to_string(),
            "unknown AskKind: later_kind"
        );
        assert!(check_ask_kind(&AskKind::Decide, task, Some(&run), scope).is_ok());
        assert!(check_ask_kind(&AskKind::Blocked, None, None, scope).is_ok());
        for (kind, run) in [
            (AskKind::Decide, None),
            (AskKind::Blocked, Some(&run)),
            (AskKind::QueueHold, Some(&run)),
        ] {
            assert!(matches!(
                check_ask_kind(&kind, None, run, scope),
                Err(DomainError::AskWithoutTarget { .. })
            ));
        }
        assert!(check_ask_kind(&AskKind::QueueHold, None, None, AskReason::Cost).is_ok());
        // The asks of the automatic update are about the queue's binary
        // (ADR-0073 decision 17): no task, no run.
        for kind in [AskKind::UpdateFailed, AskKind::ApproveUpdate] {
            assert!(kind.is_update() && kind.is_known());
            assert_eq!(AskKind::read(kind.as_str()), kind);
            assert!(check_ask_kind(&kind, None, None, scope).is_ok());
            assert!(matches!(
                check_ask_kind(&kind, None, Some(&run), scope),
                Err(DomainError::AskWithoutTarget { .. })
            ));
            let error = check_ask_kind(&kind, task, None, scope).unwrap_err();
            assert!(matches!(error, DomainError::UpdateAskWithTarget { .. }));
            assert!(
                error.to_string().contains("names no task or run"),
                "{error}"
            );
        }
        assert!(!AskKind::Blocked.is_update());
        assert_eq!(
            "update_failed".parse::<AskKind>().unwrap(),
            AskKind::UpdateFailed
        );
        for (kind, reason) in [
            (AskKind::QueueHold, AskReason::Scope),
            (AskKind::Decide, AskReason::Authentication),
        ] {
            let error = check_ask_kind(&kind, task, None, reason).unwrap_err();
            assert!(
                error.to_string().contains("only a queue_hold ask is for"),
                "{error}"
            );
        }

        assert!(check_event_target("task_created", task, None).is_ok());
        assert!(check_event_target("goal_closed", None, Some(GoalId::new(1))).is_ok());
        assert!(check_event_target("mark_recorded", None, None).is_ok());
        for kind in UPDATE_EVENT_KINDS {
            assert!(QUEUE_EVENT_KINDS.contains(kind), "{kind}");
            assert!(check_event_target(kind, None, None).is_ok());
        }
        assert_eq!(
            check_event_target("task_created", None, None)
                .unwrap_err()
                .to_string(),
            "a task_created event needs a task, a goal or a run; it is not an event of the queue itself"
        );
    }

    #[test]
    fn new_ask_rejects_blank_texts_and_bad_ids() {
        let valid = NewAsk {
            kind: AskKind::WorkerQuestion,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "q".into(),
            options: vec!["a".into()],
            asked_by: "worker".into(),
            reason_category: crate::domain::AskReason::Scope,
            finding_id: None,
        };
        assert!(valid.validate().is_ok());
        for broken in [
            NewAsk {
                question: " ".into(),
                ..valid.clone()
            },
            NewAsk {
                options: vec!["".into()],
                ..valid.clone()
            },
            NewAsk {
                asked_by: "".into(),
                ..valid.clone()
            },
            NewAsk {
                task_id: Some(TaskId::new(0)),
                ..valid.clone()
            },
            NewAsk {
                task_id: None,
                ..valid.clone()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
        // Only the observer's blocked ask may be about no task.
        let blocked = NewAsk {
            kind: AskKind::Blocked,
            task_id: None,
            ..valid.clone()
        };
        assert!(blocked.validate().is_ok());
        assert_eq!(
            NewAsk {
                task_id: None,
                ..valid.clone()
            }
            .validate()
            .unwrap_err()
            .to_string(),
            "a worker_question ask needs a task or a run; only a blocked ask may have neither"
        );
        assert_eq!("decide".parse::<AskKind>().unwrap(), AskKind::Decide);
        assert!("bogus".parse::<AskKind>().is_err());
        // Authentication and cost are queue_hold asks, one per queue
        // (ADR-0047 decision 42), and a queue_hold is nothing else.
        for broken in [
            NewAsk {
                reason_category: AskReason::Authentication,
                ..valid.clone()
            },
            NewAsk {
                reason_category: AskReason::Cost,
                ..valid.clone()
            },
            NewAsk {
                kind: AskKind::QueueHold,
                ..valid.clone()
            },
        ] {
            assert!(
                matches!(broken.validate(), Err(DomainError::AskHoldsTheQueue { .. })),
                "{broken:?}"
            );
        }
        assert!("because".parse::<AskReason>().is_err());
        assert_eq!(
            "recovery_failed".parse::<AskReason>().unwrap(),
            AskReason::RecoveryFailed
        );
    }

    #[test]
    fn a_hold_is_for_authentication_or_cost_and_lists_its_runs() {
        let hold = NewHold {
            reason_category: AskReason::Authentication,
            subject: None,
            run_id: RunId::new("run-1").unwrap(),
            question: "Log in.".into(),
            options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: "supervisor".into(),
        };
        assert!(hold.validate().is_ok());
        for broken in [
            NewHold {
                reason_category: AskReason::Scope,
                ..hold.clone()
            },
            NewHold {
                question: " ".into(),
                ..hold.clone()
            },
            NewHold {
                options: vec![" ".into()],
                ..hold.clone()
            },
            NewHold {
                asked_by: "".into(),
                ..hold.clone()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
        assert_eq!(
            NewHold {
                reason_category: AskReason::Discard,
                ..hold.clone()
            }
            .validate()
            .unwrap_err()
            .to_string(),
            "a queue_hold ask is for authentication or cost, not discard"
        );
        assert_eq!(
            NewHold::question_for("Log in.", &["a".into(), "b".into()]),
            "Log in.\n\nAffected runs: a, b"
        );
    }

    #[test]
    fn event_attention_covers_every_kind_by_its_status() {
        use AttentionNext::*;
        let cases = [
            (
                "validation_finished",
                json!({"status": "awaiting_integration"}),
                None,
            ),
            (
                "review_failed",
                json!({"status": "awaiting_integration", "error": "x", "attempt": 1}),
                Some(ReviewByHand),
            ),
            (
                "review_failed",
                json!({"status": "awaiting_integration", "error": "x", "attempt": 2, "ask_id": 7}),
                None,
            ),
            (
                "triage_failed",
                json!({"status": "failed", "error": "x", "attempt": 1}),
                Some(TriageByHand),
            ),
            ("triage_started", json!({"attempt": 1}), None),
            (
                "triage_finished",
                json!({"verdict": "retry", "action": "retry", "status": "failed"}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 6, "kind": "decide", "runtime_delivers": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 6, "kind": "decide", "runtime_delivers": false}),
                Some(ReadAnswer {
                    ask_id: AskId::new(6),
                }),
            ),
            ("review_started", json!({"attempt": 1}), None),
            (
                "review_finished",
                json!({"verdict": "concern", "reasons": ["x"], "summary": "s"}),
                None,
            ),
            (
                "revise_requested",
                json!({"attempt": 1, "reasons": ["x"]}),
                None,
            ),
            ("revise_finished", json!({"attempt": 1, "head": "h"}), None),
            (
                "conflict_precheck",
                json!({"main": "m", "head": "h", "conflicts": ["f"], "requested": true}),
                None,
            ),
            (
                "conflict_resolved",
                json!({"attempt": 1, "head": "h"}),
                None,
            ),
            (
                "landing_decided",
                json!({"ask_id": 3, "answer": "cancel", "status": "failed"}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "approve_landing", "runtime_delivers": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "approve_landing", "runtime_delivers": false}),
                Some(ReadAnswer {
                    ask_id: AskId::new(5),
                }),
            ),
            (
                "validation_finished",
                json!({"status": "failed", "reason": "x"}),
                None,
            ),
            (
                "supervision_finished",
                json!({"status": "failed", "exit_code": 1}),
                None,
            ),
            (
                "supervision_finished",
                json!({"status": "validating", "exit_code": 0}),
                None,
            ),
            (
                "integration_deferred",
                json!({"status": "needs_session", "reason": "x"}),
                None,
            ),
            (
                "integration_failed",
                json!({"status": "failed", "reason": "x"}),
                None,
            ),
            (
                "integration_error",
                json!({"status": "needs_session", "reason": "x"}),
                None,
            ),
            (
                "integration_error",
                json!({"status": "awaiting_integration"}),
                None,
            ),
            (
                "exit_request_timed_out",
                json!({"workspace_id": "w", "timeout_secs": 120}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "stuck_exit", "runtime_closed": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "stuck_exit"}),
                Some(ReadAnswer {
                    ask_id: AskId::new(5),
                }),
            ),
            (
                "push_failed",
                json!({"remote": "origin", "commit": "c", "error": "x"}),
                Some(PushMain),
            ),
            (
                "run_env_program_missing",
                json!({"variable": "RUSTC_WRAPPER", "value": "sccache", "path": "/bin"}),
                Some(InstallTool),
            ),
            (
                "run_env_program_found",
                json!({"variable": "RUSTC_WRAPPER", "value": "sccache", "path": "/bin"}),
                None,
            ),
            (
                "push_finished",
                json!({"remote": "origin", "commit": "c"}),
                None,
            ),
            (
                "push_skipped",
                json!({"remote": "origin", "reason": "x"}),
                None,
            ),
            (
                "runtime_error",
                json!({"message": "x", "lease_released": true}),
                Some(RecoverRun),
            ),
            (
                "runtime_error",
                json!({"message": "x", "lease_released": false}),
                None,
            ),
            ("runtime_error", json!({"message": "x"}), None),
            (
                "prompt_waiting",
                json!({"workspace_id": "w", "excerpt": "x", "screen_hash": "h"}),
                None,
            ),
            ("prompt_cleared", json!({"workspace_id": "w"}), None),
            (
                "integration_deferred",
                json!({"status": "needs_session", "reason": "x", "resumes_left": 2}),
                None,
            ),
            (
                "integration_deferred",
                json!({"status": "needs_session", "reason": "x", "resumes_left": 0}),
                None,
            ),
            (
                "resume_finished",
                json!({"status": "awaiting_integration", "outcome": "resolved"}),
                Some(ReviewAndIntegrate),
            ),
            (
                "resume_finished",
                json!({"status": "failed", "outcome": "failed"}),
                None,
            ),
            (
                "resume_finished",
                json!({"status": "needs_session", "outcome": "unresolved", "exhausted": true}),
                None,
            ),
            (
                "resume_finished",
                json!({"status": "needs_session", "outcome": "unresolved", "exhausted": false}),
                None,
            ),
            (
                "resume_finished",
                json!({"status": "needs_session", "outcome": "resolved"}),
                None,
            ),
            ("resume_started", json!({"attempt": 1}), None),
            ("integration_approved", json!({}), None),
            ("integration_rebase_aborted", json!({"reason": "x"}), None),
            ("run_integrated", json!({"result_commit": "x"}), None),
            (
                "lease_released",
                json!({"reason": "integration_failed"}),
                None,
            ),
            ("validation_finished", json!({"status": "bogus"}), None),
            (
                "ask_opened",
                json!({"ask_id": 3, "kind": "decide"}),
                Some(AnswerAsk {
                    ask_id: AskId::new(3),
                }),
            ),
            (
                "ask_answered",
                json!({"ask_id": 3, "kind": "decide"}),
                Some(ReadAnswer {
                    ask_id: AskId::new(3),
                }),
            ),
            ("ask_opened", json!({}), None),
            (
                "ask_answered",
                json!({"ask_id": 4, "kind": "worker_question", "runtime_delivers": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 4, "kind": "worker_question", "runtime_delivers": false}),
                Some(DeliverAnswer {
                    ask_id: AskId::new(4),
                }),
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "planner_question", "runtime_delivers": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 5, "kind": "planner_question", "runtime_delivers": false}),
                Some(DeliverAnswer {
                    ask_id: AskId::new(5),
                }),
            ),
            ("ask_delivered", json!({"ask_id": 4}), None),
            (
                "ask_delivery_failed",
                json!({"ask_id": 4, "error": "x"}),
                Some(DeliverAnswer {
                    ask_id: AskId::new(4),
                }),
            ),
            ("validation_finished", json!({}), None),
            (
                "update_installed",
                json!({"commit": "abc", "version": "0.4.0-dev+abc"}),
                Some(ReportUpdate),
            ),
            ("update_started", json!({"commit": "abc"}), None),
            (
                "update_failed",
                json!({"stage": "build", "ask_id": 3}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 3, "kind": "update_failed", "runtime_delivers": true}),
                None,
            ),
            (
                "ask_answered",
                json!({"ask_id": 3, "kind": "update_failed", "runtime_delivers": false}),
                Some(ReadAnswer {
                    ask_id: AskId::new(3),
                }),
            ),
        ];
        for (kind, payload, expected) in cases {
            assert_eq!(
                event_attention(kind, &payload),
                expected,
                "{kind} {payload}"
            );
            if expected.is_some() {
                assert!(ATTENTION_KINDS.contains(&kind), "{kind}");
            }
        }
        assert_eq!(Reviewing.to_string(), "reviewing (runtime)");
        assert_eq!(ReviewByHand.to_string(), "review by hand");
        assert_eq!(
            ApplyingAnswer {
                ask_id: AskId::new(7)
            }
            .to_string(),
            "applying the answer of ask 7 (runtime)"
        );
        assert_eq!(RecoverRun.to_string(), "recover run");
        assert_eq!(PushMain.to_string(), "push main");
        assert_eq!(InstallTool.to_string(), "install tool");
        assert_eq!(ReportUpdate.to_string(), "report the update");
        assert_eq!(
            DeliveringAnswer {
                ask_id: AskId::new(2)
            }
            .to_string(),
            "delivering the answer of ask 2 (runtime)"
        );
        assert_eq!(
            DeliverAnswer {
                ask_id: AskId::new(2)
            }
            .to_string(),
            "send the answer of ask 2 to the worker and close it"
        );
        assert_eq!(
            serde_json::to_value(RestartSupervisor).unwrap(),
            json!("restart supervisor")
        );
        // A dialog and a used-up resume are asks, not attention (ADR-0024).
        assert_eq!(event_attention("prompt_waiting", &json!({})), None);
    }

    #[test]
    fn review_verdict_is_read_from_the_whole_stdout_or_its_outermost_object() {
        let verdict = ReviewVerdict::parse(
            r#"{"verdict":"revise","reasons":["add a test"],"summary":"almost"}"#,
        )
        .unwrap();
        assert_eq!(verdict.verdict, ReviewDecision::Revise);
        assert_eq!(verdict.reasons, vec!["add a test".to_owned()]);
        let fenced =
            "Here it is:\n```json\n{\"verdict\":\"pass\",\"reasons\":[],\"summary\":\"ok\"}\n```\n";
        assert_eq!(
            ReviewVerdict::parse(fenced).unwrap().verdict,
            ReviewDecision::Pass
        );
        // A trailing comment is outside the outermost object.
        let commented = r#"{"verdict":"pass","reasons":[],"summary":"ok"} // done"#;
        assert_eq!(
            ReviewVerdict::parse(commented).unwrap().verdict,
            ReviewDecision::Pass
        );
        for bad in [
            // An unescaped quote inside a string (task 225's review).
            r#"{"verdict":"pass","reasons":[],"summary":"says "fine""}"#,
            "",
            "no json here",
            r#"{"verdict":"maybe","reasons":[],"summary":"x"}"#,
            r#"{"verdict":"pass","summary":"x"}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"x","extra":1}"#,
        ] {
            let error = ReviewVerdict::parse(bad).unwrap_err();
            assert!(error.contains("no verdict JSON"), "{bad}: {error}");
        }
    }

    #[test]
    fn run_attention_follows_the_resting_status() {
        use AttentionNext::*;
        assert_eq!(
            run_attention(RunStatus::AwaitingIntegration, false, false, false),
            Some(ReviewAndIntegrate)
        );
        // Leased, it is the supervisor's review (ADR-0027); a session that
        // held back the /exit after the verdict is its stuck_exit ask's.
        assert_eq!(
            run_attention(RunStatus::AwaitingIntegration, true, false, true),
            None
        );
        assert_eq!(run_attention(RunStatus::Failed, true, false, true), None);
        assert_eq!(
            run_attention(RunStatus::AwaitingIntegration, false, false, true),
            Some(Reviewing)
        );
        // Resuming, blocked by a live session or out of resumes: the
        // supervisor's either way.
        for leased in [false, true] {
            assert_eq!(
                run_attention(RunStatus::NeedsSession, false, false, leased),
                Some(Resuming)
            );
        }
        assert_eq!(Resuming.to_string(), "resuming (runtime)");
        assert_eq!(
            run_attention(RunStatus::Failed, false, false, false),
            Some(Triaging)
        );
        assert_eq!(
            run_attention(RunStatus::Interrupted, false, false, false),
            Some(Triaging)
        );
        assert_eq!(Triaging.to_string(), "triaging (runtime)");
        assert_eq!(TriageByHand.to_string(), "triage by hand");
        assert_eq!(RecoverByHand.to_string(), "recover by hand");
        assert_eq!(
            event_attention("recovery_failed", &serde_json::json!({})),
            Some(RecoverByHand)
        );
        // The stuck_exit ask is the attention of a session holding `/exit`.
        assert_eq!(run_attention(RunStatus::Running, true, false, true), None);
        assert_eq!(run_attention(RunStatus::Running, false, false, true), None);
        // An abandoned run is recovered.
        assert_eq!(
            run_attention(RunStatus::Running, false, false, false),
            Some(RecoverRun)
        );
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Validating,
            RunStatus::Integrating,
            RunStatus::Integrated,
            RunStatus::Succeeded,
        ] {
            assert_eq!(
                run_attention(status, true, false, true),
                None,
                "{}",
                status.as_str()
            );
        }
        assert_eq!(
            run_attention(RunStatus::Integrated, false, true, false),
            Some(PushMain)
        );
        assert_eq!(
            run_attention(RunStatus::Succeeded, false, true, false),
            None
        );
    }

    #[test]
    fn run_attention_asks_to_recover_an_unfinished_run_without_a_lease() {
        use AttentionNext::*;
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Validating,
            RunStatus::Integrating,
        ] {
            assert_eq!(
                run_attention(status, false, false, false),
                Some(RecoverRun),
                "{}",
                status.as_str()
            );
        }
        // Nothing moves an abandoned run, so `/exit` alone would not do.
        assert_eq!(
            run_attention(RunStatus::Running, true, false, false),
            Some(RecoverRun)
        );
        for status in [RunStatus::Integrated, RunStatus::Succeeded] {
            assert_eq!(
                run_attention(status, false, false, false),
                None,
                "{}",
                status.as_str()
            );
        }
    }

    #[test]
    fn triage_state_follows_the_latest_triage_or_resume() {
        let event = |id: i64, kind: &str| RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: kind.into(),
            payload: serde_json::json!({}),
            created_at: String::new(),
        };
        assert_eq!(triage_state(&[]), TriageState::Pending);
        let mut again = event(10, "triage_decided");
        again.payload = serde_json::json!({"action": "recover", "answer": "split it"});
        assert_eq!(
            triage_state(&[event(4, "triage_finished"), again]),
            TriageState::Pending
        );
        let mut wait = event(9, "triage_finished");
        wait.payload = serde_json::json!({"action": "wait", "recheck_at": 42});
        assert_eq!(triage_state(&[wait]), TriageState::Waiting { until: 42 });
        let mut events = vec![event(1, "validation_finished"), event(2, "triage_started")];
        assert_eq!(triage_state(&events), TriageState::Pending);
        events.push(event(3, "triage_failed"));
        assert_eq!(triage_state(&events), TriageState::Failed);
        events.push(event(4, "triage_finished"));
        assert_eq!(triage_state(&events), TriageState::Finished);
        // A run resumed after its triage is triaged again once it fails.
        events.push(event(5, "resume_started"));
        events.push(event(6, "resume_finished"));
        assert_eq!(triage_state(&events), TriageState::Pending);
    }

    #[test]
    fn answers_are_the_sessions_while_it_runs_or_fixes_a_request() {
        let event = |id: i64, kind: &str, payload: serde_json::Value| RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: kind.into(),
            payload,
            created_at: String::new(),
        };
        let waiting = RunStatus::AwaitingIntegration;
        assert!(session_takes_answers(RunStatus::Running, &[]));
        assert!(!session_takes_answers(waiting, &[]));
        let mut events = vec![
            event(1, "validation_finished", serde_json::json!({})),
            event(2, "revise_requested", serde_json::json!({"attempt": 1})),
            // Not a step of the review: the revise still waits.
            event(3, "ask_opened", serde_json::json!({})),
        ];
        assert!(session_takes_answers(waiting, &events));
        assert!(!session_takes_answers(RunStatus::NeedsSession, &events));
        events.push(event(4, "revise_finished", serde_json::json!({})));
        assert!(!session_takes_answers(waiting, &events));
        events.push(event(5, "conflict_precheck", json!({"requested": false})));
        assert!(!session_takes_answers(waiting, &events));
        events.push(event(6, "conflict_precheck", json!({"requested": true})));
        assert!(session_takes_answers(waiting, &events));
        assert_eq!(review_anchor(&events).map(|e| e.id), Some(EventId::new(6)));
        // A request the session did not fix ends in its `/exit`.
        events.push(event(7, "exit_requested", serde_json::json!({})));
        assert!(!session_takes_answers(waiting, &events));
        // A resumed session takes them until its `/exit` or its end.
        let parked = RunStatus::NeedsSession;
        events.push(event(8, "resume_started", serde_json::json!({})));
        assert!(session_takes_answers(parked, &events));
        assert!(!session_takes_answers(waiting, &events));
        events.push(event(9, "exit_requested", json!({"resume_attempt": 1})));
        assert!(!session_takes_answers(parked, &events));
        events.push(event(10, "resume_started", serde_json::json!({})));
        assert!(session_takes_answers(parked, &events));
        events.push(event(11, "resume_finished", serde_json::json!({})));
        assert!(!session_takes_answers(parked, &events));
    }

    #[test]
    fn supervisor_attention_reports_stale_registrations_or_a_stopped_queue() {
        let registration = |token: &str, heartbeat_at| SupervisorRegistration {
            token: token.into(),
            pid: 7,
            parallel: 1,
            started_at: 0,
            heartbeat_at,
            mode: None,
            workspace_id: None,
            handoff_accepted: false,
            handoff_binary: None,
            auto_update: false,
            max_waiting: None,
            binary_version: None,
        };
        let fresh =
            SupervisorPulse::judge(&registration("a", 100), true, 100 + HEARTBEAT_TIMEOUT_SECS);
        let hung =
            SupervisorPulse::judge(&registration("b", 100), true, 101 + HEARTBEAT_TIMEOUT_SECS);
        let dead = SupervisorPulse::judge(&registration("c", 100), false, 100);
        assert!(!fresh.stale && hung.stale && dead.stale);
        assert_eq!(supervisor_attention(std::slice::from_ref(&fresh)), vec![]);
        let stale = supervisor_attention(&[fresh, hung, dead]);
        let summary: Vec<_> = stale
            .iter()
            .map(|a| (a.kind.as_str(), a.status.as_str(), a.pid))
            .collect();
        assert_eq!(
            summary,
            [
                ("supervisor_stale", "stale", Some(7)),
                ("supervisor_stale", "dead", Some(7))
            ]
        );
        assert!(
            stale
                .iter()
                .all(|a| a.next == AttentionNext::RestartSupervisor && a.run_id.is_none())
        );
        let stopped = supervisor_attention(&[]);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].kind, "supervisor_stopped");
        assert_eq!(stopped[0].pid, None);
        assert_eq!(
            serde_json::to_value(&stopped[0]).unwrap(),
            json!({
                "run_id": null, "task_id": null, "status": "stopped", "kind": "supervisor_stopped",
                "last_error": null, "next": "restart supervisor",
            })
        );
    }
}
