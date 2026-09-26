//! `timeline RUN` (ADR-0044 decision 22): the gaps in a run's events and
//! why the run was waiting through each, derived from the run's events by
//! fixed rules. No LLM and no run directory: a gap gets the reason the
//! events before it show, `unknown` when none does.
//!
//! The rules, first match wins, on the state after the event the gap
//! starts from:
//!
//! - `no_supervisor`: the gap ends at a `run_adopted` or `run_recovered`
//!   (no supervisor held the run until another took it over).
//! - `waiting_ask`: an ask of the run that holds it (every kind but the
//!   observer's `blocked` and the planner's `planner_question`) is open.
//! - `integrating`: `integrate` is rebasing and verifying the run.
//! - In a session the supervisor watches (the worker's own after
//!   `agent_started`, a `resume_started`, a `revise_requested`, a
//!   `conflict_precheck` sent to the session), until `session_exited`:
//!   `background` while its latest idle marker (`session_idle_observed`, or
//!   the `stall_nudged` that read it) shows background work,
//!   `after_receipt` once the phase's receipt is observed (or the revise or
//!   conflict is answered), `idle` otherwise (`confirmed` when an idle
//!   marker or a stall nudge was recorded in the phase; without one the
//!   events cannot tell idle from working).
//! - `waiting_integration`: the session has exited after an accepted
//!   validation, and the run waits for its landing.
//! - `after_receipt`: a receipt was observed, and the run waits for its
//!   validation or review.
//! - `unknown`.
//!
//! Next to the gaps, `commands` lists the heavy commands (e2e, llvm-cov,
//! test, build/clippy, chains) the run's sessions ran, from the `work` their
//! `session_closed` recorded (task 514).
use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use super::{AskKind, EventId, RunEvent, stats::timestamp_millis};

/// The default shortest gap `timeline` reports, in seconds.
pub const DEFAULT_GAP_SECS: i64 = 300;

/// One gap between two events of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    /// The event the gap starts after.
    pub after_event: EventId,
    /// The event that ends it; `None` for the gap up to now of a run that is
    /// not finished.
    pub before_event: Option<EventId>,
    pub from: String,
    /// `None` with `before_event`.
    pub until: Option<String>,
    pub secs: i64,
    pub reason: &'static str,
    /// The session the supervisor watched through the gap: `session`,
    /// `resume`, `revise` or `conflict`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    /// For `idle`: whether an idle marker or a stall nudge confirmed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    /// For `waiting_ask`: the open asks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ask_ids: Vec<i64>,
}

/// What the events up to some point say about the run.
#[derive(Debug, Default)]
struct State {
    phase: Option<&'static str>,
    /// The phase a revise or conflict request replaced, for a request
    /// withdrawn because it could not be sent.
    replaced: Option<&'static str>,
    /// The phase's receipt (or answer) was observed.
    answered: bool,
    /// The latest idle marker of the phase: `Some(background_running)`.
    idle: Option<bool>,
    nudged: bool,
    receipt: bool,
    accepted: bool,
    integrating: bool,
    open_asks: BTreeSet<i64>,
}

impl State {
    fn request(&mut self, phase: &'static str) {
        self.replaced = self.phase;
        self.start(phase);
    }

    fn withdraw(&mut self) {
        self.phase = self.replaced.take();
        self.answered = true;
    }

    fn start(&mut self, phase: &'static str) {
        self.phase = Some(phase);
        self.answered = false;
        self.idle = None;
        self.nudged = false;
    }

    fn apply(&mut self, event: &RunEvent) {
        let payload = &event.payload;
        match event.kind.as_str() {
            "agent_started" => self.start("session"),
            "resume_started" => {
                // The resumed session makes a new receipt for a new validation.
                self.start("resume");
                self.receipt = false;
                self.accepted = false;
            }
            "revise_requested" => self.request("revise"),
            "conflict_precheck" if payload["requested"] == true => self.request("conflict"),
            "receipt_observed" => {
                self.receipt = true;
                self.answered = true;
            }
            "revise_finished" | "conflict_resolved" => self.answered = true,
            // A request that could not be sent leaves the session in the
            // phase it replaced, after the receipt the review followed.
            "revise_unsent" => self.withdraw(),
            "conflict_precheck" if payload["unsent"] == true => self.withdraw(),
            "session_idle_observed" => {
                self.idle = Some(payload["background_running"] == true);
            }
            "stall_nudged" => {
                self.nudged = true;
                self.idle = Some(payload["background_running"] == true);
            }
            "session_exited" => self.phase = None,
            "validation_finished" => self.accepted = payload["accepted"] == true,
            "integration_started" => self.integrating = true,
            "ask_opened" if holds_the_run(payload) => {
                if let Some(id) = ask_id(payload) {
                    self.open_asks.insert(id);
                }
            }
            "ask_answered" => {
                if let Some(id) = ask_id(payload) {
                    self.open_asks.remove(&id);
                }
            }
            _ => {}
        }
        if matches!(
            event.kind.as_str(),
            "run_integrated"
                | "integration_deferred"
                | "integration_error"
                | "integration_failed"
                | "runtime_error"
                | "run_adopted"
                | "run_recovered"
                | "resume_started"
        ) {
            self.integrating = false;
        }
    }

