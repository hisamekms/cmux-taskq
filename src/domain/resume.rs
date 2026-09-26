//! How the resumes of a `needs_session` run are counted, and when a run
//! whose resumes are used up is retried with its branch carried over
//! (ADR-0047 decision 24). Both are read from the run's events alone.

use super::{MAX_RESUME_ATTEMPTS, ReasonCode, ReviewDecision, RunEvent};

/// How many resumes of one run the supervisor starts while the run was
/// parked only by a rebase conflict after its review passed: such a resume
/// is not one of [`MAX_RESUME_ATTEMPTS`], but a conflict that never
/// resolves stops here (ADR-0047 decision 24's fence).
pub const CONFLICT_ONLY_RESUME_LIMIT: usize = 5;

/// `triage_finished`'s `action` for the automatic retry that carries the
/// run's branch over to the next run (ADR-0047 decisions 24 and 40).
pub const RETRY_INHERIT: &str = "retry_inherit";

/// The events that park a run for a session, or decide what it resumes
/// with: the latest of them before a resume is why it was resumed.
const PARKING: [&str; 7] = [
    "integration_deferred",
    "integration_error",
    "evidence_missing",
    "scope_violation",
    "landing_decided",
    "triage_finished",
    "triage_decided",
];

/// What the events so far say about the run: whether its landing was
/// approved (`integration_approved`, an `integrate` call or a person's
/// `land`), whether its latest review passed, and whether the latest
/// parking event is a landing deferred for a rebase conflict.
#[derive(Debug, Clone, Copy, Default)]
struct History {
    approved: bool,
    passed: bool,
    conflict: bool,
}

impl History {
    fn see(&mut self, event: &RunEvent) {
        match event.kind.as_str() {
            "integration_approved" => self.approved = true,
            "review_finished" => {
                self.passed = event.payload["verdict"] == ReviewDecision::Pass.as_str();
            }
            kind if PARKING.contains(&kind) => {
                self.conflict = kind == "integration_deferred"
                    && event.payload["code"] == ReasonCode::RebaseConflict.as_str();
            }
            _ => {}
        }
    }

    /// The run's review passed (or its landing was approved) and it waits
    /// for a session only because the landing's rebase conflicted with
    /// main: not a failed verification, `evidence_missing`,
    /// `scope_violation`, a person's `send_back` or a triage.
    fn conflict_only(self) -> bool {
        (self.approved || self.passed) && self.conflict
    }
}

fn history(events: &[RunEvent]) -> History {
    let mut history = History::default();
    for event in events {
        history.see(event);
    }
    history
}

/// Whether the run, as its events stand, waits for a session only because
/// of a rebase conflict after its review passed: its next resume is not
/// counted toward [`MAX_RESUME_ATTEMPTS`].
pub fn parked_for_conflict_only(events: &[RunEvent]) -> bool {
    history(events).conflict_only()
}

/// The resumes of one run (`resume_started` events), split by whether
/// each counts toward [`MAX_RESUME_ATTEMPTS`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumeCount {
    /// Resumes that count toward [`MAX_RESUME_ATTEMPTS`].
    pub counted: usize,
    /// Resumes of a run parked only by a rebase conflict after its review
    /// passed, fenced by [`CONFLICT_ONLY_RESUME_LIMIT`].
    pub conflict_only: usize,
}

impl ResumeCount {
    pub fn of(events: &[RunEvent]) -> Self {
        let mut history = History::default();
        let mut count = Self::default();
        for event in events {
            if event.kind == "resume_started" {
                if history.conflict_only() {
                    count.conflict_only += 1;
                } else {
                    count.counted += 1;
                }
            }
            history.see(event);
        }
        count
    }

    /// Every resume started, counted or not: the number of the last one.
    pub const fn total(self) -> usize {
        self.counted + self.conflict_only
    }

    /// How many counted resumes are left.
    pub const fn left(self) -> usize {
        MAX_RESUME_ATTEMPTS.saturating_sub(self.counted)
    }

    /// No further resume starts: the counted ones reached
    /// [`MAX_RESUME_ATTEMPTS`] or the conflict-only ones
    /// [`CONFLICT_ONLY_RESUME_LIMIT`].
    pub const fn exhausted(self) -> bool {
        self.counted >= MAX_RESUME_ATTEMPTS || self.conflict_only >= CONFLICT_ONLY_RESUME_LIMIT
    }
}

/// Whether the event is the automatic retry that carried a run's branch
/// over.
pub fn is_inherit_retry(event: &RunEvent) -> bool {
    event.kind == "triage_finished" && event.payload["action"] == RETRY_INHERIT
}

