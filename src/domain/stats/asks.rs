//! The asks that reached a person (task 325): how many were opened, of
//! which kind and by whom, and how many were answered, by whom and with
//! which option. Derived from `ask_opened` / `ask_answered` like the rest of
//! `stats`; an `ask_answered` recorded before its answerer was kept counts
//! as [`UNKNOWN`]. Per why a person was needed (`reason_category`,
//! ADR-0047 decision 45, task 439), the asks opened, answered and still
//! open in the window, next to `auto_repairs` of the same window. Per kind
//! and per asker, how long the asks waited for their answer and for the
//! answer to be applied, and how long the open ones have waited (task 468).
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, landing::p90, median, timestamp_millis};

/// The answerer (or asker) of an event that does not name one.
pub const UNKNOWN: &str = "unknown";

/// The asks opened and answered in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AskStats {
    pub opened: OpenedAsks,
    pub answered: AnsweredAsks,
    /// Per `reason_category` (ADR-0047 decision 41), the asks of the window
    /// opened, answered and still open; `unknown` for an ask opened before
    /// the reason was kept.
    pub by_reason_category: BTreeMap<String, ReasonAsks>,
    /// How long the asks waited, per kind and per asker (task 468).
    pub times: AskTimes,
}

/// How long the asks waited (in seconds), per the kind and the asker
/// (`asked_by`) of their `ask_opened`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AskTimes {
    pub by_kind: BTreeMap<String, AskWaits>,
    pub by_asked_by: BTreeMap<String, AskWaits>,
}

/// The waits of one group of asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AskWaits {
    /// From `ask_opened` to `ask_answered`, of the asks answered in the
    /// window.
    pub to_answer: Spread,
    /// From `ask_opened` to the answer applied, of the asks applied in the
    /// window: the first event after `ask_answered` that names the ask
    /// (`ask_closed`, `ask_delivered`, the runtime's own event of applying
    /// it such as `integration_approved` or `triage_decided`), or the
    /// answer itself when the runtime closed the ask (`runtime_closed`).
    pub to_apply: Spread,
    /// From `ask_answered` to the answer applied, of the same asks.
    pub answer_to_apply: Spread,
    /// Of the asks with no `ask_answered` by the window's end (opened in
    /// the window or before), how long they have waited by then.
    pub open: Spread,
}

/// Count, median, nearest-rank 90th percentile and maximum of a set of
/// seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Spread {
    pub count: usize,
    pub median: Option<i64>,
    pub p90: Option<i64>,
    pub max: Option<i64>,
}

impl Spread {
    fn of(mut secs: Vec<i64>) -> Self {
        Self {
            count: secs.len(),
            median: median(&mut secs),
            p90: p90(&mut secs),
            max: secs.iter().copied().max(),
        }
    }
}

/// The seconds of one group of asks, before they are summarized.
#[derive(Default)]
struct WaitSecs {
    to_answer: Vec<i64>,
    to_apply: Vec<i64>,
    answer_to_apply: Vec<i64>,
    open: Vec<i64>,
}

impl WaitSecs {
    fn is_empty(&self) -> bool {
        self.to_answer.is_empty()
            && self.to_apply.is_empty()
            && self.answer_to_apply.is_empty()
            && self.open.is_empty()
    }

    fn add(&mut self, other: &Self) {
        self.to_answer.extend(&other.to_answer);
        self.to_apply.extend(&other.to_apply);
        self.answer_to_apply.extend(&other.answer_to_apply);
        self.open.extend(&other.open);
    }

    fn summary(self) -> AskWaits {
        AskWaits {
            to_answer: Spread::of(self.to_answer),
            to_apply: Spread::of(self.to_apply),
            answer_to_apply: Spread::of(self.answer_to_apply),
            open: Spread::of(self.open),
        }
    }
}

/// One ask up to the window's end, as its events tell it.
struct AskTrack {
    reason: String,
    kind: String,
    asked_by: String,
    task_id: Option<TaskId>,
    opened_ms: Option<i64>,
    /// The first `ask_answered`: its id and time.
    answered: Option<(EventId, Option<i64>)>,
    /// The first event that applied the answer: its id and time.
    applied: Option<(EventId, Option<i64>)>,
}

/// The events that do not apply an answer, though they name the ask: the
/// ask's own, and a run's wait on it, which ends when the answer arrives,
/// before it is delivered once the run has its slot back.
const NOT_APPLYING: &[&str] = &[
    "ask_opened",
    "ask_answered",
    "ask_updated",
    "ask_delivery_failed",
    "run_waiting_started",
    "run_waiting_ask_added",
    "run_waiting_ended",
    "run_waiting_deferred",
    "run_slot_regained",
];

