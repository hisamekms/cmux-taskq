//! The breakdown of `wait_to_land` by phase (goal 36, ADR-0049 decision
//! 5): the time from a run's first `validation_finished` to its
//! `run_integrated`, cut at the events that start each phase of the
//! landing, so that the phases add up to `wait_to_land`. Each event that
//! starts a phase closes the one before; an event that starts none leaves
//! the time with the current phase. The push, which follows
//! `run_integrated`, is measured on its own and is not part of the sum.
use std::collections::HashMap;

use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::Value;

use super::{RunEvent, Summary, median, payload_status};

/// The phases, in the order a landing goes through them.
pub const PHASES: [&str; 9] = [
    // The session's exit and the hand-offs between phases (from
    // `validation_finished`, `review_finished`, a precheck that asks
    // nothing): what is left until the run waits for the slot.
    "exit",
    // The headless review (`review_started` / `review_retried`).
    "review",
    // A revise sent to the live session, through its new receipt's
    // validation (`revise_requested`).
    "revise",
    // The live session resolving what `git merge-tree` found
    // (`conflict_precheck` with `requested: true`).
    "conflict",
    // A person answering an ask of the run, or integrating it by hand after
    // a landing error (`ask_opened`, `review_failed`, `integration_error`).
    "ask",
    // Parked `needs_session` and resumed (an event whose `status` is
    // `needs_session`).
    "resume",
    // Waiting for the single integration slot while another run lands
    // (`landing_queued`, an `approve_landing` answer the runtime applies).
    "landing_queue",
    // `integrate` up to its rebase: the receipt checks and the rebase
    // (`integration_started`).
    "rebase",
    // `integrate` after the rebase: the scope check, `verification_commands`
    // and the commit to main (`integration_rebased`).
    "verify",
];

const EXIT: usize = 0;
const REVIEW: usize = 1;
const REVISE: usize = 2;
const CONFLICT: usize = 3;
const ASK: usize = 4;
const RESUME: usize = 5;
const LANDING_QUEUE: usize = 6;
const REBASE: usize = 7;
const VERIFY: usize = 8;

/// The seconds a run spent in each phase of its wait to land, and in the
/// push after it (null when no push was recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandPhases {
    pub secs: [i64; PHASES.len()],
    pub push: Option<i64>,
}

impl LandPhases {
    /// The phase the most time went to; `None` when none took any.
    pub fn longest(&self) -> Option<&'static str> {
        let (index, secs) = self
            .secs
            .iter()
            .enumerate()
            .max_by_key(|&(index, secs)| (*secs, std::cmp::Reverse(index)))?;
        (*secs > 0).then_some(PHASES[index])
    }
}

impl Serialize for LandPhases {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 1))?;
        for (name, secs) in PHASES.iter().zip(self.secs) {
            map.serialize_entry(name, &secs)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.end()
    }
}

/// One phase over a set of runs: `Summary`'s count, sum and median, the
/// 90th percentile and the maximum, and the sum over the tail runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PhaseSummary {
    #[serde(flatten)]
    pub summary: Summary,
    pub p90: Option<i64>,
    pub max: Option<i64>,
    /// The sum over the runs whose `wait_to_land` is at least
    /// [`LandBreakdown::tail_threshold`].
    pub tail_total: i64,
}

/// The phases over a set of runs, with the long tail of `wait_to_land`
/// (the runs at or above its 90th percentile) and what each phase
/// contributed to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandBreakdown {
    /// Runs with a breakdown (those that landed).
    pub runs: usize,
    /// The 90th percentile of those runs' `wait_to_land`.
    pub tail_threshold: Option<i64>,
    pub tail_runs: usize,
    pub phases: [PhaseSummary; PHASES.len()],
    pub push: PhaseSummary,
}

impl Serialize for LandBreakdown {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 4))?;
        map.serialize_entry("runs", &self.runs)?;
        map.serialize_entry("tail_threshold", &self.tail_threshold)?;
        map.serialize_entry("tail_runs", &self.tail_runs)?;
        for (name, phase) in PHASES.iter().zip(&self.phases) {
            map.serialize_entry(name, phase)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.end()
    }
}

