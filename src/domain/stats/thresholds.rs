//! `stall_thresholds` of `stats` (ADR-0043 decisions 3 and 6): per
//! threshold of the stalled-session checks, how many detections the
//! supervisor made, how each ended, how long it took to detect and to end,
//! and how often a person stepped in before any detection (a suspected
//! miss). Everything is derived again from `run_events`.
//!
//! - The receipt-less idle (`idle_without_receipt_secs`): each
//!   `stall_nudged` (detection `nudge`) and each `stalled` ask's
//!   `ask_opened` (detection `ask`), ended by the `stall_resolved` the
//!   supervisor records for it.
//! - The check that a sent text was taken (`send_confirm_secs`, task 285's
//!   events): each `submit_retried` of a text (`enter_retry`), each
//!   `submit_resent` (`resend`), and each `answer_prompt` ask opened right
//!   after a `submit_unconfirmed` of a text or a `submit_not_started`
//!   (`ask`), ended by the events after it.
//! - `background_alert_secs` has no detection of the supervisor: only the
//!   running alerts it judges now are counted.
//!
//! A detection with no end recorded yet is `pending`.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{RunningAlert, median, timestamp_millis, watched_phase};
use crate::domain::{EventId, RunEvent, RunId, RunStatus, TaskId, stall::StallConfig};

/// The setting of the receipt-less idle detections.
pub const IDLE: &str = "idle_without_receipt_secs";
/// The setting of the checks that a sent text was taken.
pub const SEND: &str = "send_confirm_secs";
/// The setting of the `long_background` alert.
pub const BACKGROUND: &str = "background_alert_secs";

/// The outcome of a detection whose end is not recorded yet.
pub const PENDING: &str = "pending";

/// One detection and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The event that made it; `--since` compares against it.
    pub event_id: EventId,
    pub task_id: Option<TaskId>,
    pub run_id: RunId,
    /// The setting it is judged by.
    pub threshold: &'static str,
    /// `nudge`, `ask`, `enter_retry` or `resend`.
    pub detection: &'static str,
    /// The value of the setting it was made with, when recorded.
    pub threshold_secs: Option<i64>,
    /// From the state it detected (the idle marker, the send) to it.
    pub detected_after_secs: Option<i64>,
    pub outcome: String,
    /// From it to its end.
    pub resolved_after_secs: Option<i64>,
}

/// A person who stepped in on a stalled-looking session no detection had
/// reached yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preemption {
    pub event_id: EventId,
    pub task_id: Option<TaskId>,
    pub threshold: &'static str,
    /// `recover` (a person recovered a run no detection had reached) or
    /// `input` (a `stall_preempted` the supervisor recorded).
    pub via: &'static str,
    pub threshold_secs: Option<i64>,
}

/// Count, median and maximum of some seconds; empty when none is recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Durations {
    pub count: usize,
    pub median: Option<i64>,
    pub max: Option<i64>,
}

impl Durations {
    fn of(values: impl Iterator<Item = Option<i64>>) -> Self {
        let mut values: Vec<i64> = values.flatten().collect();
        Self {
            count: values.len(),
            max: values.iter().copied().max(),
            median: median(&mut values),
        }
    }
}

/// A number of detections and their outcomes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Outcomes {
    pub count: i64,
    pub outcomes: BTreeMap<String, i64>,
}

impl Outcomes {
    fn add(&mut self, outcome: &str) {
        self.count += 1;
        *self.outcomes.entry(outcome.to_owned()).or_default() += 1;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Preempted {
    pub count: i64,
    /// Per `via`.
    pub by_via: BTreeMap<&'static str, i64>,
}

/// What one threshold did in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ThresholdStats {
    /// The value in force now (`stall_config`).
    pub threshold_secs: i64,
    /// Detections made in the window.
    pub detections: i64,
    /// Per detection, with their outcomes.
    pub by_detection: BTreeMap<&'static str, Outcomes>,
    /// The outcomes of every detection.
    pub outcomes: BTreeMap<String, i64>,
    pub detected_after_secs: Durations,
    pub resolved_after_secs: Durations,
    /// A person stepped in before any detection (a suspected miss).
    pub preempted: Preempted,
    /// Per value of the setting the detections were made with
    /// (`unrecorded` when the event does not carry it), so that the
    /// outcomes before and after a change can be told apart.
    pub by_threshold_secs: BTreeMap<String, Outcomes>,
    /// The running alerts judged by it now (`idle_without_receipt`,
    /// `long_background`).
    pub running_alerts: i64,
}

fn at(event: &RunEvent) -> Option<i64> {
    timestamp_millis(&event.created_at)
}

fn secs(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    Some((to? - from?) / 1000)
}

fn int(event: &RunEvent, key: &str) -> Option<i64> {
    event.payload.get(key).and_then(Value::as_i64)
}

fn text<'a>(event: &'a RunEvent, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(Value::as_str)
}