/// The asks of one `reason_category` in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReasonAsks {
    /// `ask_opened` in the window.
    pub opened: i64,
    /// `ask_answered` in the window, of an ask opened in it or before.
    pub answered: i64,
    /// Of those opened in the window, the ones with no `ask_answered` by
    /// its end.
    pub open: i64,
}

/// The `ask_opened` events of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OpenedAsks {
    pub count: i64,
    pub by_kind: BTreeMap<String, i64>,
    /// By the role that registered the ask (`asked_by`).
    pub by_asked_by: BTreeMap<String, i64>,
    /// By why a person was needed (`reason_category`, ADR-0047 decision
    /// 41), `unknown` for an ask opened before the reason was kept.
    pub by_reason_category: BTreeMap<String, i64>,
}

/// The `ask_answered` events of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AnsweredAsks {
    pub count: i64,
    pub by_kind: BTreeMap<String, i64>,
    /// By who answered: `person`, a session role (`inbox`, `planner`),
    /// `runtime`, or `unknown` for an answer recorded before it was kept.
    pub by_answered_by: BTreeMap<String, i64>,
    /// Per ask kind, what the answers chose.
    pub choices: BTreeMap<String, Choices>,
}

/// What the answers to one kind of ask chose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Choices {
    /// Per option text, the answers that chose it.
    pub by_option: BTreeMap<String, i64>,
    /// Answers that chose no option.
    pub free: i64,
    /// Answers recorded before the choice was kept.
    pub unknown: i64,
}

/// Count the `ask_opened` and `ask_answered` events with `after < id <=
/// upto` whose task `counts` accepts; the asks still open are timed up to
/// `end_ms`, the window's end.
pub fn asks(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> AskStats {
    let mut stats = AskStats::default();
    let text = |payload: &Value, key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    // Up to the window's end, each ask as `ask_opened` recorded it (its
    // reason, kind and asker), and when it was answered and applied: an
    // answer the runtime wrote itself does not carry the reason, and an ask
    // opened in the window may be answered after it.
    let mut tracks: HashMap<AskKey, AskTrack> = HashMap::new();
    // By the ask's id alone: the events that apply an answer name the ask
    // but may sit on another run or none.
    let mut by_id: HashMap<String, AskKey> = HashMap::new();
    for event in events.iter().filter(|event| event.id <= upto) {
        let at = || (event.id, timestamp_millis(&event.created_at));
        match event.kind.as_str() {
            "ask_opened" => {
                let key = ask_key(event);
                if let Some(id) = &key.0 {
                    by_id.insert(id.clone(), key.clone());
                }
                tracks.insert(
                    key,
                    AskTrack {
                        reason: text(&event.payload, "reason_category"),
                        kind: text(&event.payload, "kind"),
                        asked_by: text(&event.payload, "asked_by"),
                        task_id: event.task_id,
                        opened_ms: timestamp_millis(&event.created_at),
                        answered: None,
                        applied: None,
                    },
                );
            }
            "ask_answered" => {
                if let Some(track) = tracks.get_mut(&ask_key(event))
                    && track.answered.is_none()
                {
                    track.answered = Some(at());
                    if event.payload.get("runtime_closed") == Some(&Value::Bool(true)) {
                        track.applied = Some(at());
                    }
                }
            }
            kind if !NOT_APPLYING.contains(&kind) => {
                // Only `ask_id` names the ask here: another event's `id`
                // may be something else.
                if let Some(track) = event
                    .payload
                    .get("ask_id")
                    .and_then(|id| {
                        by_id.get(&id.as_str().map_or_else(|| id.to_string(), str::to_owned))
                    })
                    .and_then(|key| tracks.get_mut(key))
                    && track.answered.is_some()
                    && track.applied.is_none()
                {
                    track.applied = Some(at());
                }
            }
            _ => {}
        }
    }
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto && counts(event.task_id))
    {
        let payload = &event.payload;
        match event.kind.as_str() {
            "ask_opened" => {
                let opened = &mut stats.opened;
                opened.count += 1;
                *opened.by_kind.entry(text(payload, "kind")).or_default() += 1;
                *opened
                    .by_asked_by
                    .entry(text(payload, "asked_by"))
                    .or_default() += 1;
                let reason = text(payload, "reason_category");
                *opened.by_reason_category.entry(reason.clone()).or_default() += 1;
                let counts = stats.by_reason_category.entry(reason).or_default();
                counts.opened += 1;
                if tracks
                    .get(&ask_key(event))
                    .is_some_and(|track| track.answered.is_none())
                {
                    counts.open += 1;
                }
            }
            "ask_answered" => {
                let reason = match payload.get("reason_category").and_then(Value::as_str) {
                    Some(reason) => reason.to_owned(),
                    None => tracks
                        .get(&ask_key(event))
                        .map_or_else(|| UNKNOWN.to_owned(), |track| track.reason.clone()),
                };
                stats.by_reason_category.entry(reason).or_default().answered += 1;
                let answered = &mut stats.answered;
                let kind = text(payload, "kind");
                answered.count += 1;
                *answered.by_kind.entry(kind.clone()).or_default() += 1;
                *answered
                    .by_answered_by
                    .entry(text(payload, "answered_by"))
                    .or_default() += 1;
                let choices = answered.choices.entry(kind).or_default();
                match payload.get("option").and_then(Value::as_str) {
                    Some(option) => *choices.by_option.entry(option.to_owned()).or_default() += 1,
                    None if payload.get("answered_by").is_some() => choices.free += 1,
                    None => choices.unknown += 1,
                }
            }
            _ => {}
        }
    }
    stats.times = times(&tracks, after, end_ms, counts);
    stats
}

