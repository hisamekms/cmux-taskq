//! Reason codes of failures, parks and interruptions (ADR-0034 decision 1):
//! a closed set of snake_case names an event's payload carries as `code`
//! next to its free-text `reason` / `message` / `error`, which stays as it
//! was. The code says why; the event's kind says in which step. Values that
//! belong to one code (the exit code, the verification command's index,
//! the conflicted files, the backend call's `op`) sit beside it in the
//! payload, never paths or workspace IDs (ADR-0032). Events written before
//! the codes have none.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{DomainError, RunEvent, RunStatus, TaskRun};

/// The payload key of a reason code.
pub const CODE_KEY: &str = "code";

macro_rules! reason_codes {
    ($($variant:ident => $value:literal: $meaning:literal),+ $(,)?) => {
        /// Why a run failed, waits for a session or was interrupted, or why
        /// a step of it went wrong. Part of the public contract like the
        /// event kinds: renaming one needs an ADR.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum ReasonCode {
            $(#[serde(rename = $value)] $variant),+
        }

        impl ReasonCode {
            /// Every code, in the order of the documentation.
            pub const ALL: &[ReasonCode] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }

            /// What the code means, as the documentation says.
            pub const fn meaning(self) -> &'static str {
                match self { $(Self::$variant => $meaning),+ }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

reason_codes! {
    SessionExitCode => "session_exit_code": "the session exited with a non-zero code of its own (1-127)",
    SessionKilled => "session_killed": "the session was ended by a signal (exit code 128, which the wrapper reports for a signal, or 128+N such as 143 for SIGTERM)",
    ExitTimeout => "exit_timeout": "the session did not exit within the exit timeout of /exit",
    HeartbeatLost => "heartbeat_lost": "the session wrapper stopped its heartbeat while its process lived on",
    WrapperFailed => "wrapper_failed": "the session wrapper could not run the agent",
    LeaseLost => "lease_lost": "the supervisor's heartbeat failed, so it could not keep its leases",
    ReceiptMissing => "receipt_missing": "no receipt was written",
    ReceiptInvalid => "receipt_invalid": "the receipt cannot be read, names another run, or leaves a check unexplained",
    WorkerFailed => "worker_failed": "the receipt reports the run as failed",
    EvidenceFailed => "evidence_failed": "the receipt reports a check the task does not require as failed",
    EvidenceMissing => "evidence_missing": "the receipt does not back a check the task requires",
    CommitMismatch => "commit_mismatch": "the receipt's commit is not the run branch's new head on top of its base",
    WorktreeDirty => "worktree_dirty": "the worktree has uncommitted changes",
    ScopeViolation => "scope_violation": "the diff changes paths outside the task's --paths",
    RebaseConflict => "rebase_conflict": "the run conflicts with main (the landing's rebase, or the merge-tree precheck)",
    RebaseEmpty => "rebase_empty": "no commit remains on top of main after the rebase",
    RebaseInProgress => "rebase_in_progress": "a rebase was left in progress in the worktree and was aborted",
    MigrationNumberTaken => "migration_number_taken": "the run adds a migration whose number main already has, and it cannot be renumbered mechanically",
    VerificationFailed => "verification_failed": "a verification command exited non-zero after the rebase",
    BackendTimeout => "backend_timeout": "a cmux call timed out",
    BackendFailed => "backend_failed": "a cmux call failed",
    JobFailed => "job_failed": "a headless review or triage job failed",
    SentBack => "sent_back": "a person sent a review's concern back to the session",
    Cancelled => "cancelled": "a person cancelled the landing",
    TriageResume => "triage_resume": "the triage (or a person answering it) sent the run back to its session",
    ResumeExhausted => "resume_exhausted": "the run still needed a session after its last resume",
    Orphaned => "orphaned": "the run's registered processes were found dead and it was recovered",
    PushFailed => "push_failed": "the push of the landed main failed",
    GitFailed => "git_failed": "a Git command of the runtime failed (removing a landed worktree)",
    Other => "other": "none of the above; the free text says why",
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ReasonCode {
    /// A session's non-zero exit: a signal from 128 on (the wrapper reports
    /// 128 for a session a signal ended, a shell 128+N), its own code
    /// otherwise.
    pub const fn of_exit_code(exit_code: i32) -> Self {
        if exit_code >= 128 {
            Self::SessionKilled
        } else {
            Self::SessionExitCode
        }
    }

    /// A receipt the domain refused to read or check.
    pub const fn of_receipt_error(error: &DomainError) -> Self {
        match error {
            DomainError::AgentReportedResult { .. } => Self::WorkerFailed,
            DomainError::ReceiptCheckFailed { .. } => Self::EvidenceFailed,
            _ => Self::ReceiptInvalid,
        }
    }

    /// A failed cmux call, by its error text: the adapter's deadline
    /// (`did not finish within`) and cmux's own `Command timed out` are
    /// timeouts.
    pub fn of_backend_error(error: &str) -> Self {
        let error = error.to_ascii_lowercase();
        if error.contains("timed out") || error.contains("did not finish within") {
            Self::BackendTimeout
        } else {
            Self::BackendFailed
        }
    }
}

/// A reason code with the values that belong to it, for an event payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    pub code: ReasonCode,
    pub detail: Map<String, Value>,
}

impl Reason {
    pub fn new(code: ReasonCode) -> Self {
        Self {
            code,
            detail: Map::new(),
        }
    }

    /// A session's non-zero exit, with `exit_code` and, for 128+N, the
    /// `signal` N.
    pub fn of_exit_code(exit_code: i32) -> Self {
        let reason = Self::new(ReasonCode::of_exit_code(exit_code)).with("exit_code", exit_code);
        if exit_code > 128 {
            reason.with("signal", exit_code - 128)
        } else {
            reason
        }
    }

    /// A value that belongs to the code.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.detail.insert(key.to_owned(), value.into());
        self
    }

    /// Put `code` and the detail into `payload` (an object; anything else
    /// becomes one). Keys the payload has already keep their value.
    pub fn apply_to(&self, payload: &mut Value) {
        if !payload.is_object() {
            *payload = Value::Object(Map::new());
        }
        let Value::Object(object) = payload else {
            return;
        };
        object.insert(CODE_KEY.to_owned(), Value::from(self.code.as_str()));
        for (key, value) in &self.detail {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    /// `payload` with the code and the detail in it.
    #[must_use]
    pub fn on(&self, mut payload: Value) -> Value {
        self.apply_to(&mut payload);
        payload
    }
}

impl From<ReasonCode> for Reason {
    fn from(code: ReasonCode) -> Self {
        Self::new(code)
    }
}

/// The code an event carries, if any.
pub fn event_code(event: &RunEvent) -> Option<ReasonCode> {
    event
        .payload
        .get(CODE_KEY)
        .and_then(Value::as_str)
        .and_then(ReasonCode::parse)
}

/// Whether `event` is one that set its run's `last_error` or interrupted
/// it: the events whose code explains the run's current error.
fn explains_last_error(event: &RunEvent) -> bool {
    let status = event.payload.get("status").and_then(Value::as_str);
    match event.kind.as_str() {
        "supervision_finished"
        | "validation_finished"
        | "integration_deferred"
        | "integration_failed"
        | "integration_error"
        | "runtime_error"
        | "landing_decided" => true,
        // Recovery only interrupts; one mid-integration goes back to
        // `awaiting_integration` with the `last_error` it had.
        "run_recovered" => status == Some("interrupted"),
        // The triage's or the supervisor sweep's own close failure (`by:
        // triage` / `supervisor`) leaves `last_error`.
        "cleanup_failed" => !matches!(
            event.payload.get("by").and_then(Value::as_str),
            Some("triage" | "supervisor")
        ),
        // Only a triage that sent the run back to its session (or handed
        // it to a person after its last resume) replaced `last_error`;
        // a retry or an ask left it, and those carry no code.
        "triage_finished" | "triage_decided" => event.payload.get(CODE_KEY).is_some(),
        // A resume's own error leaves `last_error` as it was; a rewritten
        // `failed` receipt replaces it.
        "resume_finished" => status == Some("failed"),
        _ => false,
    }
}

/// The code of the run's `last_error` (or of its interruption): that of the
/// latest of `events` (the run's, oldest first) that set it and carries a
/// code. `None` when that event predates the codes or nothing set it.
pub fn last_error_code(events: &[RunEvent]) -> Option<ReasonCode> {
    events
        .iter()
        .rev()
        .find(|event| explains_last_error(event))
        .and_then(event_code)
}

/// The code of `run`'s `last_error`, or of its interruption, from its
/// `events`; `None` for a run with neither.
pub fn run_error_code(run: &TaskRun, events: &[RunEvent]) -> Option<ReasonCode> {
    (run.last_error().is_some() || run.status() == RunStatus::Interrupted)
        .then(|| last_error_code(events))
        .flatten()
}

/// Events whose code repeats that of another event recorded with them, so
/// counting both would count one failure twice: the park's
/// `evidence_missing` / `scope_violation` beside `validation_finished`,
/// and `backend_call_failed` beside the step that failed with it (which
/// `stats` counts in `backend_failures`).
pub const REPEATED_CODE_KINDS: [&str; 3] =
    ["evidence_missing", "scope_violation", "backend_call_failed"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn every_code_round_trips_through_its_name_and_serde() {
        for code in ReasonCode::ALL {
            assert_eq!(ReasonCode::parse(code.as_str()), Some(*code));
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
            assert!(!code.meaning().is_empty());
            assert_eq!(code.to_string(), code.as_str());
        }
        assert_eq!(ReasonCode::parse("nope"), None);
    }

    #[test]
    fn exit_codes_above_128_are_signals() {
        assert_eq!(ReasonCode::of_exit_code(1), ReasonCode::SessionExitCode);
        assert_eq!(ReasonCode::of_exit_code(127), ReasonCode::SessionExitCode);
        assert_eq!(ReasonCode::of_exit_code(128), ReasonCode::SessionKilled);
        assert_eq!(
            Reason::of_exit_code(128).on(json!({})),
            json!({"code": "session_killed", "exit_code": 128})
        );
        assert_eq!(ReasonCode::of_exit_code(143), ReasonCode::SessionKilled);
        let killed = Reason::of_exit_code(143).on(json!({"exit_code": 143}));
        assert_eq!(
            killed,
            json!({"code": "session_killed", "exit_code": 143, "signal": 15})
        );
        assert_eq!(
            Reason::of_exit_code(2).on(json!(null)),
            json!({"code": "session_exit_code", "exit_code": 2})
        );
    }

    #[test]
    fn backend_errors_that_timed_out_are_timeouts() {
        assert_eq!(
            ReasonCode::of_backend_error("cmux capture-pane failed: Error: Command timed out"),
            ReasonCode::BackendTimeout
        );
        assert_eq!(
            ReasonCode::of_backend_error(
                "\"cmux\" timed out; external resources may have been created"
            ),
            ReasonCode::BackendTimeout
        );
        assert_eq!(
            ReasonCode::of_backend_error("\"cmux\" send did not finish within 30s"),
            ReasonCode::BackendTimeout
        );
        assert_eq!(
            ReasonCode::of_backend_error("workspace not found"),
            ReasonCode::BackendFailed
        );
    }

    #[test]
    fn receipt_errors_map_to_their_codes() {
        use super::super::{CheckStatus, ReceiptResult, RunId};
        let failed = DomainError::AgentReportedResult {
            result: ReceiptResult::Failed,
            summary: String::new(),
        };
        assert_eq!(
            ReasonCode::of_receipt_error(&failed),
            ReasonCode::WorkerFailed
        );
        let check = DomainError::ReceiptCheckFailed {
            check: "e2e",
            evidence_or_reason: String::new(),
        };
        assert_eq!(
            ReasonCode::of_receipt_error(&check),
            ReasonCode::EvidenceFailed
        );
        for error in [
            DomainError::MalformedReceipt {
                reason: String::new(),
            },
            DomainError::ReceiptRunMismatch {
                receipt_run_id: String::new(),
                run_id: RunId::new("00000000-0000-4000-8000-000000000000").unwrap(),
            },
            DomainError::ReceiptCheckUnexplained {
                check: "tests",
                status: CheckStatus::Passed,
            },
            DomainError::FollowUpsNotArray,
        ] {
            assert_eq!(
                ReasonCode::of_receipt_error(&error),
                ReasonCode::ReceiptInvalid
            );
        }
    }

    #[test]
    fn a_reason_keeps_the_payloads_own_values() {
        let reason = Reason::new(ReasonCode::VerificationFailed)
            .with("index", 2)
            .with("exit_code", 1);
        assert_eq!(
            reason.on(json!({"exit_code": 3})),
            json!({"code": "verification_failed", "index": 2, "exit_code": 3})
        );
        assert_eq!(
            Reason::from(ReasonCode::Other).on(json!([1])),
            json!({"code": "other"})
        );
    }

    #[test]
    fn the_last_error_code_is_that_of_the_latest_event_that_set_it() {
        let events = vec![
            event("runtime_error", json!({"message": "from before the codes"})),
            event("supervision_finished", json!({"code": "session_killed"})),
            event(
                "resume_finished",
                json!({"code": "backend_failed", "status": "needs_session"}),
            ),
            event("screen_capture_failed", json!({"code": "backend_timeout"})),
        ];
        assert_eq!(last_error_code(&events), Some(ReasonCode::SessionKilled));
        let mut recovered = events.clone();
        recovered.push(event(
            "run_recovered",
            json!({"code": "orphaned", "status": "awaiting_integration"}),
        ));
        recovered.push(event(
            "cleanup_failed",
            json!({"code": "backend_failed", "by": "triage"}),
        ));
        recovered.push(event(
            "cleanup_failed",
            json!({"code": "other", "by": "supervisor"}),
        ));
        assert_eq!(last_error_code(&recovered), Some(ReasonCode::SessionKilled));
        recovered.push(event(
            "run_recovered",
            json!({"code": "orphaned", "status": "interrupted"}),
        ));
        assert_eq!(last_error_code(&recovered), Some(ReasonCode::Orphaned));
        let mut asked = events.clone();
        asked.push(event("triage_finished", json!({"action": "ask"})));
        assert_eq!(last_error_code(&asked), Some(ReasonCode::SessionKilled));
        let mut failed = events.clone();
        failed.push(event(
            "resume_finished",
            json!({"code": "worker_failed", "status": "failed"}),
        ));
        assert_eq!(last_error_code(&failed), Some(ReasonCode::WorkerFailed));
        // The latest event that set it predates the codes.
        assert_eq!(last_error_code(&events[..1]), None);
        assert_eq!(last_error_code(&events[2..]), None);
        assert_eq!(
            event_code(&event("x", json!({"code": "unknown_code"}))),
            None
        );
    }
}