/// The setting a recorded name stands for; the idle one when missing.
fn threshold_named(name: Option<&str>) -> &'static str {
    match name {
        Some(SEND) => SEND,
        Some(BACKGROUND) => BACKGROUND,
        _ => IDLE,
    }
}

/// Whether `event` ended the run's session or the run.
fn ends_session(event: &RunEvent) -> bool {
    let ended = match event.kind.as_str() {
        // A session handed to validation alive has not ended (ADR-0027).
        "supervision_finished" => event.payload.get("session_live") != Some(&Value::Bool(true)),
        "workspace_closed" | "run_recovered" | "run_integrated" => true,
        _ => false,
    };
    ended || matches!(text(event, "status"), Some("failed" | "interrupted"))
}

/// Each run's events, in order of each run's first event.
fn by_run(events: &[RunEvent]) -> Vec<Vec<&RunEvent>> {
    let mut order: Vec<&RunId> = Vec::new();
    let mut runs: BTreeMap<&str, Vec<&RunEvent>> = BTreeMap::new();
    for event in events {
        let Some(run_id) = &event.run_id else {
            continue;
        };
        runs.entry(run_id.as_str())
            .or_insert_with(|| {
                order.push(run_id);
                Vec::new()
            })
            .push(event);
    }
    order
        .into_iter()
        .filter_map(|run_id| runs.remove(run_id.as_str()))
        .collect()
}

