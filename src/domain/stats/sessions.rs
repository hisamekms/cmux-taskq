//! The Claude sessions of `stats` (ADR-0048 decisions 3, 11 and 12): the
//! spans `session_opened` / `session_closed` recorded, their open time
//! (wall-clock, in seconds) per kind over the window, per run and per goal.
//! Active time is the sum of the span's transcript turns (decision 5): the
//! `session_turns` recorded while it was open and when it closed, cut to
//! the window like the open time; `active_ratio` is the active time over
//! the open time of the spans that have it.

use std::collections::{BTreeMap, HashMap};

use serde::{Serialize, Serializer};
use serde_json::Value;

use super::{Summary, landing::p90, median, rfc3339_millis, timestamp_millis};
use crate::domain::{
    EventId, GoalId, RunEvent, RunId, TaskId,
    sessions::{INFERRED, KINDS, SESSION_CLOSED, SESSION_OPENED, SESSION_TURNS},
    transcript::Turn,
};

/// One span as its events recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub opened: EventId,
    pub closed: Option<EventId>,
    pub kind: String,
    pub run_id: Option<RunId>,
    pub task_id: Option<TaskId>,
    /// A plan review's: the goals of its proposal's tasks.
    pub goal_ids: Vec<GoalId>,
    /// Unix milliseconds.
    pub start: i64,
    pub end: Option<i64>,
    pub inferred: bool,
    /// The seconds the transcript's turns took, recorded when it closed.
    pub active: Option<i64>,
    /// The span closed without its active time.
    pub active_unavailable: bool,
    /// Its transcript turns recorded so far (unix milliseconds).
    pub turns: Vec<Turn>,
}

impl Span {
    /// Seconds open between `from` and `to` (unix milliseconds), an open
    /// span counted to `to`.
    fn open_secs(&self, from: Option<i64>, to: i64) -> i64 {
        let start = from.map_or(self.start, |from| self.start.max(from));
        let end = self.end.map_or(to, |end| end.min(to));
        (end - start).max(0) / 1000
    }

    /// Seconds active, whole: as recorded when it closed, the turns
    /// recorded so far while it is open; `None` when it has none.
    fn active_secs(&self) -> Option<i64> {
        if self.closed.is_some() {
            return self.active;
        }
        (!self.turns.is_empty()).then(|| self.turns.iter().map(|t| t.millis()).sum::<i64>() / 1000)
    }
}

/// The spans of `events` (ascending id). A `session_closed` naming no
/// `session_opened`, and a span whose time does not parse, are left out.
pub fn spans(events: &[RunEvent]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut index: HashMap<EventId, usize> = HashMap::new();
    for event in events {
        match event.kind.as_str() {
            SESSION_OPENED => {
                let Some(start) = timestamp_millis(&event.created_at) else {
                    continue;
                };
                index.insert(event.id, spans.len());
                spans.push(Span {
                    opened: event.id,
                    closed: None,
                    kind: event.payload["kind"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    run_id: event.run_id.clone(),
                    task_id: event.task_id,
                    goal_ids: event.payload["goal_ids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_i64)
                        .map(GoalId::new)
                        .collect(),
                    start,
                    end: None,
                    inferred: false,
                    active: None,
                    active_unavailable: false,
                    turns: Vec::new(),
                });
            }
            SESSION_TURNS => {
                let opened = event.payload["opened_event_id"].as_i64().map(EventId::new);
                let Some(span) = opened
                    .and_then(|opened| index.get(&opened))
                    .and_then(|&at| spans.get_mut(at))
                else {
                    continue;
                };
                let turns = event.payload["turns"].as_array().into_iter().flatten();
                span.turns.extend(turns.filter_map(|turn| {
                    let time = |at: usize| turn.get(at)?.as_str().and_then(rfc3339_millis);
                    Some(Turn {
                        start: time(0)?,
                        end: time(1)?,
                    })
                }));
            }
            SESSION_CLOSED => {
                let opened = event.payload["opened_event_id"].as_i64().map(EventId::new);
                let Some(span) = opened
                    .and_then(|opened| index.get(&opened))
                    .and_then(|&at| spans.get_mut(at))
                else {
                    continue;
                };
                if span.closed.is_some() {
                    continue;
                }
                span.closed = Some(event.id);
                span.end = timestamp_millis(&event.created_at).map(|end| end.max(span.start));
                span.inferred = event.payload["reason"] == INFERRED;
                span.active = event.payload["active_secs"].as_i64();
                span.active_unavailable = event.payload["active"] == "unavailable";
            }
            _ => {}
        }
    }
    spans
}

/// Count, sum, median, 90th percentile and maximum of seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TimeSummary {
    #[serde(flatten)]
    pub summary: Summary,
    pub p90: Option<i64>,
    pub max: Option<i64>,
}

impl TimeSummary {
    fn of(mut values: Vec<i64>) -> Self {
        Self {
            summary: Summary {
                count: values.len(),
                total: values.iter().sum(),
                median: median(&mut values),
            },
            p90: p90(&mut values),
            max: values.iter().copied().max(),
        }
    }
}

/// Active time over open time, to three places; serialized as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio(i64);

impl Ratio {
    /// `part / whole`; `None` when `whole` is not positive.
    pub fn of(part: i64, whole: i64) -> Option<Self> {
        (whole > 0).then(|| Self((part * 1000 + whole / 2) / whole))
    }

    pub fn thousandths(self) -> i64 {
        self.0
    }
}

impl Serialize for Ratio {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[allow(clippy::cast_precision_loss)]
        serializer.serialize_f64(self.0 as f64 / 1000.0)
    }
}