/// The waits of the `tracks` whose task `counts` accepts: answered or
/// applied after `after` (the tracks end at the window's end), or still
/// open at `end_ms`.
fn times(
    tracks: &HashMap<AskKey, AskTrack>,
    after: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> AskTimes {
    let mut by_kind: BTreeMap<String, WaitSecs> = BTreeMap::new();
    let mut by_asked_by: BTreeMap<String, WaitSecs> = BTreeMap::new();
    let secs = |from: Option<i64>, to: Option<i64>| Some((to? - from?) / 1000);
    for track in tracks.values().filter(|track| counts(track.task_id)) {
        let mut own = WaitSecs::default();
        match track.answered {
            None => own.open.extend(secs(track.opened_ms, Some(end_ms))),
            Some((id, at)) if id > after => own.to_answer.extend(secs(track.opened_ms, at)),
            Some(_) => {}
        }
        if let (Some((_, answered)), Some((id, at))) = (track.answered, track.applied)
            && id > after
        {
            own.to_apply.extend(secs(track.opened_ms, at));
            own.answer_to_apply.extend(secs(answered, at));
        }
        if own.is_empty() {
            continue;
        }
        by_kind.entry(track.kind.clone()).or_default().add(&own);
        by_asked_by
            .entry(track.asked_by.clone())
            .or_default()
            .add(&own);
    }
    let summary = |groups: BTreeMap<String, WaitSecs>| {
        groups
            .into_iter()
            .map(|(name, secs)| (name, secs.summary()))
            .collect()
    };
    AskTimes {
        by_kind: summary(by_kind),
        by_asked_by: summary(by_asked_by),
    }
}

/// What pairs an `ask_answered` with its `ask_opened`: the payload's
/// `ask_id` (or `id`), and the task and run, as `stats`' open asks do.
type AskKey = (Option<String>, Option<TaskId>, Option<String>);

fn ask_key(event: &RunEvent) -> AskKey {
    let id = event
        .payload
        .get("ask_id")
        .or_else(|| event.payload.get("id"))
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned));
    (
        id,
        event.task_id,
        event.run_id.as_ref().map(|run| run.as_str().to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: Option<i64>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task_id.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    /// Opened asks count by kind and asker, answered ones by answerer and
    /// choice; an answer recorded before the answerer was kept is unknown,
    /// and events outside the window or of a task not counted are left out.
    #[test]
    fn counts_the_asks_opened_and_answered_in_the_window() {
        let events = [
            event(
                1,
                Some(1),
                "ask_opened",
                json!({"ask_id": 1, "kind": "decide", "asked_by": "triage", "reason_category": "recovery_failed"}),
            ),
            event(
                2,
                Some(1),
                "ask_opened",
                json!({"ask_id": 2, "kind": "approve_landing", "asked_by": "supervisor"}),
            ),
            event(
                3,
                None,
                "ask_opened",
                json!({"ask_id": 3, "kind": "blocked"}),
            ),
            event(
                4,
                Some(1),
                "ask_answered",
                json!({"ask_id": 2, "kind": "approve_landing", "answered_by": "inbox", "option_index": 0, "option": "land"}),
            ),
            event(
                5,
                Some(1),
                "ask_answered",
                json!({"ask_id": 1, "kind": "decide", "answered_by": "person", "option_index": null}),
            ),
            event(
                6,
                Some(2),
                "ask_answered",
                json!({"ask_id": 4, "kind": "stuck_exit", "runtime_closed": true, "answered_by": "runtime", "option_index": null}),
            ),
            event(
                7,
                Some(1),
                "ask_answered",
                json!({"ask_id": 5, "kind": "approve_landing"}),
            ),
            event(8, Some(1), "run_claimed", json!({})),
            event(
                9,
                Some(1),
                "ask_opened",
                json!({"ask_id": 6, "kind": "decide", "asked_by": "triage"}),
            ),
        ];
        let all = asks(&events, EventId::new(0), EventId::new(8), 0, |_| true);
        assert_eq!(all.opened.count, 3);
        assert_eq!(all.opened.by_kind["decide"], 1);
        assert_eq!(all.opened.by_asked_by[UNKNOWN], 1);
        assert_eq!(all.opened.by_asked_by["supervisor"], 1);
        assert_eq!(all.opened.by_reason_category["recovery_failed"], 1);
        assert_eq!(all.opened.by_reason_category[UNKNOWN], 2);
        assert_eq!(all.answered.count, 4);
        assert_eq!(all.answered.by_kind["approve_landing"], 2);
        assert_eq!(
            all.answered.by_answered_by,
            BTreeMap::from([
                ("inbox".to_owned(), 1),
                ("person".to_owned(), 1),
                ("runtime".to_owned(), 1),
                (UNKNOWN.to_owned(), 1),
            ])
        );
        assert_eq!(
            all.answered.choices["approve_landing"],
            Choices {
                by_option: BTreeMap::from([("land".to_owned(), 1)]),
                free: 0,
                unknown: 1,
            }
        );
        assert_eq!(all.answered.choices["decide"].free, 1);

        let task_one = asks(&events, EventId::new(1), EventId::new(9), 0, |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!(task_one.opened.count, 2);
        assert_eq!(task_one.opened.by_kind["decide"], 1);
        assert_eq!(task_one.answered.count, 3);
        assert!(!task_one.answered.by_kind.contains_key("stuck_exit"));
    }

    /// Per reason, the asks opened, answered and still open in the window:
    /// a `queue_hold` ask on neither a task nor a run counts; an answer the
    /// runtime wrote without the reason takes its `ask_opened`'s, also when
    /// that was before the window; an ask answered after the window is
    /// still open in it.
    #[test]
    fn counts_the_asks_by_reason_opened_answered_and_open() {
        let hold = json!({"ask_id": 3, "kind": "queue_hold", "asked_by": "supervisor", "reason_category": "authentication", "affected": ["r1"]});
        let events = [
            event(
                1,
                Some(1),
                "ask_opened",
                json!({"ask_id": 1, "kind": "stuck_exit", "reason_category": "recovery_failed"}),
            ),
            event(
                2,
                Some(1),
                "ask_opened",
                json!({"ask_id": 2, "kind": "approve_landing", "reason_category": "scope"}),
            ),
            event(3, None, "ask_opened", hold),
            event(
                4,
                Some(1),
                "ask_answered",
                json!({"ask_id": 1, "kind": "stuck_exit", "runtime_closed": true, "answered_by": "runtime"}),
            ),
            event(
                5,
                None,
                "ask_answered",
                json!({"ask_id": 3, "kind": "queue_hold", "reason_category": "authentication", "answered_by": "inbox"}),
            ),
            event(
                6,
                None,
                "ask_opened",
                json!({"ask_id": 4, "kind": "blocked"}),
            ),
            event(
                7,
                Some(1),
                "ask_answered",
                json!({"ask_id": 2, "kind": "approve_landing", "reason_category": "scope"}),
            ),
        ];
        let window = asks(&events, EventId::new(1), EventId::new(6), 0, |_| true);
        let reason = |opened, answered, open| ReasonAsks {
            opened,
            answered,
            open,
        };
        assert_eq!(
            window.by_reason_category,
            BTreeMap::from([
                ("authentication".to_owned(), reason(1, 1, 0)),
                ("recovery_failed".to_owned(), reason(0, 1, 0)),
                ("scope".to_owned(), reason(1, 0, 1)),
                (UNKNOWN.to_owned(), reason(1, 0, 1)),
            ])
        );
        assert_eq!(window.opened.by_reason_category["authentication"], 1);

        // With --goal a task-less ask is not counted, like the rest of `asks`.
        let goal = asks(&events, EventId::new(0), EventId::new(7), 0, |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!(
            goal.by_reason_category,
            BTreeMap::from([
                ("recovery_failed".to_owned(), reason(1, 1, 0)),
                ("scope".to_owned(), reason(1, 1, 0)),
            ])
        );
    }

    /// An event `secs` after midnight of 2026-09-26.
    fn at(id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            created_at: format!(
                "2026-09-26T{:02}:{:02}:{:02}.000Z",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ),
            ..event(id, Some(1), kind, payload)
        }
    }

    /// Per kind and asker, the time to the answer and to its application
    /// (the first event after the answer naming the ask, or the answer of
    /// an ask the runtime closed), and how long the open asks have waited
    /// by the window's end; an ask answered before the window, a mention
    /// of the ask before its answer, the end of a run's wait on it and a
    /// failed delivery are left out.
    #[test]
    fn times_the_asks_to_their_answer_and_its_application() {
        let events = [
            at(
                1,
                "ask_opened",
                json!({"ask_id": 9, "kind": "decide", "asked_by": "triage"}),
                0,
            ),
            at(2, "ask_answered", json!({"ask_id": 9, "kind": "decide"}), 5),
            at(
                3,
                "ask_opened",
                json!({"ask_id": 1, "kind": "approve_landing", "asked_by": "supervisor"}),
                10,
            ),
            at(
                4,
                "run_waiting_started",
                json!({"ask_id": 1, "phase": "review"}),
                11,
            ),
            at(
                5,
                "ask_opened",
                json!({"ask_id": 2, "kind": "approve_landing", "asked_by": "supervisor"}),
                20,
            ),
            at(
                6,
                "ask_opened",
                json!({"ask_id": 3, "kind": "stuck_exit", "asked_by": "supervisor"}),
                30,
            ),
            at(
                7,
                "ask_answered",
                json!({"ask_id": 3, "kind": "stuck_exit", "runtime_closed": true}),
                60,
            ),
            at(
                8,
                "ask_opened",
                json!({"ask_id": 4, "kind": "decide", "asked_by": "triage"}),
                100,
            ),
            at(
                9,
                "ask_answered",
                json!({"ask_id": 1, "kind": "approve_landing"}),
                110,
            ),
            at(
                10,
                "run_waiting_ended",
                json!({"ask_id": 1, "cause": "answered"}),
                110,
            ),
            at(
                10,
                "ask_delivery_failed",
                json!({"ask_id": 1, "error": "x"}),
                150,
            ),
            at(
                11,
                "integration_approved",
                json!({"ask_id": 1, "push": true}),
                410,
            ),
            at(
                12,
                "ask_closed",
                json!({"ask_id": 1, "kind": "approve_landing"}),
                420,
            ),
            at(
                13,
                "ask_answered",
                json!({"ask_id": 2, "kind": "approve_landing"}),
                3620,
            ),
            at(
                14,
                "ask_closed",
                json!({"ask_id": 2, "kind": "approve_landing"}),
                3720,
            ),
        ];
        let spread = |count, median, p90, max| Spread {
            count,
            median: Some(median),
            p90: Some(p90),
            max: Some(max),
        };
        let midnight = timestamp_millis("2026-09-26T00:00:00.000Z").unwrap();
        let times = asks(
            &events,
            EventId::new(2),
            EventId::new(14),
            midnight + 1_100_000,
            |_| true,
        )
        .times;
        let landing = &times.by_kind["approve_landing"];
        assert_eq!(landing.to_answer, spread(2, 1850, 3600, 3600));
        assert_eq!(landing.to_apply, spread(2, 2050, 3700, 3700));
        assert_eq!(landing.answer_to_apply, spread(2, 200, 300, 300));
        assert_eq!(landing.open, Spread::default());
        let stuck = &times.by_kind["stuck_exit"];
        assert_eq!(stuck.to_apply, spread(1, 30, 30, 30));
        assert_eq!(stuck.answer_to_apply, spread(1, 0, 0, 0));
        // Ask 9 was answered before the window; ask 4 is still open.
        let decide = &times.by_kind["decide"];
        assert_eq!(decide.to_answer, Spread::default());
        assert_eq!(decide.open, spread(1, 1000, 1000, 1000));
        assert_eq!(times.by_asked_by["supervisor"].to_answer.count, 3);
        assert_eq!(times.by_asked_by["triage"].open.count, 1);

        // Up to event 12 ask 2 is open, and its closing is past the window.
        let earlier = asks(
            &events,
            EventId::new(0),
            EventId::new(12),
            midnight + 1_000_000,
            |_| true,
        )
        .times;
        assert_eq!(
            earlier.by_kind["approve_landing"].open,
            spread(1, 980, 980, 980)
        );
        assert_eq!(earlier.by_kind["decide"].to_answer, spread(1, 5, 5, 5));
        // A task not counted leaves every wait out.
        let none = asks(&events, EventId::new(0), EventId::new(14), 0, |task| {
            task.is_none()
        });
        assert_eq!(none.times, AskTimes::default());
    }
}