    /// The reason of a gap that starts in this state and ends at `end`.
    fn reason(&self, end: Option<&RunEvent>, gap: &mut Gap) {
        if end.is_some_and(|e| matches!(e.kind.as_str(), "run_adopted" | "run_recovered")) {
            gap.reason = "no_supervisor";
        } else if !self.open_asks.is_empty() {
            gap.reason = "waiting_ask";
            gap.ask_ids = self.open_asks.iter().copied().collect();
        } else if self.integrating {
            gap.reason = "integrating";
        } else if let Some(phase) = self.phase {
            gap.phase = Some(phase);
            gap.reason = if self.idle == Some(true) {
                "background"
            } else if self.answered {
                "after_receipt"
            } else {
                gap.confirmed = Some(self.idle.is_some() || self.nudged);
                "idle"
            };
        } else if self.accepted {
            gap.reason = "waiting_integration";
        } else if self.receipt {
            gap.reason = "after_receipt";
        }
    }
}

fn ask_id(payload: &Value) -> Option<i64> {
    payload
        .get("ask_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_i64)
}

/// Whether an ask holds the run it is opened on: the observer's `blocked`
/// and the planner's question do not.
fn holds_the_run(payload: &Value) -> bool {
    !matches!(
        payload["kind"].as_str(),
        Some(kind) if kind == AskKind::Blocked.as_str() || kind == AskKind::PlannerQuestion.as_str()
    )
}

/// The gaps of at least `min_secs` between consecutive `events` of one run
/// (ascending id), with their reasons. With `now_ms`, the run is not
/// finished and the time from its last event to now is a gap too.
pub fn gaps(events: &[RunEvent], min_secs: i64, now_ms: Option<i64>) -> Vec<Gap> {
    let mut state = State::default();
    let mut found = Vec::new();
    for (i, event) in events.iter().enumerate() {
        state.apply(event);
        let next = events.get(i + 1);
        let Some(start) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let end = match next {
            Some(next) => timestamp_millis(&next.created_at),
            None => now_ms,
        };
        let Some(end) = end else { continue };
        let secs = (end - start) / 1000;
        if secs < min_secs.max(1) {
            continue;
        }
        let mut gap = Gap {
            after_event: event.id,
            before_event: next.map(|e| e.id),
            from: event.created_at.clone(),
            until: next.map(|e| e.created_at.clone()),
            secs,
            reason: "unknown",
            phase: None,
            confirmed: None,
            ask_ids: Vec::new(),
        };
        state.reason(next, &mut gap);
        found.push(gap);
    }
    found
}

/// A heavy command a run's session ran, as `timeline` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeavyCommand {
    /// The span's kind: `worker`, `resume` or `revise`.
    pub session: String,
    /// The `session_closed` that recorded it.
    pub event: EventId,
    pub category: String,
    pub from: String,
    pub until: String,
    pub secs: i64,
    pub background: bool,
    /// Whether its end was seen; otherwise it is cut at the session's end.
    pub finished: bool,
    /// `None` when its outcome is unknown.
    pub failed: Option<bool>,
}

