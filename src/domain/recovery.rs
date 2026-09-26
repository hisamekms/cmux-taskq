//! The recovery job (ADR-0047 decisions 39 and 40): the alerts it is
//! started for, the verdict it prints, and which processes belong to a run
//! so that `stop_processes` can touch only them.
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{AskKind, AskReason, DomainError, RunEvent, RunStatus, parse_json_object};

// The alert a recovery job is started for (`recovery_requested`'s `alert`,
// ADR-0047 decision 39).
string_enum!(RecoveryAlert {
    Failed => "failed",
    Interrupted => "interrupted",
    ResumeExhausted => "resume_exhausted",
    StuckExit => "stuck_exit",
    PromptWaiting => "prompt_waiting",
    Stalled => "stalled",
    LongBackground => "long_background",
});

string_enum!(RecoveryDecision {
    Repair => "repair",
    Escalate => "escalate",
});

string_enum!(RecoveryConfidence {
    High => "high",
    Low => "low",
});

/// How many recovery jobs one run gets per kind of alert; past it the
/// alert goes to the inbox as `recovery_failed` (ADR-0047 decision 39).
pub const MAX_RECOVERY_ATTEMPTS: usize = 3;

/// The longest `wait` a recovery job may ask for (ADR-0047 decision 40).
pub const MAX_RECHECK_SECS: u64 = 3600;

/// The actions a recovery job may choose for a run that ended `failed` or
/// `interrupted` (its session is gone): the ones that move the run or its
/// task on, and `wait`.
pub const ENDED_ACTIONS: [&str; 4] = ["retry", "retry_inherit", "resume", "wait"];

/// The actions for a session that holds the supervisor's `/exit` back
/// (`stuck_exit`): `close_and_proceed` only where the run lands after its
/// exit, which the runtime checks.
pub const STUCK_EXIT_ACTIONS: [&str; 4] = [
    "answer_known_dialog",
    "close_and_proceed",
    "stop_processes",
    "wait",
];

/// The actions for a session held by a dialog (`prompt_waiting`).
pub const PROMPT_WAITING_ACTIONS: [&str; 3] = ["answer_known_dialog", "stop_processes", "wait"];

impl RecoveryAlert {
    /// The alert of a run that ended `failed` or `interrupted` without a
    /// pending request of its own (`resume_exhausted` records one).
    pub fn of_ended(status: RunStatus) -> Self {
        match status {
            RunStatus::Interrupted => Self::Interrupted,
            _ => Self::Failed,
        }
    }

    /// The kind of the ask an escalation of this alert opens (ADR-0047
    /// decision 40): `decide` for a run that ended, the kind the runtime
    /// raised before the recovery job for a live session.
    pub fn ask_kind(self) -> AskKind {
        match self {
            Self::Failed | Self::Interrupted | Self::ResumeExhausted => AskKind::Decide,
            Self::StuckExit => AskKind::StuckExit,
            Self::PromptWaiting => AskKind::AnswerPrompt,
            Self::Stalled | Self::LongBackground => AskKind::Stalled,
        }
    }
}

/// How many recovery jobs the run's `events` requested for `alert`.
pub fn attempts(events: &[RunEvent], alert: RecoveryAlert) -> usize {
    events
        .iter()
        .filter(|e| e.kind == "recovery_requested" && e.payload["alert"] == alert.as_str())
        .count()
}

/// The `recovery_requested` of a run that ended which no round took yet: the
/// latest one after the latest `triage_started` and `resume_started` (the
/// runtime records it when a run's resumes are used up).
pub fn pending_request(events: &[RunEvent]) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .take_while(|e| !matches!(e.kind.as_str(), "triage_started" | "resume_started"))
        .find(|e| e.kind == "recovery_requested")
}

/// The alert of the run's recovery since its last `resume_started`: that
/// of its latest `recovery_requested`, so a round after a `wait` or a
/// handoff goes on with the alert the first one was requested for.
pub fn current_alert(events: &[RunEvent]) -> Option<RecoveryAlert> {
    events
        .iter()
        .rev()
        .take_while(|e| e.kind != "resume_started")
        .find(|e| e.kind == "recovery_requested")
        .and_then(|e| e.payload["alert"].as_str())
        .and_then(|alert| alert.parse().ok())
}

