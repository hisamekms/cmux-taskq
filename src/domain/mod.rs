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

string_enum!(TaskStatus {
    Draft => "draft",
    Ready => "ready",
    InProgress => "in_progress",
    Completed => "completed",
    Canceled => "canceled",
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
// ADR-0024 decision 1 are the supervisor, the worker, the planner, the
// inbox and the observer; `up` opens the planner's and the inbox's
// workspaces. `Observer` is the periodic job: it has no workspace, and the
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
// need an answer are asks; a notice is an attention.
string_enum!(AskKind {
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

// The verdict of the supervisor's headless triage of a `failed` or
// `interrupted` run (ADR-0024 decision 3): `retry` makes the task `ready`
// for a new run, `resume` sends the run to a session of its own as
// `needs_session`, `ask` waits for a person in a `decide` ask.
string_enum!(TriageDecision {
    Retry => "retry",
    Resume => "resume",
    Ask => "ask",
});

/// What the headless triage prints on stdout: one JSON object. `instruction`
/// is what the resumed session is asked to do for `resume`, the question for
/// `ask`, and may be empty for `retry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageVerdict {
    pub verdict: TriageDecision,
    pub reason: String,
    #[serde(default)]
    pub instruction: String,
}

impl TriageVerdict {
    /// The verdict in the triage's stdout, found the way
    /// [`ReviewVerdict::parse`] finds the review's.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        parse_json_object(stdout)
            .map_err(|error| format!("the triage printed no verdict JSON: {error}"))
    }
}

/// The whole text as one JSON object of `T`, or else the outermost `{...}`
/// in it (a model may wrap the object in a fence or a sentence).
fn parse_json_object<T: serde::de::DeserializeOwned>(stdout: &str) -> serde_json::Result<T> {
    let text = stdout.trim();
    serde_json::from_str::<T>(text).or_else(|error| match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => serde_json::from_str::<T>(&text[start..=end]),
        _ => Err(error),
    })
}

/// The options of the `decide` ask a triage opens, which the supervisor acts
/// on once answered: `retry` and `cancel` move the task, `resume` the run.
pub const TRIAGE_OPTIONS: &[&str] = &["retry", "resume", "cancel"];

/// A task with this many `failed` or `interrupted` runs, the triaged one
/// included, is not retried by the triage: a `retry` verdict becomes an
/// ask, so a failure that repeats reaches a person.
pub const TRIAGE_RETRY_FAILURES: usize = 2;

/// Where the triage of a `failed` or `interrupted` run stands, from the
/// latest of its `resume_started`, `triage_finished` and `triage_failed`: a
/// run resumed since its last triage is triaged again when it fails again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageState {
    /// Not triaged yet: the supervisor triages it.
    Pending,
    /// The headless triage failed: a person decides.
    Failed,
    /// The verdict was acted on.
    Finished,
}

pub fn triage_state(events: &[RunEvent]) -> TriageState {
    match events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.as_str(),
                "resume_started" | "triage_finished" | "triage_failed"
            )
        })
        .map(|e| e.kind.as_str())
    {
        Some("triage_finished") => TriageState::Finished,
        Some("triage_failed") => TriageState::Failed,
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

mod error;
pub mod goal;
pub mod ids;
mod input;
mod run;
pub mod scope;
pub mod stats;
pub mod task;
mod views;

pub use error::DomainError;
use error::require;
pub use goal::Goal;
pub use ids::{CommitSha, GoalId, RunId, TaskId};
pub use input::{GoalEdit, GoalRecord, NewGoal, NewTask, TaskRecord};
pub use run::{RunPaths, TaskRun};
pub use task::{Task, TaskAction};
pub use views::{
    ClaimOutcome, GoalDetail, GoalSummary, GoalTask, IntegrationOutcome, Predecessor, Receipt,
    ReceiptCheck, RegisteredFollowUp, RunEvent, RunLease, RunProcess, SupervisorRegistration,
    TaskDetail, TaskStatusCounts, evidence_missing_reason,
};

/// A question for a person (ADR-0022): about a task, or one of its runs when
/// `run_id` is set; a `blocked` ask of the observer may be about neither
/// (ADR-0024 decision 4). It is open while `answered_at` and `closed_at` are
/// unset; once answered it waits for the inbox, where the person acts on
/// the answer, to close it (or for the runtime to apply it). Times are unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub id: i64,
    pub kind: AskKind,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub question: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    /// The role of the session that registered it (`DAGQ_ROLE`).
    pub asked_by: String,
    pub created_at: i64,
    pub answered_at: Option<i64>,
    pub closed_at: Option<i64>,
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
            self.task_id.is_some() || self.run_id.is_some() || self.kind == AskKind::Blocked,
            || DomainError::AskWithoutTarget { kind: self.kind },
        )
    }
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
    pub since: Option<i64>,
    pub limit: usize,
}

