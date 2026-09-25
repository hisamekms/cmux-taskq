//! The recovery job (ADR-0047 decisions 39 and 40): the alerts it is
//! started for, the verdict it prints, and which processes belong to a run
//! so that `stop_processes` can touch only them.
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{AskReason, DomainError, parse_json_object};

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

    #[test]
    fn alerts_and_categories_read_as_their_names() {
        assert_eq!(RecoveryAlert::LongBackground.as_str(), "long_background");
        assert_eq!(
            "stalled".parse::<RecoveryAlert>().unwrap(),
            RecoveryAlert::Stalled
        );
    }
}