/// One kind's spans in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KindSessions {
    pub count: usize,
    pub open: TimeSummary,
    /// Only the spans whose active time was recorded.
    pub active: TimeSummary,
    /// `active.total` over the open time of those spans; null without one.
    pub active_ratio: Option<Ratio>,
    /// Spans not closed yet, counted open to the window's end.
    pub open_now: usize,
    /// Spans the runtime closed because nothing recorded their end.
    pub inferred: usize,
    /// Spans closed without their active time.
    pub active_unavailable: usize,
}

/// The window the sessions were counted in: the events after `after` up to
/// `upto`, as `backend_failures`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SessionWindow {
    pub after: EventId,
    pub upto: EventId,
}

/// The sessions of the window, per kind; every kind is listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sessions {
    pub window: SessionWindow,
    pub by_kind: BTreeMap<&'static str, KindSessions>,
}

/// One kind's spans of a run: how many, and their seconds open and active
/// in total (active null when none was recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunKindSessions {
    pub count: usize,
    pub open: i64,
    pub active: Option<i64>,
}

/// One kind's spans over a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GoalKindSessions {
    pub count: usize,
    pub open: Summary,
    pub active: Summary,
    /// `active.total` over the open time of the spans that have it.
    pub active_ratio: Option<Ratio>,
}

/// A span of a run as the per-goal summaries use it: its kind, seconds
/// open and seconds active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpan {
    pub kind: String,
    pub open: i64,
    pub active: Option<i64>,
}

/// The spans of `run`, whole (not cut to a window), an open one counted to
/// `now` (unix milliseconds).
pub fn run_spans(spans: &[Span], run: &RunId, now: i64) -> Vec<RunSpan> {
    spans
        .iter()
        .filter(|span| span.run_id.as_ref() == Some(run))
        .map(|span| RunSpan {
            kind: span.kind.clone(),
            open: span.open_secs(None, now),
            active: span.active_secs(),
        })
        .collect()
}

/// A run's spans per kind; kinds without one are not listed.
pub fn per_run(spans: &[RunSpan]) -> BTreeMap<String, RunKindSessions> {
    let mut kinds: BTreeMap<String, RunKindSessions> = BTreeMap::new();
    for span in spans {
        let kind = kinds.entry(span.kind.clone()).or_default();
        kind.count += 1;
        kind.open += span.open;
        if let Some(active) = span.active {
            *kind.active.get_or_insert(0) += active;
        }
    }
    kinds
}