/// Whether a run whose resumes are used up is retried with its branch
/// carried over, without a person (ADR-0047 decision 24): its review
/// passed and the last reason it waited for a session is a rebase conflict
/// only, and no run of its task (`task_events`) was retried that way
/// before. Otherwise a person decides.
pub fn inherits_on_exhaustion(run_events: &[RunEvent], task_events: &[RunEvent]) -> bool {
    parked_for_conflict_only(run_events) && !task_events.iter().any(is_inherit_retry)
}

/// Whether the run was ended by the automatic retry that carries its
/// branch over: its latest `triage_finished` / `triage_decided` is one.
/// The next run of its task inherits it.
pub fn retried_with_inheritance(run_events: &[RunEvent]) -> bool {
    run_events
        .iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "triage_finished" | "triage_decided"))
        .is_some_and(is_inherit_retry)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(0),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    fn conflict() -> RunEvent {
        event(
            "integration_deferred",
            json!({"code": "rebase_conflict", "conflicts": ["src/lib.rs"]}),
        )
    }

    fn resume() -> RunEvent {
        event("resume_started", json!({}))
    }

    fn pass() -> RunEvent {
        event("review_finished", json!({"verdict": "pass"}))
    }

    #[test]
    fn a_conflict_after_a_passed_review_is_not_counted() {
        let events = [pass(), conflict(), resume(), conflict(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 2
            }
        );
        assert_eq!(count.total(), 2);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        assert!(!count.exhausted());
        assert!(parked_for_conflict_only(&events));
    }

    #[test]
    fn an_approved_landing_counts_as_a_passed_review() {
        let events = [
            event("integration_approved", json!({})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).conflict_only, 1);
    }

    #[test]
    fn a_conflict_without_a_passed_review_is_counted() {
        let events = [conflict(), resume()];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        // A later review that did not pass takes the pass back.
        let events = [
            pass(),
            event("review_finished", json!({"verdict": "revise"})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        assert!(!parked_for_conflict_only(&events));
    }

    #[test]
    fn other_reasons_after_a_passed_review_are_counted() {
        for parking in [
            event(
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            event("integration_error", json!({"code": "rebase_conflict"})),
            event("evidence_missing", json!({})),
            event("scope_violation", json!({})),
            event("landing_decided", json!({"answer": "send_back"})),
            event("triage_finished", json!({"action": "resume"})),
            event("triage_decided", json!({"answer": "resume"})),
        ] {
            let events = [pass(), conflict(), parking.clone(), resume()];
            assert_eq!(ResumeCount::of(&events).counted, 1, "{}", parking.kind);
        }
    }

    #[test]
    fn a_resume_is_judged_by_what_parked_it_before_it() {
        // A conflict recorded after the resume started does not change it.
        let events = [
            pass(),
            event("evidence_missing", json!({})),
            resume(),
            conflict(),
            resume(),
        ];
        assert_eq!(
            ResumeCount::of(&events),
            ResumeCount {
                counted: 1,
                conflict_only: 1
            }
        );
    }

    #[test]
    fn either_limit_uses_the_resumes_up() {
        let counted = ResumeCount {
            counted: MAX_RESUME_ATTEMPTS,
            conflict_only: 0,
        };
        assert!(counted.exhausted());
        assert_eq!(counted.left(), 0);
        let conflicts = ResumeCount {
            counted: 0,
            conflict_only: CONFLICT_ONLY_RESUME_LIMIT,
        };
        assert!(conflicts.exhausted());
        assert!(
            !ResumeCount {
                counted: MAX_RESUME_ATTEMPTS - 1,
                conflict_only: CONFLICT_ONLY_RESUME_LIMIT - 1,
            }
            .exhausted()
        );
    }

    #[test]
    fn a_used_up_conflict_run_inherits_once_per_task() {
        let run = [pass(), conflict(), resume()];
        assert!(inherits_on_exhaustion(&run, &run));
        let inherited = event("triage_finished", json!({"action": RETRY_INHERIT}));
        let task: Vec<RunEvent> = run.iter().cloned().chain([inherited.clone()]).collect();
        assert!(!inherits_on_exhaustion(&run, &task));
        // Not after a reason other than the conflict.
        let failed = [pass(), conflict(), event("evidence_missing", json!({}))];
        assert!(!inherits_on_exhaustion(&failed, &failed));
        assert!(retried_with_inheritance(std::slice::from_ref(&inherited)));
        assert!(!retried_with_inheritance(&[
            inherited,
            event("triage_decided", json!({"answer": "retry"}))
        ]));
        assert!(!retried_with_inheritance(&run));
    }
}