/// The events after which a live session's failed recovery job
/// (`recovery_failed`) no longer waits for a person: the session ended or
/// was closed, the run was given up, or (for its own alert) what raised the
/// alert went away.
fn clears(event: &RunEvent, alert: &str) -> bool {
    matches!(
        event.kind.as_str(),
        "session_exited"
            | "workspace_closed"
            | "supervision_finished"
            | "run_recovered"
            | "runtime_error"
    ) || (alert == RecoveryAlert::PromptWaiting.as_str() && event.kind == "prompt_cleared")
        || (alert == RecoveryAlert::LongBackground.as_str() && event.kind == "receipt_observed")
}

/// The latest `recovery_failed` of a live session (of `alert`, or any)
/// that still waits for a person to recover the session by hand: nothing
/// after it cleared it (see [`clears`]). While it waits, the runtime starts
/// no other job for that alert.
pub fn failed_live(events: &[RunEvent], alert: Option<RecoveryAlert>) -> Option<&RunEvent> {
    let (index, failed) = events.iter().enumerate().rev().find(|(_, e)| {
        e.kind == "recovery_failed"
            && alert.is_none_or(|alert| e.payload["alert"] == alert.as_str())
    })?;
    let failed_alert = failed.payload["alert"].as_str().unwrap_or_default();
    (!events[index + 1..].iter().any(|e| clears(e, failed_alert))).then_some(failed)
}

/// One operation of a `repair` verdict (ADR-0047 decision 40). The runtime
/// checks each one's preconditions again when it applies the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryAction {
    Retry,
    RetryInherit,
    Resume {
        #[serde(default)]
        instruction: String,
    },
    SendInstruction {
        instruction: String,
    },
    StopProcesses {
        pids: Vec<u32>,
    },
    AnswerKnownDialog {
        #[serde(default)]
        dialog: String,
    },
    CloseAndProceed,
    Wait {
        recheck_after_secs: u64,
    },
}

impl RecoveryAction {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::RetryInherit => "retry_inherit",
            Self::Resume { .. } => "resume",
            Self::SendInstruction { .. } => "send_instruction",
            Self::StopProcesses { .. } => "stop_processes",
            Self::AnswerKnownDialog { .. } => "answer_known_dialog",
            Self::CloseAndProceed => "close_and_proceed",
            Self::Wait { .. } => "wait",
        }
    }
}

/// What the recovery job prints on stdout: one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryVerdict {
    pub verdict: RecoveryDecision,
    pub confidence: RecoveryConfidence,
    pub diagnosis: String,
    #[serde(default)]
    pub actions: Vec<RecoveryAction>,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub reason_category: Option<AskReason>,
}

impl RecoveryVerdict {
    /// The verdict in the job's stdout, found the way the review's is.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        let verdict: Self = parse_json_object(stdout)
            .map_err(|error| format!("the recovery job printed no verdict JSON: {error}"))?;
        if verdict.verdict == RecoveryDecision::Repair && verdict.actions.is_empty() {
            return Err("the recovery job's repair verdict names no action".to_owned());
        }
        Ok(verdict)
    }

    /// Whether the runtime applies it: a `repair` of high confidence. A
    /// `repair` of low confidence is an `escalate` with its actions as the
    /// recommendation (ADR-0047 decision 40).
    pub fn applies(&self) -> bool {
        self.verdict == RecoveryDecision::Repair && self.confidence == RecoveryConfidence::High
    }
}

/// A process as the runtime lists it for the recovery job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub elapsed_secs: u64,
    pub command: String,
    /// The working directory; `None` when it could not be read.
    pub cwd: Option<String>,
}

/// Whether `path` is `root` or under it.
fn under(path: &str, root: &Path) -> bool {
    Path::new(path).starts_with(root)
}