/// The heavy commands the run's sessions ran, by start.
pub fn heavy_commands(events: &[RunEvent]) -> Vec<HeavyCommand> {
    let mut commands: Vec<HeavyCommand> = events
        .iter()
        .filter(|event| event.kind == super::sessions::SESSION_CLOSED)
        .flat_map(|event| {
            let heavy = event.payload["work"]["heavy"].as_array().cloned();
            heavy.into_iter().flatten().filter_map(move |row| {
                let text = |key: &str| row[key].as_str().map(str::to_owned);
                Some(HeavyCommand {
                    session: event.payload["kind"].as_str()?.to_owned(),
                    event: event.id,
                    category: text("category")?,
                    from: text("start")?,
                    until: text("end")?,
                    secs: row["secs"].as_i64()?,
                    background: row["background"].as_bool().unwrap_or(false),
                    finished: row["finished"].as_bool().unwrap_or(true),
                    failed: row["failed"].as_bool(),
                })
            })
        })
        .collect();
    commands.sort_by(|a, b| a.from.cmp(&b.from));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, TaskId};
    use serde_json::json;

    fn events(list: &[(&str, Value, &str)]) -> Vec<RunEvent> {
        list.iter()
            .enumerate()
            .map(|(i, (kind, payload, at))| RunEvent {
                id: EventId::new(i as i64 + 1),
                task_id: Some(TaskId::new(1)),
                goal_id: None,
                run_id: Some(RunId::new("r").unwrap()),
                kind: (*kind).to_owned(),
                payload: payload.clone(),
                created_at: format!("2026-09-24T{at}Z"),
            })
            .collect()
    }

    fn reasons(gaps: &[Gap]) -> Vec<(i64, &'static str)> {
        gaps.iter()
            .map(|gap| (gap.after_event.as_i64(), gap.reason))
            .collect()
    }

    #[test]
    fn a_long_gap_before_the_receipt_is_an_unconfirmed_idle() {
        // Task 182: claimed, then nothing until the receipt ten hours later,
        // and no supervisor until another adopted the run.
        let run = events(&[
            ("run_claimed", json!({}), "12:32:30.123"),
            ("agent_started", json!({}), "12:32:32.219"),
            ("first_commit_observed", json!({}), "23:07:26.133"),
            ("receipt_observed", json!({}), "23:07:38.104"),
            (
                "session_idle_observed",
                json!({"background_running": false}),
                "23:07:44.537",
            ),
            (
                "validation_finished",
                json!({"accepted": true}),
                "23:07:45.619",
            ),
            (
                "conflict_precheck",
                json!({"requested": true}),
                "23:07:59.055",
            ),
            ("run_adopted", json!({}), "23:33:33.436"),
            (
                "ask_opened",
                json!({"ask_id": 56, "kind": "blocked"}),
                "23:36:31.843",
            ),
            ("conflict_resolved", json!({}), "23:46:12.315"),
            ("session_exited", json!({}), "23:46:40.274"),
            ("integration_started", json!({}), "23:46:43.044"),
            ("run_integrated", json!({}), "23:48:30.556"),
        ]);
        let found = gaps(&run, DEFAULT_GAP_SECS, None);
        assert_eq!(
            reasons(&found),
            [(2, "idle"), (7, "no_supervisor"), (9, "idle")]
        );
        assert_eq!(found[0].secs, 38093);
        assert_eq!(found[0].phase, Some("session"));
        assert_eq!(found[0].confirmed, Some(false));
        assert_eq!(found[0].before_event, Some(EventId::new(3)));
        assert_eq!(found[2].phase, Some("conflict"));
        assert!(gaps(&run, 60 * 60 * 11, None).is_empty());
    }

    /// A request withdrawn because it could not be sent leaves the gap
    /// after it in the session it replaced.
    #[test]
    fn a_withdrawn_request_leaves_the_session_it_replaced() {
        for (kind, payload) in [
            ("revise_requested", json!({})),
            ("conflict_precheck", json!({"requested": true})),
        ] {
            let withdrawal = if kind == "revise_requested" {
                ("revise_unsent", json!({}))
            } else {
                (
                    "conflict_precheck",
                    json!({"requested": false, "unsent": true}),
                )
            };
            let run = events(&[
                ("agent_started", json!({}), "00:00:00"),
                ("receipt_observed", json!({}), "00:00:01"),
                (kind, payload, "00:00:02"),
                (withdrawal.0, withdrawal.1, "00:00:03"),
                ("session_exited", json!({}), "02:00:00"),
            ]);
            let found = gaps(&run, DEFAULT_GAP_SECS, None);
            assert_eq!(reasons(&found), [(4, "after_receipt")], "{kind}");
            assert_eq!(found[0].phase, Some("session"), "{kind}");
        }
    }

    #[test]
    fn each_state_has_its_reason() {
        let run = events(&[
            ("agent_started", json!({}), "00:00:00"),
            (
                "ask_opened",
                json!({"ask_id": 3, "kind": "worker_question"}),
                "00:00:01",
            ),
            ("ask_answered", json!({"ask_id": 3}), "01:00:00"),
            (
                "stall_nudged",
                json!({"background_running": true}),
                "01:00:01",
            ),
            (
                "stall_nudged",
                json!({"background_running": false}),
                "02:00:00",
            ),
            (
                "session_idle_observed",
                json!({"background_running": false}),
                "02:00:01",
            ),
            ("receipt_observed", json!({}), "03:00:00"),
            ("validation_finished", json!({"accepted": true}), "04:00:00"),
            ("revise_requested", json!({}), "04:00:01"),
            ("revise_finished", json!({}), "05:00:00"),
            ("session_exited", json!({}), "06:00:00"),
            ("integration_started", json!({}), "07:00:00"),
            ("integration_deferred", json!({}), "08:00:00"),
            ("resume_started", json!({}), "08:00:01"),
        ]);
        let found = gaps(
            &run,
            60,
            Some(timestamp_millis("2026-09-24T09:00:01Z").unwrap()),
        );
        assert_eq!(
            reasons(&found),
            [
                (2, "waiting_ask"),
                (4, "background"),
                (6, "idle"),
                (7, "after_receipt"),
                (9, "idle"),
                (10, "after_receipt"),
                (11, "waiting_integration"),
                (12, "integrating"),
                (14, "idle"),
            ]
        );
        assert_eq!(found[0].ask_ids, [3]);
        assert_eq!(found[2].confirmed, Some(true));
        assert_eq!(found[3].phase, Some("session"));
        assert_eq!(found[4].phase, Some("revise"));
        assert_eq!(found[5].phase, Some("revise"));
        let last = found.last().unwrap();
        assert_eq!(
            (last.phase, last.before_event, last.until.as_deref()),
            (Some("resume"), None, None)
        );
        assert_eq!(last.secs, 3600);
    }

    #[test]
    fn a_takeover_ends_the_landing_and_background_outranks_the_receipt() {
        let run = events(&[
            ("agent_started", json!({}), "00:00:00"),
            ("receipt_observed", json!({}), "00:00:01"),
            (
                "session_idle_observed",
                json!({"background_running": true}),
                "00:00:02",
            ),
            ("session_exited", json!({}), "01:00:00"),
            ("validation_finished", json!({"accepted": true}), "01:00:01"),
            ("integration_started", json!({}), "01:00:02"),
            ("run_recovered", json!({}), "02:00:00"),
            ("lease_acquired", json!({}), "02:00:01"),
        ]);
        let found = gaps(
            &run,
            60,
            Some(timestamp_millis("2026-09-24T03:00:01Z").unwrap()),
        );
        assert_eq!(
            reasons(&found),
            [
                (3, "background"),
                (6, "no_supervisor"),
                (8, "waiting_integration")
            ]
        );
    }

    #[test]
    fn a_gap_with_no_known_state_is_unknown() {
        let run = events(&[
            ("run_claimed", json!({}), "00:00:00"),
            ("workspace_created", json!({}), "01:00:00"),
            ("receipt_observed", json!({}), "not a time"),
        ]);
        assert_eq!(reasons(&gaps(&run, 60, None)), [(1, "unknown")]);
    }

    #[test]
    fn heavy_commands_come_from_the_closed_sessions_work() {
        let heavy = |category: &str, start: &str| {
            json!({"category": category, "start": start, "end": "2026-09-24T01:10:00.000Z",
                   "secs": 60, "background": true, "finished": true, "failed": false})
        };
        let run = events(&[
            ("session_opened", json!({"kind": "worker"}), "01:00:00"),
            (
                "session_closed",
                json!({"kind": "worker", "work": {"heavy": [
                    heavy("test", "2026-09-24T01:05:00.000Z"),
                    {"category": "e2e"},
                ]}}),
                "01:20:00",
            ),
            (
                "session_closed",
                json!({"kind": "resume", "work": {"heavy": [heavy("llvm_cov", "2026-09-24T01:01:00.000Z")]}}),
                "01:30:00",
            ),
            ("session_closed", json!({"kind": "review"}), "01:40:00"),
        ]);
        let commands = heavy_commands(&run);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].category, "llvm_cov");
        assert_eq!(commands[0].session, "resume");
        assert_eq!(commands[1].event, EventId::new(2));
        assert_eq!(commands[1].failed, Some(false));
        assert!(commands[1].background);
    }
}