/// The detections in `events` (ascending id) and how each ended by `now_ms`.
pub fn detections(events: &[RunEvent], now_ms: i64) -> Vec<Detection> {
    let mut found = Vec::new();
    for run in by_run(events) {
        for (i, event) in run.iter().enumerate() {
            let later = &run[i + 1..];
            let detection = |threshold, detection| Detection {
                event_id: event.id,
                task_id: event.task_id,
                run_id: event.run_id.clone().expect("grouped by run"),
                threshold,
                detection,
                threshold_secs: None,
                detected_after_secs: None,
                outcome: PENDING.to_owned(),
                resolved_after_secs: None,
            };
            match event.kind.as_str() {
                "stall_nudged" => {
                    let mut nudge = detection(IDLE, "nudge");
                    nudge.threshold_secs = int(event, "threshold_secs");
                    nudge.detected_after_secs = int(event, "idle_secs");
                    if let Some(end) = later.iter().find(|e| {
                        e.kind == "stall_resolved"
                            && text(e, "detection") == Some("nudge")
                            && e.payload.get("phase") == event.payload.get("phase")
                    }) {
                        resolved_by(&mut nudge, end);
                    }
                    found.push(nudge);
                }
                "ask_opened" if text(event, "kind") == Some("stalled") => {
                    let id = event.payload.get("ask_id");
                    let end = later.iter().find(|e| {
                        e.kind == "stall_resolved"
                            && text(e, "detection") == Some("ask")
                            && e.payload.get("ask_id") == id
                    });
                    let mut ask = detection(
                        threshold_named(end.and_then(|e| text(e, "threshold"))),
                        "ask",
                    );
                    if let Some(end) = end {
                        resolved_by(&mut ask, end);
                    }
                    found.push(ask);
                }
                "submit_retried" if text(event, "input") != Some("exit") => {
                    let mut retry = detection(SEND, "enter_retry");
                    retry.outcome = if event.payload.get("submitted") == Some(&Value::Bool(true)) {
                        "resolved_by_enter"
                    } else {
                        "escalated"
                    }
                    .to_owned();
                    found.push(retry);
                }
                "submit_resent" => {
                    let waited = int(event, "waited_secs");
                    let mut resend = detection(SEND, "resend");
                    resend.threshold_secs = waited;
                    resend.detected_after_secs = waited;
                    let sent = at(event);
                    // Its check ends within twice its wait (the check comes
                    // `waited_secs` after it), and before the next resend.
                    let bound = later
                        .iter()
                        .take_while(|e| {
                            e.kind != "submit_resent"
                                && at(e).zip(sent).zip(waited).is_some_and(
                                    |((at, sent), waited)| at - sent <= 2 * waited * 1000,
                                )
                        })
                        .collect::<Vec<_>>();
                    let same_what =
                        |e: &RunEvent| e.payload.get("what") == event.payload.get("what");
                    // Not taken: no sign of work again, or stuck in the
                    // input box (whose check then ends without one).
                    if let Some(end) = bound.iter().find(|e| {
                        same_what(e)
                            && ((e.kind == "submit_not_started"
                                && e.payload.get("resent") == Some(&Value::Bool(true)))
                                || (e.kind == "submit_unconfirmed"
                                    && text(e, "input") != Some("exit")))
                    }) {
                        resend.outcome = "escalated".to_owned();
                        resend.resolved_after_secs = secs(sent, at(end));
                    } else if let Some(end) = bound.iter().find(|e| {
                        ends_session(e)
                            && at(e)
                                .zip(sent)
                                .zip(waited)
                                .is_some_and(|((at, sent), waited)| at - sent < waited * 1000)
                    }) {
                        // The session ended before the resend could be checked.
                        resend.outcome = "run_ended".to_owned();
                        resend.resolved_after_secs = secs(sent, at(end));
                    } else if sent.zip(waited).is_some_and(|(sent, waited)| {
                        // Twice its wait old with neither, it was taken.
                        now_ms - sent >= 2 * waited * 1000
                    }) {
                        resend.outcome = "resolved_by_resend".to_owned();
                    }
                    found.push(resend);
                }
                "submit_unconfirmed" | "submit_not_started"
                    if text(event, "input") != Some("exit") =>
                {
                    // The `answer_prompt` ask the supervisor opens right
                    // after; none when one was open already.
                    let Some(opened) = later.first().filter(|e| {
                        e.kind == "ask_opened" && text(e, "kind") == Some("answer_prompt")
                    }) else {
                        continue;
                    };
                    let mut ask = Detection {
                        event_id: opened.id,
                        ..detection(SEND, "ask")
                    };
                    if event.kind == "submit_not_started" {
                        ask.threshold_secs = int(event, "waited_secs");
                        ask.detected_after_secs = int(event, "waited_secs");
                    }
                    let id = opened.payload.get("ask_id");
                    let after: Vec<&&RunEvent> =
                        later.iter().filter(|e| e.id > opened.id).collect();
                    let answered = after
                        .iter()
                        .position(|e| e.kind == "ask_answered" && e.payload.get("ask_id") == id);
                    let ended_before = |upto: usize| after[..upto].iter().any(|e| ends_session(e));
                    match answered {
                        Some(j) => {
                            let answer = after[j];
                            ask.outcome = if answer.payload.get("runtime_closed")
                                != Some(&Value::Bool(true))
                            {
                                "answered_intervene"
                            } else if ended_before(j) {
                                "run_ended"
                            } else {
                                "resolved_by_itself"
                            }
                            .to_owned();
                            ask.resolved_after_secs = secs(at(opened), at(answer));
                        }
                        None => {
                            if let Some(end) = after.iter().find(|e| ends_session(e)) {
                                ask.outcome = "run_ended".to_owned();
                                ask.resolved_after_secs = secs(at(opened), at(end));
                            }
                        }
                    }
                    found.push(ask);
                }
                _ => {}
            }
        }
    }
    found.sort_by_key(|detection| detection.event_id);
    found
}

/// Take the end the supervisor recorded in `stall_resolved`.
fn resolved_by(detection: &mut Detection, end: &RunEvent) {
    if let Some(outcome) = text(end, "outcome") {
        detection.outcome = outcome.to_owned();
    }
    detection.threshold_secs = int(end, "threshold_secs").or(detection.threshold_secs);
    detection.detected_after_secs =
        int(end, "detected_after_secs").or(detection.detected_after_secs);
    detection.resolved_after_secs = int(end, "resolved_after_secs");
}

/// The people who stepped in before any detection: a `stall_preempted`
/// the supervisor recorded, and a `recover` by a person (not the
/// supervisor's own) of a run whose watched session had no receipt, no
/// idle detection, no question or dialog waiting for a person, when it
/// was recovered.
pub fn preemptions(events: &[RunEvent]) -> Vec<Preemption> {
    let mut found = Vec::new();
    for run in by_run(events) {
        for (i, event) in run.iter().enumerate() {
            match event.kind.as_str() {
                "stall_preempted" => found.push(Preemption {
                    event_id: event.id,
                    task_id: event.task_id,
                    threshold: threshold_named(text(event, "threshold")),
                    via: "input",
                    threshold_secs: int(event, "threshold_secs"),
                }),
                "run_recovered" if text(event, "by") != Some("supervisor") => {
                    let Some(previous) = text(event, "previous_status")
                        .and_then(|status| status.parse::<RunStatus>().ok())
                    else {
                        continue;
                    };
                    let before = &run[..i];
                    let Some((_, start)) = watched_phase(previous, before) else {
                        continue;
                    };
                    let phase: Vec<&&RunEvent> = before
                        .iter()
                        .filter(|e| at(e).is_some_and(|at| at >= start))
                        .collect();
                    let detected_or_done = phase.iter().any(|e| {
                        matches!(e.kind.as_str(), "receipt_observed" | "stall_nudged")
                            || (e.kind == "ask_opened" && text(e, "kind") == Some("stalled"))
                    });
                    if detected_or_done || waits_for_a_person(before, start) {
                        continue;
                    }
                    found.push(Preemption {
                        event_id: event.id,
                        task_id: event.task_id,
                        threshold: IDLE,
                        via: "recover",
                        threshold_secs: None,
                    });
                }
                _ => {}
            }
        }
    }
    found.sort_by_key(|preemption| preemption.event_id);
    found
}