/// The nearest-rank 90th percentile of `values` (sorted in place).
pub fn p90(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    let rank = (values.len() * 9).div_ceil(10);
    values.get(rank.checked_sub(1)?).copied()
}

fn phase_summary(values: &[(i64, bool)]) -> PhaseSummary {
    let mut all: Vec<i64> = values.iter().map(|(secs, _)| *secs).collect();
    PhaseSummary {
        summary: Summary {
            count: all.len(),
            total: all.iter().sum(),
            median: median(&mut all),
        },
        p90: p90(&mut all),
        max: all.iter().copied().max(),
        tail_total: values
            .iter()
            .filter(|(_, tail)| *tail)
            .map(|(secs, _)| secs)
            .sum(),
    }
}

/// Aggregate the breakdowns of runs, each with its `wait_to_land`.
pub fn breakdown<'a>(runs: impl Iterator<Item = (&'a LandPhases, i64)>) -> LandBreakdown {
    let runs: Vec<(&LandPhases, i64)> = runs.collect();
    let tail_threshold = p90(&mut runs.iter().map(|(_, wait)| *wait).collect::<Vec<_>>());
    let tail = |wait: i64| tail_threshold.is_some_and(|threshold| wait >= threshold);
    LandBreakdown {
        runs: runs.len(),
        tail_threshold,
        tail_runs: runs.iter().filter(|(_, wait)| tail(*wait)).count(),
        phases: std::array::from_fn(|index| {
            phase_summary(
                &runs
                    .iter()
                    .map(|(phases, wait)| (phases.secs[index], tail(*wait)))
                    .collect::<Vec<_>>(),
            )
        }),
        push: phase_summary(
            &runs
                .iter()
                .filter_map(|(phases, wait)| Some((phases.push?, tail(*wait))))
                .collect::<Vec<_>>(),
        ),
    }
}

/// A run's wait to land as its events go by, from its first
/// `validation_finished` (unix milliseconds throughout).
#[derive(Debug, Clone)]
pub struct LandClock {
    phase: usize,
    since: i64,
    spent: [i64; PHASES.len()],
    /// The phase an open ask interrupted, to go back to once it is answered.
    before_ask: Option<usize>,
    /// The run's open asks, by `ask_id`, with their kind.
    asks: HashMap<String, Option<String>>,
    integrated: Option<i64>,
    push: Option<i64>,
}

fn ask_key(payload: &Value) -> Option<String> {
    payload
        .get("ask_id")
        .or_else(|| payload.get("id"))
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
}

impl LandClock {
    /// Start at the run's first `validation_finished`, in the phase that
    /// event starts (`resume` when it parked the run).
    pub fn start(event: &RunEvent, at: i64) -> Self {
        let mut clock = Self {
            phase: EXIT,
            since: at,
            spent: [0; PHASES.len()],
            before_ask: None,
            asks: HashMap::new(),
            integrated: None,
            push: None,
        };
        clock.observe(event, at);
        clock
    }

