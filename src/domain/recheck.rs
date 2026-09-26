//! The recheck of the runs that wait to land after each landing
//! (ADR-0068): what it found for one run, how that is written into the
//! run's events, its asks and its `last_error`, and how the events are
//! read back. The supervisor runs the checks; this module is the record.

use serde_json::{Value, json};

use super::{CommitSha, ReasonCode, RunEvent, RunId, TaskId};

/// A run the recheck found no longer landing on main (ADR-0068 decision
/// 3). Its `action` says what followed: [`RESUMED`] (it was parked for a
/// resume in the same transaction) or [`HELD`] (a session or the landing
/// holds it; it is parked when it would land).
pub const LANDING_RECHECK_FAILED: &str = "landing_recheck_failed";

/// One recheck ended, recorded on the run whose landing moved main: the
/// main it checked against and what it found.
pub const LANDING_RECHECK_FINISHED: &str = "landing_recheck_finished";

/// `action` of a failure that parked the run for a resume.
pub const RESUMED: &str = "resumed";

/// `action` of a failure recorded on a run this supervisor holds in a slot
/// (waiting for the landing slot, or for its session's `/exit`).
pub const HELD: &str = "held";

/// The landing whose commit the recheck checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub run_id: RunId,
    pub task_id: TaskId,
}

/// Why a waiting run no longer lands on main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckFailure {
    /// `git merge-tree` finds these paths conflicting.
    Conflict { paths: Vec<String> },
    /// The tree merged cleanly, but the recheck's command failed on it.
    CheckFailed {
        command: String,
        exit_code: i32,
        log_path: String,
        output_tail: String,
    },
}

impl RecheckFailure {
    /// A conflict is the landing's `rebase_conflict`, a failed command its
    /// `verification_failed` (ADR-0034's codes, ADR-0068 decision 3).
    pub const fn code(&self) -> ReasonCode {
        match self {
            Self::Conflict { .. } => ReasonCode::RebaseConflict,
            Self::CheckFailed { .. } => ReasonCode::VerificationFailed,
        }
    }

    /// The run's `last_error` once parked, and the reason of its resume.
    pub fn reason(&self, landed: &Landed, main: &CommitSha) -> String {
        let after = format!(
            "after task {} (run {}) landed, the landing recheck found",
            landed.task_id, landed.run_id
        );
        match self {
            Self::Conflict { paths } => format!(
                "{after} that main {main} conflicts with the run in {}",
                paths.join(", ")
            ),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                ..
            } => format!(
                "{after} that {command:?} exits with {exit_code} on main {main} with the run merged in (git merges it without a conflict); see {log_path}"
            ),
        }
    }

    /// The payload of [`LANDING_RECHECK_FAILED`], without its `action`.
    pub fn payload(&self, landed: &Landed, main: &CommitSha, head: &CommitSha) -> Value {
        let mut payload = json!({
            "code": self.code(),
            "main": main,
            "head": head,
            "landed_run_id": landed.run_id,
            "landed_task_id": landed.task_id,
        });
        match self {
            Self::Conflict { paths } => payload["conflicts"] = json!(paths),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                output_tail,
            } => {
                payload["command"] = json!(command);
                payload["exit_code"] = json!(exit_code);
                payload["log_path"] = json!(log_path);
                payload["output_tail"] = json!(output_tail);
            }
        }
        payload
    }
}

/// The paragraph the recheck adds to the run's open asks (ADR-0068
/// decision 4): what it found, and that the run is resumed without waiting
/// for the answer, which still applies once the run waits again.
pub fn ask_note(reason: &str, resumed: bool) -> String {
    let next = if resumed {
        "The supervisor resumes the run to bring it onto main without waiting for this answer; once it waits again, the answer applies to the rebased run."
    } else {
        "The supervisor parks the run for a resume instead of landing it once its session has exited."
    };
    format!("Landing recheck: {reason}. {next}")
}

/// Whether the event parked its run for a session.
pub fn parks(event: &RunEvent) -> bool {
    event.kind == LANDING_RECHECK_FAILED && event.payload["action"] == RESUMED
}

/// The failure a recheck recorded as [`HELD`] on the run against `main`
/// with `head`, when it is the run's latest recheck failure: the run would
/// land a head known not to land there.
pub fn held_against<'a>(events: &'a [RunEvent], main: &str, head: &str) -> Option<&'a RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == LANDING_RECHECK_FAILED)
        .filter(|e| {
            e.payload["action"] == HELD && e.payload["main"] == main && e.payload["head"] == head
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn landed() -> Landed {
        Landed {
            run_id: RunId::new("landed-run").unwrap(),
            task_id: TaskId::new(7),
        }
    }

    fn event(payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: LANDING_RECHECK_FAILED.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn a_conflict_names_the_landing_and_the_paths() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::Conflict {
            paths: vec!["x.rs".into(), "y.rs".into()],
        };
        assert_eq!(failure.code(), ReasonCode::RebaseConflict);
        let reason = failure.reason(&landed(), &main);
        assert_eq!(
            reason,
            format!(
                "after task 7 (run landed-run) landed, the landing recheck found that main {main} conflicts with the run in x.rs, y.rs"
            )
        );
        assert_eq!(
            failure.payload(&landed(), &main, &head),
            json!({
                "code": "rebase_conflict",
                "main": main,
                "head": head,
                "landed_run_id": "landed-run",
                "landed_task_id": 7,
                "conflicts": ["x.rs", "y.rs"],
            })
        );
        assert!(ask_note(&reason, true).contains("without waiting for this answer"));
        assert!(ask_note(&reason, false).contains("once its session has exited"));
    }

    #[test]
    fn a_failed_check_names_the_command_and_its_log() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::CheckFailed {
            command: "cargo check".into(),
            exit_code: 101,
            log_path: "/r/recheck.log".into(),
            output_tail: "error[E0063]".into(),
        };
        assert_eq!(failure.code(), ReasonCode::VerificationFailed);
        let reason = failure.reason(&landed(), &main);
        assert!(
            reason.contains("\"cargo check\" exits with 101"),
            "{reason}"
        );
        assert!(reason.ends_with("see /r/recheck.log"), "{reason}");
        let payload = failure.payload(&landed(), &main, &head);
        assert_eq!(payload["code"], "verification_failed");
        assert_eq!(payload["exit_code"], 101);
        assert_eq!(payload["output_tail"], "error[E0063]");
    }

    #[test]
    fn only_the_latest_held_failure_against_the_same_main_and_head_holds() {
        let held =
            |main: &str, head: &str| event(json!({"action": HELD, "main": main, "head": head}));
        let events = [held("m", "h")];
        assert!(held_against(&events, "m", "h").is_some());
        assert!(held_against(&events, "m2", "h").is_none());
        assert!(held_against(&events, "m", "h2").is_none());
        let resumed = event(json!({"action": RESUMED, "main": "m", "head": "h"}));
        assert!(parks(&resumed));
        assert!(!parks(&events[0]));
        assert!(held_against(&[held("m", "h"), resumed], "m", "h").is_none());
    }
}