/// Whether, after `events`, the run had a `worker_question` or an
/// `answer_prompt` ask open, or a dialog recorded since `start` not
/// cleared: a session that waits for a person is not stalled.
fn waits_for_a_person(events: &[&RunEvent], start: i64) -> bool {
    let mut open: Vec<Option<&Value>> = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "ask_opened"
                if matches!(
                    text(event, "kind"),
                    Some("worker_question" | "answer_prompt")
                ) =>
            {
                open.push(event.payload.get("ask_id"));
            }
            "ask_answered" => open.retain(|id| *id != event.payload.get("ask_id")),
            _ => {}
        }
    }
    let dialog = events
        .iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "prompt_waiting" | "prompt_cleared"))
        .is_some_and(|e| e.kind == "prompt_waiting" && at(e).is_some_and(|at| at >= start));
    !open.is_empty() || dialog
}

/// Aggregate the detections and preemptions `counts` accepts (the
/// window and the goal) per threshold; `running` are the running alerts
/// judged now and `config` the thresholds in force. Every setting has an
/// entry.
pub fn thresholds(
    detections: &[Detection],
    preemptions: &[Preemption],
    counts: impl Fn(EventId, Option<TaskId>) -> bool,
    running: &[RunningAlert],
    config: &StallConfig,
) -> BTreeMap<&'static str, ThresholdStats> {
    let mut stats: BTreeMap<&'static str, ThresholdStats> = [
        (IDLE, config.idle_without_receipt_secs),
        (SEND, config.send_confirm_secs),
        (BACKGROUND, config.background_alert_secs),
    ]
    .into_iter()
    .map(|(name, secs)| {
        (
            name,
            ThresholdStats {
                threshold_secs: secs,
                ..Default::default()
            },
        )
    })
    .collect();
    let counted: Vec<&Detection> = detections
        .iter()
        .filter(|d| counts(d.event_id, d.task_id))
        .collect();
    for (name, entry) in &mut stats {
        let mine: Vec<&&Detection> = counted.iter().filter(|d| d.threshold == *name).collect();
        for detection in &mine {
            entry.detections += 1;
            entry
                .by_detection
                .entry(detection.detection)
                .or_default()
                .add(&detection.outcome);
            *entry.outcomes.entry(detection.outcome.clone()).or_default() += 1;
            entry
                .by_threshold_secs
                .entry(
                    detection
                        .threshold_secs
                        .map_or_else(|| "unrecorded".to_owned(), |secs| secs.to_string()),
                )
                .or_default()
                .add(&detection.outcome);
        }
        entry.detected_after_secs = Durations::of(mine.iter().map(|d| d.detected_after_secs));
        entry.resolved_after_secs = Durations::of(mine.iter().map(|d| d.resolved_after_secs));
        for preemption in preemptions
            .iter()
            .filter(|p| p.threshold == *name && counts(p.event_id, p.task_id))
        {
            entry.preempted.count += 1;
            *entry.preempted.by_via.entry(preemption.via).or_default() += 1;
        }
        let alert = match *name {
            IDLE => "idle_without_receipt",
            BACKGROUND => "long_background",
            _ => "",
        };
        entry.running_alerts =
            i64::try_from(running.iter().filter(|a| a.kind == alert).count()).unwrap_or(i64::MAX);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T: i64 = 1_800_000_000;
    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";

    fn event(id: i64, run: &str, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: crate::application::timestamp(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
            ),
        }
    }

    /// Number the events in order.
    fn numbered(events: Vec<(&str, &str, Value, i64)>) -> Vec<RunEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, (run, kind, payload, secs))| event(i as i64 + 1, run, kind, payload, secs))
            .collect()
    }

    fn outcomes(events: &[RunEvent], now: i64) -> Vec<(&'static str, &'static str, String)> {
        detections(events, now * 1000)
            .into_iter()
            .map(|d| (d.threshold, d.detection, d.outcome))
            .collect()
    }

    fn resolved(detection: &str, outcome: &str, extra: Value) -> Value {
        let mut payload = json!({
            "phase": "session",
            "detection": detection,
            "threshold": IDLE,
            "threshold_secs": 1200,
            "detected_after_secs": 1250,
            "outcome": outcome,
            "resolved_after_secs": 300,
        });
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    }

    fn nudged() -> Value {
        json!({"phase": "session", "idle_secs": 1250, "threshold_secs": 1200})
    }

    fn stalled(id: i64) -> Value {
        json!({"ask_id": id, "kind": "stalled", "asked_by": "supervisor"})
    }

    /// Task 182's shape: the nudge ends the stall, or escalates to an ask
    /// answered `wait` (too early?) or `intervene`, or the ask resolves by
    /// itself, or the run ends first.
    #[test]
    fn an_idle_detection_takes_the_end_the_supervisor_recorded() {
        let events = numbered(vec![
            (R1, "stall_nudged", nudged(), T),
            (
                R1,
                "stall_resolved",
                resolved("nudge", "resolved_by_nudge", json!({})),
                T + 300,
            ),
            (R2, "stall_nudged", nudged(), T),
            (
                R2,
                "stall_resolved",
                resolved("nudge", "escalated", json!({})),
                T + 1300,
            ),
            (R2, "ask_opened", stalled(7), T + 1300),
            (
                R2,
                "stall_resolved",
                resolved("ask", "answered_wait", json!({"ask_id": 7})),
                T + 1400,
            ),
            (R2, "ask_opened", stalled(8), T + 2700),
            (
                R2,
                "stall_resolved",
                resolved("ask", "answered_intervene", json!({"ask_id": 8})),
                T + 2800,
            ),
            (R2, "ask_opened", stalled(9), T + 4000),
            (
                R2,
                "stall_resolved",
                resolved("ask", "resolved_by_itself", json!({"ask_id": 9})),
                T + 4100,
            ),
            (R2, "ask_opened", stalled(10), T + 5400),
            (
                R2,
                "stall_resolved",
                resolved("ask", "run_ended", json!({"ask_id": 10})),
                T + 5500,
            ),
        ]);
        let found = detections(&events, (T + 6000) * 1000);
        assert_eq!(
            found
                .iter()
                .map(|d| (d.detection, d.outcome.as_str()))
                .collect::<Vec<_>>(),
            [
                ("nudge", "resolved_by_nudge"),
                ("nudge", "escalated"),
                ("ask", "answered_wait"),
                ("ask", "answered_intervene"),
                ("ask", "resolved_by_itself"),
                ("ask", "run_ended"),
            ]
        );
        assert!(found.iter().all(|d| d.threshold == IDLE));
        assert_eq!(
            (
                found[0].threshold_secs,
                found[0].detected_after_secs,
                found[0].resolved_after_secs
            ),
            (Some(1200), Some(1250), Some(300))
        );
        // The ask is the event `--since` compares against.
        assert_eq!(found[2].event_id, EventId::new(5));
    }

    #[test]
    fn an_idle_detection_with_no_end_yet_is_pending() {
        let events = numbered(vec![
            (R1, "stall_nudged", nudged(), T),
            (R2, "stall_nudged", nudged(), T),
            (
                R2,
                "stall_resolved",
                resolved("nudge", "escalated", json!({})),
                T + 1300,
            ),
            (R2, "ask_opened", stalled(7), T + 1300),
            // Another ask's end does not end this one.
            (
                R2,
                "stall_resolved",
                resolved("ask", "answered_wait", json!({"ask_id": 6})),
                T + 1400,
            ),
        ]);
        let found = detections(&events, (T + 2000) * 1000);
        assert_eq!(
            found.iter().map(|d| d.outcome.as_str()).collect::<Vec<_>>(),
            [PENDING, "escalated", PENDING]
        );
        // What the nudge recorded is kept.
        assert_eq!(
            (found[0].threshold_secs, found[0].detected_after_secs),
            (Some(1200), Some(1250))
        );
        assert_eq!(found[2].detected_after_secs, None);
    }

    /// Task 205's shape: a text the Enter did or did not submit.
    #[test]
    fn an_enter_sent_again_resolves_or_escalates_and_exit_is_not_counted() {
        let retried = |input: &str, submitted: bool| json!({"input": input, "what": "revise", "retries": 1, "submitted": submitted});
        let events = numbered(vec![
            (R1, "submit_retried", retried("text", true), T),
            (R1, "submit_retried", retried("text", false), T + 10),
            (
                R1,
                "submit_unconfirmed",
                json!({"input": "text", "what": "revise"}),
                T + 10,
            ),
            (
                R1,
                "ask_opened",
                json!({"ask_id": 3, "kind": "answer_prompt"}),
                T + 10,
            ),
            (
                R1,
                "ask_answered",
                json!({"ask_id": 3, "kind": "answer_prompt"}),
                T + 70,
            ),
            (R2, "submit_retried", retried("exit", false), T),
            (
                R2,
                "submit_unconfirmed",
                json!({"input": "exit", "what": "exit"}),
                T,
            ),
            (
                R2,
                "ask_opened",
                json!({"ask_id": 4, "kind": "answer_prompt"}),
                T,
            ),
        ]);
        let found = detections(&events, (T + 100) * 1000);
        assert_eq!(
            outcomes(&events, T + 100),
            [
                (SEND, "enter_retry", "resolved_by_enter".to_owned()),
                (SEND, "enter_retry", "escalated".to_owned()),
                (SEND, "ask", "answered_intervene".to_owned()),
            ]
        );
        assert_eq!(found[2].event_id, EventId::new(4));
        assert_eq!(found[2].resolved_after_secs, Some(60));
        assert_eq!(found[2].threshold_secs, None);
    }

    #[test]
    fn a_resent_request_resolves_escalates_waits_or_ends_with_the_run() {
        let resent = json!({"what": "resume request", "waited_secs": 60});
        let not_started = json!({"what": "resume request", "waited_secs": 60, "resent": true});
        let events = numbered(vec![
            (R1, "submit_resent", resent.clone(), T),
            (R1, "submit_not_started", not_started, T + 62),
            (
                R1,
                "ask_opened",
                json!({"ask_id": 5, "kind": "answer_prompt"}),
                T + 62,
            ),
            (R2, "submit_resent", resent.clone(), T),
        ]);
        let found = detections(&events, (T + 100) * 1000);
        assert_eq!(
            found
                .iter()
                .map(|d| (d.detection, d.outcome.as_str()))
                .collect::<Vec<_>>(),
            [
                ("resend", "escalated"),
                ("ask", PENDING),
                ("resend", PENDING)
            ]
        );
        assert_eq!(
            (
                found[0].threshold_secs,
                found[0].detected_after_secs,
                found[0].resolved_after_secs
            ),
            (Some(60), Some(60), Some(62))
        );
        assert_eq!(
            (found[1].threshold_secs, found[1].detected_after_secs),
            (Some(60), Some(60))
        );
        // Twice its wait later with no `submit_not_started`, it was taken.
        assert_eq!(
            detections(&events, (T + 120) * 1000)[2].outcome,
            "resolved_by_resend"
        );
        // A session that ended before the check.
        let mut ended = events.clone();
        ended.push(event(5, R2, "supervision_finished", json!({}), T + 30));
        assert_eq!(detections(&ended, (T + 200) * 1000)[2].outcome, "run_ended");
        // A resend stuck in the input box escalated too; a later send of
        // the same kind that failed does not make this one fail.
        let events = numbered(vec![
            (R1, "submit_resent", resent.clone(), T),
            (
                R1,
                "submit_unconfirmed",
                json!({"input": "text", "what": "resume request"}),
                T + 5,
            ),
            (R2, "submit_resent", resent.clone(), T),
            (R2, "submit_resent", resent.clone(), T + 1000),
            (
                R2,
                "submit_not_started",
                json!({"what": "resume request", "waited_secs": 60, "resent": true}),
                T + 1062,
            ),
        ]);
        assert_eq!(
            outcomes(&events, T + 2000),
            [
                (SEND, "resend", "escalated".to_owned()),
                (SEND, "resend", "resolved_by_resend".to_owned()),
                (SEND, "resend", "escalated".to_owned()),
            ]
        );
        // Without its wait recorded, it cannot be judged taken.
        let events = numbered(vec![(R1, "submit_resent", json!({"what": "answer"}), T)]);
        assert_eq!(
            outcomes(&events, T + 9999),
            [(SEND, "resend", PENDING.to_owned())]
        );
    }

    #[test]
    fn a_send_ask_closed_by_the_runtime_resolved_itself_or_ended_with_the_run() {
        let not_started = json!({"what": "answer", "waited_secs": 60, "resent": false});
        let ask = |id| json!({"ask_id": id, "kind": "answer_prompt"});
        let closed = |id| json!({"ask_id": id, "kind": "answer_prompt", "runtime_closed": true});
        let events = numbered(vec![
            (R1, "submit_not_started", not_started.clone(), T),
            (R1, "ask_opened", ask(1), T),
            (R1, "ask_answered", closed(1), T + 30),
            (R2, "submit_not_started", not_started.clone(), T),
            (R2, "ask_opened", ask(2), T),
            (R2, "supervision_finished", json!({}), T + 40),
            (R2, "ask_answered", closed(2), T + 40),
        ]);
        assert_eq!(
            outcomes(&events, T + 100),
            [
                (SEND, "ask", "resolved_by_itself".to_owned()),
                (SEND, "ask", "run_ended".to_owned()),
            ]
        );
        // Unanswered: ended with the run, or pending while it goes on; a
        // session handed to validation alive has not ended.
        let events = numbered(vec![
            (R1, "submit_not_started", not_started.clone(), T),
            (R1, "ask_opened", ask(1), T),
            (
                R1,
                "run_recovered",
                json!({"status": "interrupted"}),
                T + 50,
            ),
            (R2, "submit_not_started", not_started.clone(), T),
            (R2, "ask_opened", ask(2), T),
            (
                R2,
                "supervision_finished",
                json!({"session_live": true}),
                T + 50,
            ),
        ]);
        assert_eq!(
            outcomes(&events, T + 100),
            [
                (SEND, "ask", "run_ended".to_owned()),
                (SEND, "ask", PENDING.to_owned()),
            ]
        );
        // An ask already open is not opened again: nothing new to count.
        let events = numbered(vec![
            (R1, "submit_not_started", not_started, T),
            (
                R1,
                "submit_resent",
                json!({"what": "answer", "waited_secs": 60}),
                T + 1,
            ),
        ]);
        assert_eq!(
            outcomes(&events, T + 10),
            [(SEND, "resend", PENDING.to_owned())]
        );
    }

    fn recovered(by: Option<&str>) -> Value {
        let mut payload = json!({"previous_status": "running", "status": "interrupted"});
        if let Some(by) = by {
            payload["by"] = json!(by);
        }
        payload
    }

    /// A person recovered a run the supervisor had not detected.
    #[test]
    fn a_person_who_stepped_in_before_any_detection_is_a_suspected_miss() {
        let events = numbered(vec![
            (R1, "run_claimed", json!({}), T),
            (R1, "agent_started", json!({}), T + 10),
            (R1, "run_recovered", recovered(None), T + 900),
            (
                R2,
                "stall_preempted",
                json!({"threshold": IDLE, "threshold_secs": 1200, "idle_secs": 600}),
                T,
            ),
        ]);
        let found = preemptions(&events);
        assert_eq!(
            found
                .iter()
                .map(|p| (p.threshold, p.via, p.threshold_secs))
                .collect::<Vec<_>>(),
            [(IDLE, "recover", None), (IDLE, "input", Some(1200))]
        );
        assert_eq!(found[0].event_id, EventId::new(3));
    }

    #[test]
    fn a_recover_after_a_detection_a_receipt_or_a_question_is_no_miss() {
        let base = || {
            vec![
                (R1, "run_claimed", json!({}), T),
                (R1, "agent_started", json!({}), T + 10),
            ]
        };
        let with = |extra: Vec<(&'static str, &'static str, Value, i64)>, by: Option<&str>| {
            let mut events = base();
            events.extend(extra);
            events.push((R1, "run_recovered", recovered(by), T + 5000));
            preemptions(&numbered(events))
        };
        assert_eq!(with(vec![], None).len(), 1);
        // The supervisor's own recover of an orphan.
        assert!(with(vec![], Some("supervisor")).is_empty());
        for (kind, payload) in [
            ("stall_nudged", nudged()),
            ("ask_opened", stalled(1)),
            ("receipt_observed", json!({})),
            (
                "ask_opened",
                json!({"ask_id": 2, "kind": "worker_question"}),
            ),
            ("ask_opened", json!({"ask_id": 3, "kind": "answer_prompt"})),
            ("prompt_waiting", json!({})),
        ] {
            assert!(
                with(vec![(R1, kind, payload, T + 100)], None).is_empty(),
                "{kind}"
            );
        }
        // An answered question and a cleared dialog wait for nobody.
        assert_eq!(
            with(
                vec![
                    (
                        R1,
                        "ask_opened",
                        json!({"ask_id": 2, "kind": "worker_question"}),
                        T + 100
                    ),
                    (R1, "ask_answered", json!({"ask_id": 2}), T + 200),
                    (R1, "prompt_waiting", json!({}), T + 300),
                    (R1, "prompt_cleared", json!({}), T + 400),
                ],
                None
            )
            .len(),
            1
        );
        // A detection of an earlier session does not count.
        let events = numbered(vec![
            (R1, "run_claimed", json!({}), T),
            (R1, "agent_started", json!({}), T + 10),
            (R1, "stall_nudged", nudged(), T + 20),
            (R1, "agent_started", json!({}), T + 30),
            (R1, "run_recovered", recovered(None), T + 40),
        ]);
        assert_eq!(preemptions(&events).len(), 1);
        // A run no session was watched for, or without its previous status.
        let events = numbered(vec![
            (R1, "run_claimed", json!({}), T),
            (
                R1,
                "run_recovered",
                json!({"previous_status": "integrating"}),
                T + 40,
            ),
            (R2, "run_recovered", json!({}), T + 40),
        ]);
        assert!(preemptions(&events).is_empty());
    }

    #[test]
    fn thresholds_count_detections_outcomes_times_and_misses_per_setting() {
        let detection =
            |id: i64, threshold, kind, secs: Option<i64>, outcome: &str, after| Detection {
                event_id: EventId::new(id),
                task_id: Some(TaskId::new(if id == 9 { 2 } else { 1 })),
                run_id: RunId::new(R1).unwrap(),
                threshold,
                detection: kind,
                threshold_secs: secs,
                detected_after_secs: after,
                outcome: outcome.to_owned(),
                resolved_after_secs: after.map(|a| a / 10),
            };
        let found = [
            detection(
                1,
                IDLE,
                "nudge",
                Some(1200),
                "resolved_by_nudge",
                Some(1210),
            ),
            detection(2, IDLE, "nudge", Some(1200), "escalated", Some(1300)),
            detection(3, IDLE, "ask", Some(600), "answered_wait", Some(700)),
            detection(4, SEND, "enter_retry", None, "resolved_by_enter", None),
            // Outside the window, and of another goal's task.
            detection(0, IDLE, "nudge", Some(1200), "escalated", Some(1)),
            detection(9, IDLE, "nudge", Some(1200), "escalated", Some(1)),
        ];
        let misses = [
            Preemption {
                event_id: EventId::new(5),
                task_id: Some(TaskId::new(1)),
                threshold: IDLE,
                via: "recover",
                threshold_secs: None,
            },
            Preemption {
                event_id: EventId::new(0),
                task_id: Some(TaskId::new(1)),
                threshold: IDLE,
                via: "input",
                threshold_secs: None,
            },
        ];
        let alert = |kind| RunningAlert::new(kind, None, None);
        let running = [
            alert("idle_without_receipt"),
            alert("long_background"),
            alert("long_background"),
        ];
        let stats = thresholds(
            &found,
            &misses,
            |id, task| id > EventId::new(0) && task == Some(TaskId::new(1)),
            &running,
            &StallConfig::default(),
        );
        assert_eq!(
            stats.keys().copied().collect::<Vec<_>>(),
            [BACKGROUND, IDLE, SEND]
        );
        let idle = &stats[IDLE];
        assert_eq!((idle.threshold_secs, idle.detections), (1200, 3));
        assert_eq!(idle.by_detection["nudge"].count, 2);
        assert_eq!(idle.by_detection["ask"].outcomes["answered_wait"], 1);
        assert_eq!(
            idle.outcomes,
            BTreeMap::from([
                ("answered_wait".to_owned(), 1),
                ("escalated".to_owned(), 1),
                ("resolved_by_nudge".to_owned(), 1),
            ])
        );
        assert_eq!(
            idle.detected_after_secs,
            Durations {
                count: 3,
                median: Some(1210),
                max: Some(1300)
            }
        );
        assert_eq!(
            idle.resolved_after_secs,
            Durations {
                count: 3,
                median: Some(121),
                max: Some(130)
            }
        );
        assert_eq!(idle.by_threshold_secs["1200"].count, 2);
        assert_eq!(idle.by_threshold_secs["600"].outcomes["answered_wait"], 1);
        assert_eq!(idle.preempted.count, 1);
        assert_eq!(idle.preempted.by_via["recover"], 1);
        assert_eq!(idle.running_alerts, 1);
        let send = &stats[SEND];
        assert_eq!((send.threshold_secs, send.detections), (60, 1));
        assert_eq!(send.by_threshold_secs["unrecorded"].count, 1);
        assert_eq!(send.detected_after_secs, Durations::default());
        assert_eq!(send.running_alerts, 0);
        let background = &stats[BACKGROUND];
        assert_eq!((background.detections, background.running_alerts), (0, 2));
    }
}