/// The processes of a run that `stop_processes` may stop (ADR-0047
/// decision 40): those whose working directory is in the run's worktree or
/// that descend from its session wrapper, but neither the wrapper nor the
/// agent, nor anything they run under (the terminal that started the
/// wrapper), nor `except` (the supervisor itself), its ancestors or its
/// descendants, nor pid 1.
pub fn run_processes<'a>(
    all: &'a [ProcessInfo],
    worktree: &Path,
    wrapper: Option<u32>,
    agent: Option<u32>,
    except: u32,
) -> Vec<&'a ProcessInfo> {
    let parent = |pid: u32| all.iter().find(|p| p.pid == pid).map(|p| p.ppid);
    // The chain of parents from `pid` up, `pid` included.
    let chain = |pid: u32| {
        let mut chain = vec![pid];
        let mut current = pid;
        while let Some(next) = parent(current) {
            if next <= 1 || chain.contains(&next) {
                break;
            }
            chain.push(next);
            current = next;
        }
        chain
    };
    let supervisor = chain(except);
    // A wrapper that is the supervisor or runs above it (as in a process
    // that hosts both) makes nothing the run's by descent.
    let wrapper = wrapper.filter(|pid| !supervisor.contains(pid));
    let session: Vec<u32> = [wrapper, agent].into_iter().flatten().collect();
    let protected: Vec<u32> = session
        .iter()
        .flat_map(|pid| chain(*pid))
        .chain(supervisor)
        .collect();
    all.iter()
        // The supervisor's own children (its review job runs in the
        // worktree) are never the run's.
        .filter(|p| p.pid > 1 && !protected.contains(&p.pid) && !chain(p.pid).contains(&except))
        .filter(|p| {
            p.cwd.as_deref().is_some_and(|cwd| under(cwd, worktree))
                || wrapper.is_some_and(|wrapper| chain(p.pid).contains(&wrapper))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32, cwd: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 5,
            command: format!("cmd {pid}"),
            cwd: (!cwd.is_empty()).then(|| cwd.to_owned()),
        }
    }

    #[test]
    fn a_verdict_is_parsed_with_its_actions() {
        let verdict = RecoveryVerdict::parse(
            r#"Here: {"verdict": "repair", "confidence": "high", "diagnosis": "orphan", "actions": [{"action": "stop_processes", "pids": [7, 8]}, {"action": "wait", "recheck_after_secs": 60}, {"action": "retry"}]}"#,
        )
        .unwrap();
        assert!(verdict.applies());
        assert_eq!(
            verdict.actions,
            [
                RecoveryAction::StopProcesses { pids: vec![7, 8] },
                RecoveryAction::Wait {
                    recheck_after_secs: 60
                },
                RecoveryAction::Retry,
            ]
        );
        assert_eq!(
            verdict
                .actions
                .iter()
                .map(RecoveryAction::name)
                .collect::<Vec<_>>(),
            ["stop_processes", "wait", "retry"]
        );
        let low = RecoveryVerdict::parse(
            r#"{"verdict": "repair", "confidence": "low", "diagnosis": "?", "actions": [{"action": "resume"}], "reason_category": "recovery_failed"}"#,
        )
        .unwrap();
        assert!(!low.applies());
        assert_eq!(low.reason_category, Some(AskReason::RecoveryFailed));
    }

    #[test]
    fn a_verdict_with_unknown_fields_or_no_action_is_refused() {
        for text in [
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": []}"#,
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "cancel"}]}"#,
            r#"{"verdict": "escalate", "confidence": "high", "diagnosis": "x", "extra": 1}"#,
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "stop_processes", "pids": [1], "signal": 9}]}"#,
            "no json",
        ] {
            assert!(RecoveryVerdict::parse(text).is_err(), "{text}");
        }
        let escalate = RecoveryVerdict::parse(
            r#"{"verdict": "escalate", "confidence": "high", "diagnosis": "x"}"#,
        )
        .unwrap();
        assert!(!escalate.applies());
    }

    #[test]
    fn only_the_runs_own_processes_may_be_stopped() {
        let worktree = Path::new("/runs/r/worktree");
        let all = [
            process(1, 0, "/"),
            // The terminal that runs the wrapper, in the worktree.
            process(10, 1, "/runs/r/worktree"),
            process(11, 10, "/runs/r"),          // wrapper
            process(12, 11, "/runs/r/worktree"), // agent
            process(13, 12, "/runs/r/worktree/src"),
            process(14, 13, ""), // a descendant whose cwd is unreadable
            process(20, 1, "/runs/r/worktree"), // orphan
            process(21, 1, "/runs/other/worktree"),
            process(22, 1, "/runs/r/worktree-2"),
            process(30, 1, "/runs/r/worktree"), // the supervisor
            process(31, 30, "/repo"),
        ];
        let pids: Vec<u32> = run_processes(&all, worktree, Some(11), Some(12), 31)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [13, 14, 20]);
        // Without a wrapper, only the working directory counts.
        let pids: Vec<u32> = run_processes(&all, worktree, None, None, 31)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [10, 12, 13, 20]);
        // A wrapper hosted by the supervisor's own process: none of the
        // supervisor's children are the run's, even in the worktree (its
        // review job), while an orphan there is.
        let hosted = [
            process(30, 1, "/repo"),
            process(31, 30, "/tmp"),
            process(32, 30, "/runs/r/worktree"),
            process(33, 1, "/runs/r/worktree"),
        ];
        let pids: Vec<u32> = run_processes(&hosted, worktree, Some(30), None, 30)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [33]);
    }

    fn event(kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn requests_are_counted_by_alert_and_one_is_pending_until_a_round_takes_it() {
        let exhausted = event(
            "recovery_requested",
            serde_json::json!({"alert": "resume_exhausted"}),
        );
        let failed = event("recovery_requested", serde_json::json!({"alert": "failed"}));
        let events = vec![
            failed.clone(),
            event("triage_started", serde_json::json!({})),
            event("resume_started", serde_json::json!({})),
            exhausted.clone(),
        ];
        assert_eq!(attempts(&events, RecoveryAlert::Failed), 1);
        assert_eq!(attempts(&events, RecoveryAlert::ResumeExhausted), 1);
        assert_eq!(attempts(&events, RecoveryAlert::StuckExit), 0);
        let alert = |events: &[RunEvent]| {
            pending_request(events).map(|e| e.payload["alert"].as_str().unwrap().to_owned())
        };
        assert_eq!(alert(&events).as_deref(), Some("resume_exhausted"));
        let taken = [exhausted, event("triage_started", serde_json::json!({}))];
        assert_eq!(alert(&taken), None);
        assert_eq!(
            alert(std::slice::from_ref(&failed)).as_deref(),
            Some("failed")
        );
        // A round after a wait keeps the alert; a resume starts afresh.
        assert_eq!(current_alert(&taken), Some(RecoveryAlert::ResumeExhausted));
        let resumed = [
            taken[0].clone(),
            event("resume_started", serde_json::json!({})),
        ];
        assert_eq!(current_alert(&resumed), None);
    }

    #[test]
    fn a_failed_live_job_waits_until_the_session_moves_on() {
        let failed = |alert: &str| {
            event(
                "recovery_failed",
                serde_json::json!({"alert": alert, "attempt": 1}),
            )
        };
        let stuck = vec![failed("stuck_exit")];
        assert!(failed_live(&stuck, None).is_some());
        assert!(failed_live(&stuck, Some(RecoveryAlert::StuckExit)).is_some());
        assert!(failed_live(&stuck, Some(RecoveryAlert::PromptWaiting)).is_none());
        let mut exited = stuck.clone();
        exited.push(event("session_exited", serde_json::json!({})));
        assert!(failed_live(&exited, None).is_none());
        // A cleared dialog clears only the dialog's alert.
        let mut cleared = vec![failed("prompt_waiting")];
        cleared.push(event("prompt_cleared", serde_json::json!({})));
        assert!(failed_live(&cleared, None).is_none());
        let mut other = stuck;
        other.push(event("prompt_cleared", serde_json::json!({})));
        assert!(failed_live(&other, None).is_some());
        let mut received = vec![failed("long_background")];
        received.push(event("receipt_observed", serde_json::json!({})));
        assert!(failed_live(&received, None).is_none());
    }

    #[test]
    fn each_alert_escalates_to_its_kind() {
        assert_eq!(
            RecoveryAlert::of_ended(RunStatus::Interrupted),
            RecoveryAlert::Interrupted
        );
        assert_eq!(
            RecoveryAlert::of_ended(RunStatus::Failed),
            RecoveryAlert::Failed
        );
        for (alert, kind) in [
            (RecoveryAlert::Failed, AskKind::Decide),
            (RecoveryAlert::Interrupted, AskKind::Decide),
            (RecoveryAlert::ResumeExhausted, AskKind::Decide),
            (RecoveryAlert::StuckExit, AskKind::StuckExit),
            (RecoveryAlert::PromptWaiting, AskKind::AnswerPrompt),
            (RecoveryAlert::Stalled, AskKind::Stalled),
            (RecoveryAlert::LongBackground, AskKind::Stalled),
        ] {
            assert_eq!(alert.ask_kind(), kind);
        }
    }

    #[test]
    fn alerts_and_categories_read_as_their_names() {
        assert_eq!(RecoveryAlert::LongBackground.as_str(), "long_background");
        assert_eq!(
            "stalled".parse::<RecoveryAlert>().unwrap(),
            RecoveryAlert::Stalled
        );
    }
}
