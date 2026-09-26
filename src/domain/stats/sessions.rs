//! The Claude sessions of `stats` (ADR-0048 decisions 3, 11 and 12): the
//! spans `session_opened` / `session_closed` recorded, their open time
//! (wall-clock, in seconds) per kind over the window, per run and per goal.
//! Active time is the transcript's, which a later task records; until then
//! no span has it and its summaries are empty.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{Summary, landing::p90, median, timestamp_millis};
use crate::domain::{
    EventId, GoalId, RunEvent, RunId, TaskId,
    sessions::{INFERRED, KINDS, SESSION_CLOSED, SESSION_OPENED},
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
    /// The seconds the transcript's turns took, when they were recorded.
    pub active: Option<i64>,
    /// The span closed without its active time.
    pub active_unavailable: bool,
}

impl Span {
    /// Seconds open between `from` and `to` (unix milliseconds), an open
    /// span counted to `to`.
    fn open_secs(&self, from: Option<i64>, to: i64) -> i64 {
        let start = from.map_or(self.start, |from| self.start.max(from));
        let end = self.end.map_or(to, |end| end.min(to));
        (end - start).max(0) / 1000
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
                });
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

/// One kind's spans in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KindSessions {
    pub count: usize,
    pub open: TimeSummary,
    /// Only the spans whose active time was recorded.
    pub active: TimeSummary,
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
            active: span.active,
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
    let mut values: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for span in spans {
        let (open, active) = values.entry(span.kind.clone()).or_default();
        open.push(span.open);
        active.extend(span.active);
    }
    values
        .into_iter()
        .map(|(kind, (mut open, mut active))| {
            let summary = |values: &mut Vec<i64>| Summary {
                count: values.len(),
                total: values.iter().sum(),
                median: median(values),
            };
            let sessions = GoalKindSessions {
                count: open.len(),
                open: summary(&mut open),
                active: summary(&mut active),
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
    let mut by_kind: BTreeMap<&'static str, (KindSessions, Vec<i64>, Vec<i64>)> = KINDS
        .iter()
        .map(|&kind| (kind, Default::default()))
        .collect();
    for span in spans.iter().filter(|span| {
        span.opened <= window.upto
            && span.closed.is_none_or(|closed| closed > window.after)
            && counts(span)
    }) {
        let Some((sessions, open, active)) = by_kind.get_mut(span.kind.as_str()) else {
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
        open.push(clipped.open_secs(from, end));
        if inferred {
            sessions.inferred += 1;
        }
        if unavailable {
            sessions.active_unavailable += 1;
        }
        if closed_in_window {
            active.extend(span.active);
        }
    }
    Sessions {
        window,
        by_kind: by_kind
            .into_iter()
            .map(|(kind, (mut sessions, open, active))| {
                sessions.open = TimeSummary::of(open);
                sessions.active = TimeSummary::of(active);
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
        assert_eq!(none.by_kind["worker"].count, 0);
        assert_eq!(
            serde_json::to_value(none.window).unwrap(),
            json!({"after": 0, "upto": 4})
        );
    }
}