/// The spans of a set of runs per kind; kinds without one are not listed.
pub fn per_goal<'a>(
    spans: impl Iterator<Item = &'a RunSpan>,
) -> BTreeMap<String, GoalKindSessions> {
    let mut values: BTreeMap<String, (Vec<i64>, Vec<i64>, i64)> = BTreeMap::new();
    for span in spans {
        let (open, active, open_of_active) = values.entry(span.kind.clone()).or_default();
        open.push(span.open);
        active.extend(span.active);
        if span.active.is_some() {
            *open_of_active += span.open;
        }
    }
    values
        .into_iter()
        .map(|(kind, (mut open, mut active, open_of_active))| {
            let summary = |values: &mut Vec<i64>| Summary {
                count: values.len(),
                total: values.iter().sum(),
                median: median(values),
            };
            let active = summary(&mut active);
            let sessions = GoalKindSessions {
                count: open.len(),
                open: summary(&mut open),
                active_ratio: Ratio::of(active.total, open_of_active),
                active,
            };
            (kind, sessions)
        })
        .collect()
}

/// The spans that overlap the window `window` (by event id) per kind, their
/// time cut to it: from the time of the event `window.after` (from the
/// first span when it is 0) to `end` (unix milliseconds). `counts` says
/// which spans belong (`--goal`).
pub fn by_kind(
    spans: &[Span],
    events: &[RunEvent],
    window: SessionWindow,
    end: i64,
    counts: impl Fn(&Span) -> bool,
) -> Sessions {
    let from = (window.after.as_i64() > 0)
        .then(|| {
            events
                .iter()
                .rev()
                .find(|event| event.id <= window.after)
                .and_then(|event| timestamp_millis(&event.created_at))
        })
        .flatten();
    // Per kind: its sessions, the open and active seconds of its spans, and
    // the open seconds of the spans that have their active time.
    type Tally = (KindSessions, Vec<i64>, Vec<i64>, i64);
    let mut by_kind: BTreeMap<&'static str, Tally> = KINDS
        .iter()
        .map(|&kind| (kind, Default::default()))
        .collect();
    for span in spans.iter().filter(|span| {
        span.opened <= window.upto
            && span.closed.is_none_or(|closed| closed > window.after)
            && counts(span)
    }) {
        let Some((sessions, open, active, open_of_active)) = by_kind.get_mut(span.kind.as_str())
        else {
            continue;
        };
        sessions.count += 1;
        let closed_in_window = span.closed.is_some_and(|closed| closed <= window.upto);
        if !closed_in_window {
            sessions.open_now += 1;
        }
        let (span_end, inferred, unavailable) = if closed_in_window {
            (span.end, span.inferred, span.active_unavailable)
        } else {
            (None, false, false)
        };
        let clipped = Span {
            end: span_end,
            ..span.clone()
        };
        let open_secs = clipped.open_secs(from, end);
        open.push(open_secs);
        if inferred {
            sessions.inferred += 1;
        }
        if unavailable {
            sessions.active_unavailable += 1;
        }
        // Its turns that overlap the window: all of them when it closed
        // with its active time recorded, those recorded so far while open.
        let recorded = if closed_in_window {
            span.active.is_some()
        } else {
            !span.turns.is_empty()
        };
        if recorded {
            let millis: i64 = span
                .turns
                .iter()
                .map(|turn| turn.overlap(from.unwrap_or(i64::MIN), end))
                .sum();
            active.push(millis / 1000);
            *open_of_active += open_secs;
        }
    }
    Sessions {
        window,
        by_kind: by_kind
            .into_iter()
            .map(|(kind, (mut sessions, open, active, open_of_active))| {
                sessions.open = TimeSummary::of(open);
                sessions.active = TimeSummary::of(active);
                sessions.active_ratio = Ratio::of(sessions.active.summary.total, open_of_active);
                (kind, sessions)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, run: Option<&str>, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: run.map(|_| TaskId::new(1)),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: format!("1970-01-01T00:{:02}:{:02}.000Z", secs / 60, secs % 60),
        }
    }

    fn opened(id: i64, run: Option<&str>, kind: &str, secs: i64) -> RunEvent {
        event(id, run, SESSION_OPENED, json!({"kind": kind}), secs)
    }

    fn closed(id: i64, run: Option<&str>, opened: i64, reason: &str, secs: i64) -> RunEvent {
        event(
            id,
            run,
            SESSION_CLOSED,
            json!({"opened_event_id": opened, "reason": reason}),
            secs,
        )
    }

    fn fixture() -> Vec<RunEvent> {
        let run = Some("r1");
        vec![
            opened(1, run, "worker", 0),
            closed(2, run, 1, "next_span", 100),
            opened(3, run, "revise", 100),
            opened(4, run, "review", 110),
            closed(5, run, 4, "job_finished", 170),
            closed(6, run, 3, "exited", 300),
            // A second close of the same span is ignored.
            closed(7, run, 3, "inferred", 400),
            opened(8, None, "observer", 500),
            // A close naming no span is ignored.
            closed(9, None, 99, "exited", 510),
            opened(10, Some("r2"), "worker", 600),
            closed(11, Some("r2"), 10, "inferred", 900),
        ]
    }

    /// The turns recorded of a span are its active time: whole for a run,
    /// cut to the window per kind; an open span has the turns recorded so
    /// far, and one closed without its active time has none.
    #[test]
    fn active_time_is_the_turns_cut_to_the_window() {
        let run = Some("r1");
        let turns = |id: i64, opened: i64, turns: Value, secs: i64| {
            event(
                id,
                run,
                SESSION_TURNS,
                json!({"opened_event_id": opened, "turns": turns}),
                secs,
            )
        };
        let t = |secs: i64| format!("1970-01-01T00:{:02}:{:02}.000Z", secs / 60, secs % 60);
        let events = vec![
            opened(1, run, "worker", 0),
            turns(2, 1, json!([[t(10), t(40)], ["bad"]]), 50),
            turns(3, 1, json!([[t(60), t(90)]]), 100),
            event(
                4,
                run,
                SESSION_CLOSED,
                json!({"opened_event_id": 1, "reason": "exited", "active": "recorded", "active_secs": 60}),
                100,
            ),
            opened(5, run, "review", 100),
            turns(6, 5, json!([[t(110), t(130)]]), 140),
            opened(7, run, "triage", 200),
            event(
                8,
                run,
                SESSION_CLOSED,
                json!({"opened_event_id": 7, "reason": "job_finished", "active": "unavailable",
                       "active_unavailable": "transcript_missing"}),
                210,
            ),
            // Turns of no span are ignored.
            turns(9, 99, json!([[t(0), t(1)]]), 220),
        ];
        let spans = spans(&events);
        assert_eq!(spans[0].turns.len(), 2);
        let run_spans = run_spans(&spans, &RunId::new("r1").unwrap(), 300_000);
        let run = per_run(&run_spans);
        assert_eq!(run["worker"].active, Some(60));
        assert_eq!(run["review"].active, Some(20));
        assert_eq!(run["triage"].active, None);
        let goal = per_goal(run_spans.iter());
        assert_eq!(goal["worker"].active_ratio, Ratio::of(60, 100));
        assert_eq!(goal["triage"].active_ratio, None);

        let window = |after: i64, upto: i64, end: i64| {
            let window = SessionWindow {
                after: EventId::new(after),
                upto: EventId::new(upto),
            };
            by_kind(&spans, &events, window, end, |_| true)
        };
        let all = window(0, 9, 300_000);
        assert_eq!(all.by_kind["worker"].active.summary.total, 60);
        assert_eq!(
            all.by_kind["worker"].active_ratio.unwrap().thousandths(),
            600
        );
        // The review is still open: its turns so far.
        assert_eq!(all.by_kind["review"].active.summary.total, 20);
        assert_eq!(all.by_kind["triage"].active.summary.count, 0);
        assert_eq!(all.by_kind["triage"].active_unavailable, 1);
        // From event 2 (50 s) to 75 s: the worker's turns 10..40 and 60..90
        // overlap it by 15 seconds; the worker is open in it.
        let cut = window(2, 3, 75_000);
        assert_eq!(cut.by_kind["worker"].active.summary.total, 15);
        assert_eq!(cut.by_kind["worker"].open.summary.total, 25);
        assert_eq!(serde_json::to_value(Ratio::of(1, 3)).unwrap(), json!(0.333));
    }

    #[test]
    fn spans_pair_their_open_and_close() {
        let spans = spans(&fixture());
        assert_eq!(spans.len(), 5);
        assert_eq!(spans[0].kind, "worker");
        assert_eq!((spans[0].start, spans[0].end), (0, Some(100_000)));
        assert_eq!(spans[1].closed, Some(EventId::new(6)));
        assert!(!spans[1].inferred);
        assert_eq!(spans[3].end, None);
        assert!(spans[4].inferred);
    }

    /// A run's spans are whole; an open one runs to now.
    #[test]
    fn per_run_and_per_goal_sum_the_spans_by_kind() {
        let spans = spans(&fixture());
        let r1 = run_spans(&spans, &RunId::new("r1").unwrap(), 1_000_000);
        let run = per_run(&r1);
        assert_eq!(run.len(), 3);
        assert_eq!(
            run["revise"],
            RunKindSessions {
                count: 1,
                open: 200,
                active: None
            }
        );
        assert_eq!(run["review"].open, 60);
        let r2 = run_spans(&spans, &RunId::new("r2").unwrap(), 1_000_000);
        let goal = per_goal(r1.iter().chain(&r2));
        assert_eq!(goal["worker"].count, 2);
        assert_eq!(goal["worker"].open.total, 400);
        assert_eq!(goal["worker"].open.median, Some(200));
        assert_eq!(goal["worker"].active.count, 0);
        let with_active = [RunSpan {
            kind: "review".into(),
            open: 10,
            active: Some(4),
        }];
        assert_eq!(per_run(&with_active)["review"].active, Some(4));
        assert_eq!(per_goal(with_active.iter())["review"].active.total, 4);
    }

    /// The window cuts the spans' time, counts the open and inferred ones,
    /// and lists every kind.
    #[test]
    fn by_kind_cuts_the_spans_to_the_window() {
        let events = fixture();
        let spans = spans(&events);
        let all = by_kind(
            &spans,
            &events,
            SessionWindow {
                after: EventId::new(0),
                upto: EventId::new(11),
            },
            1_000_000,
            |_| true,
        );
        assert_eq!(all.by_kind.len(), KINDS.len());
        assert_eq!(all.by_kind["inbox"], KindSessions::default());
        let worker = &all.by_kind["worker"];
        assert_eq!(worker.count, 2);
        assert_eq!(worker.open.summary.total, 400);
        assert_eq!(worker.open.max, Some(300));
        assert_eq!(worker.open.p90, Some(300));
        assert_eq!(worker.inferred, 1);
        // The observer never closed: open to the window's end.
        let observer = &all.by_kind["observer"];
        assert_eq!((observer.count, observer.open_now), (1, 1));
        assert_eq!(observer.open.summary.total, 500);

        // After event 3 (100 s) up to event 6 (300 s): the worker's span
        // closed at event 2 is out; the revise is cut to 100..300.
        let window = SessionWindow {
            after: EventId::new(3),
            upto: EventId::new(6),
        };
        let cut = by_kind(&spans, &events, window, 300_000, |_| true);
        assert_eq!(cut.by_kind["worker"].count, 0);
        assert_eq!(cut.by_kind["revise"].open.summary.total, 200);
        assert_eq!(cut.by_kind["review"].open.summary.total, 60);
        assert_eq!(cut.by_kind["observer"].count, 0);
        // A span closed after the window is open in it.
        let early = SessionWindow {
            after: EventId::new(0),
            upto: EventId::new(4),
        };
        let open = by_kind(&spans, &events, early, 110_000, |_| true);
        assert_eq!(open.by_kind["revise"].open_now, 1);
        assert_eq!(open.by_kind["revise"].open.summary.total, 10);
        let none = by_kind(&spans, &events, early, 110_000, |_| false);
        assert_eq!(none.by_kind["worker"].active_ratio, None);
        assert_eq!(none.by_kind["worker"].count, 0);
        assert_eq!(
            serde_json::to_value(none.window).unwrap(),
            json!({"after": 0, "upto": 4})
        );
    }
}