/// One page of `notes`, oldest first; `cursor` is the last note's event id
/// (or `since` when the page is empty), to pass back as `--since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotePage {
    pub notes: Vec<RunEvent>,
    pub cursor: i64,
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
            dependencies: vec![TaskId::new(0)],
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
            dependencies: Vec::new(),
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
        ask_id: i64,
    },
    ReadAnswer {
        ask_id: i64,
    },
    /// Not a person's to act on: the supervisor types the answer of a
    /// `worker_question` into the worker's terminal once the worker is idle.
    DeliveringAnswer {
        ask_id: i64,
    },
    /// The supervisor could not type the answer of a `worker_question` into
    /// the worker's terminal (it tries once), or the worker's session is gone.
    DeliverAnswer {
        ask_id: i64,
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
        ask_id: i64,
    },
    /// Not a person's to act on: the supervisor triages the `failed`
    /// or `interrupted` run and acts on the verdict (ADR-0024 decision 3).
    Triaging,
    /// The headless triage failed (`triage_failed`): a person decides
    /// whether to `ready` the task again, resume or cancel.
    TriageByHand,
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
    "ask_opened",
    "ask_answered",
    "ask_delivery_failed",
];

/// The attention kinds an ask writes (ADR-0022): about the ask, even when it
/// names a run.
pub const ASK_EVENT_KINDS: &[&str] = &[
    "ask_opened",
    "ask_answered",
    "ask_delivered",
    "ask_delivery_failed",
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
/// run then waits for a review by hand.
/// `ask_opened` waits for the inbox's answer and
/// `ask_answered` for the person to act on it through the inbox,
/// except the answer of a `worker_question`, which the supervisor types into
/// the worker's terminal itself (`runtime_delivers: true`); its answer to a
/// run no longer running and its `ask_delivery_failed` are the inbox's.
/// The answer of an `approve_landing` ask the supervisor applies
/// (`runtime_delivers: true`: one of [`LANDING_OPTIONS`] for a run awaiting
/// integration) is not one either, nor that of a triage's `decide` ask
/// (`runtime_delivers: true`: one of [`TRIAGE_OPTIONS`] for a `failed` or
/// `interrupted` run).
pub fn event_attention(kind: &str, payload: &serde_json::Value) -> Option<AttentionNext> {
    let status = payload
        .get("status")
        .and_then(serde_json::Value::as_str)
        .and_then(|status| status.parse::<RunStatus>().ok());
    match (kind, status) {
        // The supervisor that validated the run reviews it and acts on the
        // verdict itself (ADR-0023 decision 2, ADR-0027).
        ("validation_finished", Some(RunStatus::AwaitingIntegration)) => None,
        ("review_failed", _) => Some(AttentionNext::ReviewByHand),
        // The supervisor triages a failed run and acts on the verdict
        // (ADR-0024 decision 3); only a triage that failed is a person's.
        ("triage_failed", _) => Some(AttentionNext::TriageByHand),
        ("push_failed", _) => Some(AttentionNext::PushMain),
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
        ("ask_answered", _)
            if payload.get("kind").and_then(serde_json::Value::as_str)
                == Some(AskKind::WorkerQuestion.as_str()) =>
        {
            match payload.get("runtime_delivers") {
                Some(serde_json::Value::Bool(false)) => {
                    ask_id(payload).map(|ask_id| AttentionNext::DeliverAnswer { ask_id })
                }
                _ => None,
            }
        }
        // An answer the supervisor applies to the run itself: an
        // `approve_landing` one, or a triage's `decide` one.
        ("ask_answered", _)
            if matches!(
                payload.get("kind").and_then(serde_json::Value::as_str),
                Some(kind) if kind == AskKind::ApproveLanding.as_str()
                    || kind == AskKind::Decide.as_str()
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

fn ask_id(payload: &serde_json::Value) -> Option<i64> {
    payload.get("ask_id").and_then(serde_json::Value::as_i64)
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
    pub ask_id: Option<i64>,
    pub status: String,
    pub kind: String,
    pub last_error: Option<String>,
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
        status: status.into(),
        kind: kind.into(),
        last_error: None,
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

    #[test]
    fn asks_wait_for_the_inbox_until_closed() {
        let mut ask = Ask {
            id: 1,
            kind: AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "q".into(),
            options: vec![],
            answer: None,
            asked_by: "supervisor".into(),
            created_at: 0,
            answered_at: None,
            closed_at: None,
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
            AttentionNext::ReadAnswer { ask_id: 4 }.to_string(),
            "read the answer of ask 4 and close it"
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
                Some(ReadAnswer { ask_id: 6 }),
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
                Some(ReadAnswer { ask_id: 5 }),
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
                Some(ReadAnswer { ask_id: 5 }),
            ),
            (
                "push_failed",
                json!({"remote": "origin", "commit": "c", "error": "x"}),
                Some(PushMain),
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
                Some(AnswerAsk { ask_id: 3 }),
            ),
            (
                "ask_answered",
                json!({"ask_id": 3, "kind": "decide"}),
                Some(ReadAnswer { ask_id: 3 }),
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
                Some(DeliverAnswer { ask_id: 4 }),
            ),
            ("ask_delivered", json!({"ask_id": 4}), None),
            (
                "ask_delivery_failed",
                json!({"ask_id": 4, "error": "x"}),
                Some(DeliverAnswer { ask_id: 4 }),
            ),
            ("validation_finished", json!({}), None),
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
            ApplyingAnswer { ask_id: 7 }.to_string(),
            "applying the answer of ask 7 (runtime)"
        );
        assert_eq!(RecoverRun.to_string(), "recover run");
        assert_eq!(PushMain.to_string(), "push main");
        assert_eq!(
            DeliveringAnswer { ask_id: 2 }.to_string(),
            "delivering the answer of ask 2 (runtime)"
        );
        assert_eq!(
            DeliverAnswer { ask_id: 2 }.to_string(),
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
        for bad in [
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
    fn triage_verdict_is_read_from_the_output_and_instruction_defaults_to_empty() {
        let verdict = TriageVerdict::parse(
            "Here it is:\n```json\n{\"verdict\": \"resume\", \"reason\": \"r\", \"instruction\": \"fix it\"}\n```",
        )
        .unwrap();
        assert_eq!(verdict.verdict, TriageDecision::Resume);
        assert_eq!(verdict.instruction, "fix it");
        let verdict =
            TriageVerdict::parse("{\"verdict\": \"retry\", \"reason\": \"flaky\"}").unwrap();
        assert_eq!(verdict.verdict, TriageDecision::Retry);
        assert!(verdict.instruction.is_empty());
        let error = TriageVerdict::parse("test provider").unwrap_err();
        assert!(
            error.starts_with("the triage printed no verdict JSON"),
            "{error}"
        );
        assert!(TriageVerdict::parse("{\"verdict\": \"land\", \"reason\": \"x\"}").is_err());
    }

    #[test]
    fn triage_state_follows_the_latest_triage_or_resume() {
        let event = |id: i64, kind: &str| RunEvent {
            id,
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: kind.into(),
            payload: serde_json::json!({}),
            created_at: String::new(),
        };
        assert_eq!(triage_state(&[]), TriageState::Pending);
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
    fn supervisor_attention_reports_stale_registrations_or_a_stopped_queue() {
        let registration = |token: &str, heartbeat_at| SupervisorRegistration {
            token: token.into(),
            pid: 7,
            parallel: 1,
            started_at: 0,
            heartbeat_at,
            mode: None,
            workspace_id: None,
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