    /// Take the run's next event, recorded at `at`.
    pub fn observe(&mut self, event: &RunEvent, at: i64) {
        let kind = event.kind.as_str();
        if let Some(integrated) = self.integrated {
            if self.push.is_none()
                && matches!(kind, "push_finished" | "push_failed" | "push_skipped")
            {
                self.push = Some((at - integrated).max(0) / 1000);
            }
            return;
        }
        let next = if kind == "run_integrated" {
            self.enter(self.phase, at);
            self.integrated = Some(at);
            return;
        } else if payload_status(&event.payload) == Some("needs_session") {
            Some(RESUME)
        } else {
            match kind {
                "review_started" | "review_retried" => Some(REVIEW),
                "review_finished" | "validation_finished" => Some(EXIT),
                "review_failed" => Some(ASK),
                "revise_requested" => Some(REVISE),
                "conflict_precheck" if event.payload["requested"] == true => Some(CONFLICT),
                "conflict_precheck" => Some(EXIT),
                "landing_queued" => Some(LANDING_QUEUE),
                // The landing gave the lease back: the run waits for a
                // person's `review and integrate`, not for the slot.
                "integration_error" => Some(ASK),
                "integration_started" => Some(REBASE),
                "integration_rebased" => Some(VERIFY),
                // The observer's `blocked` and a planner's question are
                // about the run but do not hold it (like the timeline's gaps).
                "ask_opened"
                    if matches!(
                        event.payload["kind"].as_str(),
                        Some("blocked" | "planner_question")
                    ) =>
                {
                    None
                }
                "ask_opened" => {
                    let opened = event.payload.get("kind").and_then(Value::as_str);
                    if let Some(key) = ask_key(&event.payload) {
                        self.asks.insert(key, opened.map(str::to_owned));
                    }
                    if self.phase != ASK {
                        self.before_ask = Some(self.phase);
                    }
                    Some(ASK)
                }
                "ask_answered" => {
                    let opened = ask_key(&event.payload).and_then(|key| self.asks.remove(&key));
                    let answered = event
                        .payload
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(opened.flatten());
                    if answered.as_deref() == Some("approve_landing") {
                        // `land` waits for the slot; `send_back` and
                        // `cancel` record their own status next. Another
                        // answer is the inbox's to read: still a person's
                        // wait.
                        self.asks.clear();
                        self.before_ask = None;
                        Some(if event.payload["runtime_delivers"] == false {
                            ASK
                        } else {
                            LANDING_QUEUE
                        })
                    } else if self.asks.is_empty() {
                        self.before_ask.take()
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some(next) = next {
            if next != ASK && kind != "ask_answered" {
                self.asks.clear();
                self.before_ask = None;
            }
            self.enter(next, at);
        }
    }

    fn enter(&mut self, phase: usize, at: i64) {
        self.spent[self.phase] += (at - self.since).max(0);
        self.since = at.max(self.since);
        self.phase = phase;
    }

    /// Whether `run_integrated` ended the wait.
    pub fn landed(&self) -> bool {
        self.integrated.is_some()
    }

    /// The seconds per phase: up to `run_integrated` for a run that landed,
    /// up to `now` for one still waiting.
    pub fn phases(&self, now: i64) -> LandPhases {
        let mut spent = self.spent;
        if self.integrated.is_none() {
            spent[self.phase] += (now - self.since).max(0);
        }
        LandPhases {
            secs: spent.map(|ms| ms / 1000),
            push: self.push,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, TaskId};
    use serde_json::json;

    fn event(index: usize, kind: &str, payload: &Value) -> RunEvent {
        RunEvent {
            id: EventId::new(index as i64 + 1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload: payload.clone(),
            created_at: String::new(),
        }
    }

    /// Feed `(kind, payload, second)` to a clock started at second 0 by the
    /// first event when it is a `validation_finished`, by one that left the
    /// run awaiting integration otherwise, and return the phases, keyed by
    /// name, at second `now`.
    fn phases(events: &[(&str, Value, i64)], now: i64) -> (HashMap<&'static str, i64>, LandPhases) {
        let awaiting = (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
            0,
        );
        let (first, rest) = match events.split_first() {
            Some((first, rest)) if first.0 == "validation_finished" => (first, rest),
            _ => (&awaiting, events),
        };
        let mut clock = LandClock::start(&event(0, first.0, &first.1), first.2 * 1000);
        for (index, (kind, payload, secs)) in rest.iter().enumerate() {
            clock.observe(&event(index + 1, kind, payload), secs * 1000);
        }
        let phases = clock.phases(now * 1000);
        (
            PHASES
                .iter()
                .zip(phases.secs)
                .filter(|(_, secs)| *secs > 0)
                .map(|(name, secs)| (*name, secs))
                .collect(),
            phases,
        )
    }

    fn map(pairs: &[(&'static str, i64)]) -> HashMap<&'static str, i64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_passed_review_splits_into_review_exit_queue_integrate_and_push() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({"attempt": 1}), 2),
                ("review_finished", json!({"verdict": "pass"}), 62),
                ("exit_requested", json!({}), 63),
                ("landing_queued", json!({}), 70),
                ("integration_started", json!({}), 170),
                ("integration_rebased", json!({}), 175),
                ("verification_command", json!({"exit_code": 0}), 400),
                ("run_integrated", json!({}), 410),
                ("worktree_removed", json!({}), 411),
                ("push_finished", json!({}), 414),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("exit", 2 + 8),
                ("review", 60),
                ("landing_queue", 100),
                ("rebase", 5),
                ("verify", 235),
            ])
        );
        assert_eq!(phases.secs.iter().sum::<i64>(), 410);
        assert_eq!(phases.push, Some(4));
        assert_eq!(phases.longest(), Some("verify"));
    }

    #[test]
    fn a_revise_counts_until_the_next_review() {
        let (spent, _) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "revise"}), 50),
                ("revise_requested", json!({"attempt": 1}), 51),
                ("revise_finished", json!({"attempt": 1}), 300),
                ("receipt_observed", json!({}), 301),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    311,
                ),
                ("review_started", json!({"attempt": 2}), 312),
                ("review_finished", json!({"verdict": "pass"}), 342),
                ("landing_queued", json!({}), 350),
                ("integration_started", json!({}), 350),
                ("integration_rebased", json!({}), 351),
                ("run_integrated", json!({}), 400),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("review", 80),
                ("exit", 1 + 1 + 8),
                ("revise", 260),
                ("rebase", 1),
                ("verify", 49),
            ])
        );
    }

    #[test]
    fn a_concern_waits_for_the_person_then_for_the_slot() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "concern"}), 30),
                (
                    "ask_opened",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    40,
                ),
                (
                    "ask_answered",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    3640,
                ),
                ("integration_approved", json!({}), 3700),
                ("integration_started", json!({}), 3700),
                ("integration_rebased", json!({}), 3701),
                ("run_integrated", json!({}), 3800),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("review", 30),
                ("exit", 10),
                ("ask", 3600),
                ("landing_queue", 60),
                ("rebase", 1),
                ("verify", 99),
            ])
        );
        assert_eq!(phases.longest(), Some("ask"));
        assert_eq!(phases.push, None);
    }

    #[test]
    fn a_question_during_a_revise_returns_to_the_revise() {
        let (spent, _) = phases(
            &[
                ("revise_requested", json!({}), 0),
                (
                    "ask_opened",
                    json!({"ask_id": "a", "kind": "worker_question"}),
                    100,
                ),
                ("ask_opened", json!({"ask_id": "b", "kind": "stalled"}), 150),
                ("ask_answered", json!({"ask_id": "a"}), 200),
                ("ask_answered", json!({"ask_id": "b"}), 250),
                ("run_integrated", json!({}), 300),
            ],
            9999,
        );
        assert_eq!(spent, map(&[("revise", 150), ("ask", 150)]));
    }

    #[test]
    fn a_resume_and_a_retried_landing_are_their_own_phases() {
        let (spent, _) = phases(
            &[
                ("landing_queued", json!({}), 0),
                ("integration_started", json!({}), 10),
                (
                    "integration_deferred",
                    json!({"status": "needs_session", "code": "verification_failed"}),
                    20,
                ),
                ("resume_started", json!({}), 80),
                ("resume_finished", json!({"status": "validating"}), 500),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    505,
                ),
                ("landing_queued", json!({}), 510),
                ("integration_started", json!({}), 610),
                (
                    "integration_error",
                    json!({"status": "awaiting_integration"}),
                    615,
                ),
                ("integration_started", json!({}), 700),
                ("integration_rebased", json!({}), 720),
                ("run_integrated", json!({}), 900),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("landing_queue", 10 + 100),
                ("ask", 85),
                ("rebase", 10 + 5 + 20),
                ("resume", 485),
                ("exit", 5),
                ("verify", 180),
            ])
        );
    }

    #[test]
    fn a_first_validation_that_parks_the_run_starts_in_resume() {
        let (spent, _) = phases(
            &[
                ("validation_finished", json!({"status": "needs_session"}), 0),
                ("evidence_missing", json!({"checks": ["e2e"]}), 0),
                ("resume_started", json!({}), 60),
                ("resume_finished", json!({"status": "validating"}), 600),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    610,
                ),
                ("review_started", json!({}), 611),
                ("review_finished", json!({"verdict": "pass"}), 641),
                ("run_integrated", json!({}), 700),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[("resume", 610), ("exit", 1 + 59), ("review", 30)])
        );
    }

    #[test]
    fn asks_that_do_not_hold_the_run_or_go_to_the_inbox() {
        let (spent, _) = phases(
            &[
                ("landing_queued", json!({}), 0),
                ("ask_opened", json!({"ask_id": 1, "kind": "blocked"}), 10),
                ("ask_answered", json!({"ask_id": 1, "kind": "blocked"}), 20),
                (
                    "ask_opened",
                    json!({"ask_id": 2, "kind": "planner_question"}),
                    30,
                ),
                ("integration_started", json!({}), 40),
                ("integration_rebased", json!({}), 41),
                (
                    "integration_deferred",
                    json!({"status": "needs_session"}),
                    50,
                ),
                ("resume_started", json!({}), 51),
                (
                    "ask_opened",
                    json!({"ask_id": 3, "kind": "approve_landing"}),
                    100,
                ),
                (
                    "ask_answered",
                    json!({"ask_id": 3, "kind": "approve_landing", "runtime_delivers": false}),
                    200,
                ),
                ("integration_approved", json!({}), 300),
                ("integration_started", json!({}), 300),
                ("integration_rebased", json!({}), 301),
                ("run_integrated", json!({}), 310),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("landing_queue", 40),
                ("rebase", 2),
                ("verify", 9 + 9),
                ("resume", 50),
                ("ask", 200),
            ])
        );
    }

    #[test]
    fn a_conflict_precheck_that_asks_the_session_is_the_conflict_phase() {
        let (spent, _) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "pass"}), 10),
                ("conflict_precheck", json!({"requested": true}), 11),
                ("conflict_resolved", json!({}), 200),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    210,
                ),
                ("review_started", json!({}), 211),
                ("review_finished", json!({"verdict": "pass"}), 221),
                ("conflict_precheck", json!({"requested": false}), 221),
                ("run_integrated", json!({}), 230),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[("review", 20), ("exit", 1 + 1 + 9), ("conflict", 199)])
        );
    }

    #[test]
    fn a_failed_review_and_a_run_still_waiting_count_up_to_now() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({}), 0),
                ("ask_opened", json!({"ask_id": 3}), 40),
                ("review_failed", json!({"ask_id": 3}), 41),
            ],
            1000,
        );
        assert_eq!(spent, map(&[("review", 40), ("ask", 960)]));
        assert_eq!(phases.longest(), Some("ask"));
        assert_eq!(LandPhases::default().longest(), None);
    }

    #[test]
    fn breakdowns_sum_and_mark_the_tail() {
        let run = |exit: i64, verify: i64, push: Option<i64>| LandPhases {
            secs: std::array::from_fn(|index| match index {
                EXIT => exit,
                VERIFY => verify,
                _ => 0,
            }),
            push,
        };
        let runs: Vec<LandPhases> = (1..=10)
            .map(|n| {
                run(
                    n,
                    if n == 10 { 1000 } else { 10 },
                    (n % 2 == 0).then_some(n),
                )
            })
            .collect();
        let result = breakdown(runs.iter().map(|phases| (phases, phases.secs.iter().sum())));
        assert_eq!(result.runs, 10);
        assert_eq!(result.tail_threshold, Some(19));
        assert_eq!(result.tail_runs, 2);
        let verify = &result.phases[VERIFY];
        assert_eq!(verify.summary.total, 1090);
        assert_eq!(verify.summary.median, Some(10));
        assert_eq!((verify.p90, verify.max), (Some(10), Some(1000)));
        assert_eq!(verify.tail_total, 1010);
        assert_eq!(result.push.summary.count, 5);
        assert_eq!(result.push.tail_total, 10);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["verify"]["count"], 10);
        assert_eq!(json["verify"]["p90"], 10);
        assert_eq!(json["tail_runs"], 2);
        assert_eq!(json["review"]["median"], 0);
        assert_eq!(serde_json::to_value(&runs[0]).unwrap()["push"], Value::Null);
        assert_eq!(breakdown(std::iter::empty()).tail_threshold, None);
        assert_eq!(p90(&mut []), None);
    }
}
