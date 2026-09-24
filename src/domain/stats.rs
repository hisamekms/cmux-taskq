//! `stats` (ADR-0023 decision 5): the time each run spent in work,
//! validation, waiting to land and startup, how often it came back, and the
//! thresholds it crossed. Everything is derived from `run_events`; this
//! module is a pure function of the events, the task → goal map and a
//! snapshot of the supervisors' free slots.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{
    GoalId, RunEvent, RunId, TaskId,
    reason::{REPEATED_CODE_KINDS, event_code},
};

/// Runs returned without `--full`.
pub const DEFAULT_RUNS: usize = 50;
/// A run waiting to land longer than this many seconds is an alert.
pub const AWAITING_INTEGRATION_SECS: i64 = 15 * 60;
/// The `needs_session` count of a run that is an alert.
pub const NEEDS_SESSION_TIMES: i64 = 3;
/// An ask open longer than this many seconds is an alert.
pub const ASK_UNANSWERED_SECS: i64 = 60 * 60;
/// The `failed` count across one task's runs that is an alert.
pub const TASK_FAILED_TIMES: i64 = 2;
/// A run whose work took more than this many times its goal's median is an alert.
pub const WORK_MEDIAN_FACTOR: i64 = 2;
/// This many `backend_call_failed` in one window is an alert.
pub const BACKEND_FAILURES: i64 = 2;

/// What `stats` looks at.
#[derive(Debug, Clone, Default)]
pub struct StatsQuery {
    /// Only runs that finished after this event id (`--since`).
    pub since: Option<i64>,
    /// Only runs of tasks in this goal (`--goal`).
    pub goal_id: Option<GoalId>,
    /// Every finished run instead of [`DEFAULT_RUNS`] (`--full`).
    pub full: bool,
}

/// The supervisors as they are now: execution slots nobody uses, the
/// dependency-ready tasks and the ready tasks that are still blocked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotSnapshot {
    pub free_slots: i64,
    pub candidates: usize,
    pub ready: usize,
}

/// One run's times in seconds (null when an end point was never recorded)
/// and counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunStats {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub goal_id: Option<GoalId>,
    /// `integrated`, `failed` or `interrupted` for a finished run; the last
    /// status recorded for one still in flight.
    pub status: Option<String>,
    /// The event that finished the run; `--since` compares against it.
    pub finished_event_id: Option<i64>,
    /// `run_claimed` → first `receipt_observed`.
    pub work: Option<i64>,
    /// First `receipt_observed` → first `validation_finished`.
    pub validate: Option<i64>,
    /// First `validation_finished` → `run_integrated`.
    pub wait_to_land: Option<i64>,
    /// `agent_started` → `first_commit_observed`.
    pub startup: Option<i64>,
    pub resumes: i64,
    /// The `verdict` of the last `review_finished`.
    pub review_verdict: Option<String>,
    pub needs_session: i64,
    pub failed: i64,
}

/// Count, sum and median of one interval over a set of runs; runs without
/// the interval are not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub count: usize,
    pub total: i64,
    pub median: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Intervals {
    pub runs: usize,
    pub work: Summary,
    pub validate: Summary,
    pub wait_to_land: Summary,
    pub startup: Summary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalStats {
    pub goal_id: Option<GoalId>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub kind: &'static str,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub value: i64,
    pub threshold: i64,
}

/// The `backend_call_failed` events in the window: how often cmux failed or
/// timed out, for which calls, and under what load. A window where nothing
/// failed has a zero count and null maxima.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BackendFailures {
    pub count: i64,
    /// Failures per `op`.
    pub by_op: BTreeMap<String, i64>,
    /// The highest 1-minute load average recorded with a failure.
    pub max_load_avg: Option<f64>,
    /// The most slots held when one failed.
    pub max_slots: Option<i64>,
}

/// The events in the window that carry a reason code (ADR-0034): how often
/// each code was recorded, and in which kinds of event. An event whose code
/// repeats that of the `validation_finished` recorded with it
/// (`evidence_missing`, `scope_violation`) is not counted again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReasonCodes {
    pub count: i64,
    /// Events per code.
    pub by_code: BTreeMap<String, i64>,
    /// Events per kind, then per code.
    pub by_kind: BTreeMap<String, BTreeMap<String, i64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    /// Finished runs, oldest finish first.
    pub runs: Vec<RunStats>,
    /// One entry per goal of those runs, ascending; runs of no goal last.
    pub goals: Vec<GoalStats>,
    pub overall: Intervals,
    pub alerts: Vec<Alert>,
    /// Failed backend calls after `--since` (up to `next_cursor`); without
    /// it, those since the earliest first event of the runs returned, or all of
    /// them with `--full` or when no run is returned. With `--goal`, only
    /// the failures of that goal's runs.
    pub backend_failures: BackendFailures,
    /// The reason codes recorded in the same window as `backend_failures`.
    pub reason_codes: ReasonCodes,
    /// Pass it to `--since` to read only runs that finish later.
    pub next_cursor: i64,
}

/// Aggregate `events` (every `run_events` row, ascending id). `goals` maps a
/// task to its goal, `now` is the unix second the open waits are measured
/// to, and `slots` is the supervisors' snapshot for the idle alert.
pub fn stats(
    events: &[RunEvent],
    goals: &HashMap<TaskId, Option<GoalId>>,
    now: i64,
    slots: SlotSnapshot,
    query: &StatsQuery,
) -> Stats {
    let latest = events.iter().map(|e| e.id).max().unwrap_or(0);
    let in_goal = |task_id: TaskId| {
        query
            .goal_id
            .is_none_or(|goal| goals.get(&task_id).copied().flatten() == Some(goal))
    };
    let tracks = runs(events, goals);
    let mut first_event: HashMap<&str, i64> = HashMap::new();
    for event in events {
        if let Some(run_id) = &event.run_id {
            first_event.entry(run_id.as_str()).or_insert(event.id);
        }
    }
    // Per task, over every run and not only this page: the `failed` count
    // and the latest run that failed.
    let mut failures: BTreeMap<TaskId, (i64, RunId)> = BTreeMap::new();
    for track in tracks.iter().filter(|track| track.stats.failed > 0) {
        let entry = failures
            .entry(track.stats.task_id)
            .or_insert_with(|| (0, track.stats.run_id.clone()));
        entry.0 += track.stats.failed;
        entry.1.clone_from(&track.stats.run_id);
    }
    let (finished, open): (Vec<_>, Vec<_>) = tracks
        .into_iter()
        .filter(|track| in_goal(track.stats.task_id))
        .partition(|track| track.stats.finished_event_id.is_some());
    let mut finished = finished
        .into_iter()
        .filter(|track| {
            query
                .since
                .is_none_or(|since| track.stats.finished_event_id > Some(since))
        })
        .collect::<Vec<_>>();
    finished.sort_by_key(|track| track.stats.finished_event_id);
    let limit = if query.full {
        finished.len()
    } else {
        DEFAULT_RUNS
    };
    let mut next_cursor = latest;
    if finished.len() > limit {
        if query.since.is_some() {
            // Page forward: the oldest runs past the cursor, then the rest.
            finished.truncate(limit);
            next_cursor = finished
                .last()
                .and_then(|track| track.stats.finished_event_id)
                .unwrap_or(latest);
        } else {
            finished.drain(..finished.len() - limit);
        }
    }

    let mut by_goal: BTreeMap<(bool, Option<GoalId>), Vec<&RunStats>> = BTreeMap::new();
    for track in &finished {
        let goal = track.stats.goal_id;
        by_goal
            .entry((goal.is_none(), goal))
            .or_default()
            .push(&track.stats);
    }
    let goal_stats = by_goal
        .iter()
        .map(|(&(_, goal_id), runs)| GoalStats {
            goal_id,
            intervals: intervals(runs),
        })
        .collect::<Vec<_>>();
    let overall = intervals(&finished.iter().map(|t| &t.stats).collect::<Vec<_>>());

    let mut alerts = Vec::new();
    let considered = finished.iter().chain(open.iter()).collect::<Vec<_>>();
    for track in &considered {
        let run = &track.stats;
        let waited = match (run.wait_to_land, track.awaiting_since) {
            (Some(wait), _) => Some(wait),
            (None, Some(since)) if run.status.as_deref() == Some("awaiting_integration") => {
                Some((now * 1000 - since) / 1000)
            }
            _ => None,
        };
        if let Some(waited) = waited.filter(|&w| w > AWAITING_INTEGRATION_SECS) {
            alerts.push(alert(
                "awaiting_integration",
                run,
                waited,
                AWAITING_INTEGRATION_SECS,
            ));
        }
        if run.needs_session >= NEEDS_SESSION_TIMES {
            alerts.push(alert(
                "needs_session",
                run,
                run.needs_session,
                NEEDS_SESSION_TIMES,
            ));
        }
        let median = goal_stats
            .iter()
            .find(|goal| goal.goal_id == run.goal_id)
            .and_then(|goal| goal.intervals.work.median);
        if let (Some(work), Some(median)) = (run.work, median)
            && median > 0
            && work > median * WORK_MEDIAN_FACTOR
        {
            alerts.push(alert(
                "work_over_median",
                run,
                work,
                median * WORK_MEDIAN_FACTOR,
            ));
        }
    }
    // Once the latest failed run of a task is on this page (or in flight).
    for track in &considered {
        if let Some((count, run_id)) = failures.get(&track.stats.task_id)
            && *run_id == track.stats.run_id
            && *count >= TASK_FAILED_TIMES
        {
            alerts.push(alert(
                "task_failed",
                &track.stats,
                *count,
                TASK_FAILED_TIMES,
            ));
        }
    }
    for ask in open_asks(events) {
        let waited = (now * 1000 - ask.opened_ms) / 1000;
        if waited > ASK_UNANSWERED_SECS && ask.task_id.is_none_or(in_goal) {
            alerts.push(Alert {
                kind: "ask_unanswered",
                task_id: ask.task_id,
                run_id: ask.run_id,
                value: waited,
                threshold: ASK_UNANSWERED_SECS,
            });
        }
    }
    let window_start = match query.since {
        Some(since) => since,
        None if query.full => 0,
        None => finished
            .iter()
            .filter_map(|track| first_event.get(track.stats.run_id.as_str()))
            .min()
            .map_or(0, |id| id - 1),
    };
    let counts = |task_id: Option<TaskId>| query.goal_id.is_none() || task_id.is_some_and(in_goal);
    let backend_failures = backend_failures(events, window_start, next_cursor, counts);
    let reason_codes = reason_codes(events, window_start, next_cursor, counts);
    if backend_failures.count >= BACKEND_FAILURES {
        alerts.push(Alert {
            kind: "backend_failures",
            task_id: None,
            run_id: None,
            value: backend_failures.count,
            threshold: BACKEND_FAILURES,
        });
    }
    if slots.free_slots > 0 && slots.candidates == 0 && slots.ready > 0 {
        alerts.push(Alert {
            kind: "idle_slots",
            task_id: None,
            run_id: None,
            value: slots.free_slots,
            threshold: 0,
        });
    }

    Stats {
        runs: finished.into_iter().map(|track| track.stats).collect(),
        goals: goal_stats,
        overall,
        alerts,
        backend_failures,
        reason_codes,
        next_cursor,
    }
}

/// Count the reason codes of the events with `after < id <= upto` whose
/// task `counts` accepts.
fn reason_codes(
    events: &[RunEvent],
    after: i64,
    upto: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ReasonCodes {
    let mut codes = ReasonCodes::default();
    for event in events.iter().filter(|event| {
        event.id > after
            && event.id <= upto
            && !REPEATED_CODE_KINDS.contains(&event.kind.as_str())
            && counts(event.task_id)
    }) {
        let Some(code) = event_code(event) else {
            continue;
        };
        codes.count += 1;
        *codes.by_code.entry(code.as_str().to_owned()).or_default() += 1;
        *codes
            .by_kind
            .entry(event.kind.clone())
            .or_default()
            .entry(code.as_str().to_owned())
            .or_default() += 1;
    }
    codes
}

/// Aggregate the `backend_call_failed` events with `after < id <= upto`
/// whose task `counts` accepts.
fn backend_failures(
    events: &[RunEvent],
    after: i64,
    upto: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BackendFailures {
    let mut failures = BackendFailures::default();
    for event in events.iter().filter(|event| {
        event.kind == "backend_call_failed"
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
    }) {
        failures.count += 1;
        let op = event.payload.get("op").and_then(Value::as_str);
        *failures
            .by_op
            .entry(op.unwrap_or("unknown").to_owned())
            .or_default() += 1;
        if let Some(load) = event.payload.get("load_avg").and_then(Value::as_f64) {
            failures.max_load_avg = Some(failures.max_load_avg.map_or(load, |max| max.max(load)));
        }
        if let Some(slots) = event.payload.get("slots").and_then(Value::as_i64) {
            failures.max_slots = Some(failures.max_slots.map_or(slots, |max| max.max(slots)));
        }
    }
    failures
}

fn alert(kind: &'static str, run: &RunStats, value: i64, threshold: i64) -> Alert {
    Alert {
        kind,
        task_id: Some(run.task_id),
        run_id: Some(run.run_id.clone()),
        value,
        threshold,
    }
}

/// The median of `values` (sorted in place); the lower-rounded mean of the
/// two middle values for an even count.
pub fn median(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    let n = values.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => Some((values[n / 2 - 1] + values[n / 2]).div_euclid(2)),
    }
}

fn summary(values: impl Iterator<Item = Option<i64>>) -> Summary {
    let mut values = values.flatten().collect::<Vec<_>>();
    Summary {
        count: values.len(),
        total: values.iter().sum(),
        median: median(&mut values),
    }
}

fn intervals(runs: &[&RunStats]) -> Intervals {
    Intervals {
        runs: runs.len(),
        work: summary(runs.iter().map(|r| r.work)),
        validate: summary(runs.iter().map(|r| r.validate)),
        wait_to_land: summary(runs.iter().map(|r| r.wait_to_land)),
        startup: summary(runs.iter().map(|r| r.startup)),
    }
}

/// A run as its events describe it, with the instants (unix milliseconds)
/// the intervals are measured between.
struct Track {
    stats: RunStats,
    claimed: Option<i64>,
    receipt: Option<i64>,
    validated: Option<i64>,
    agent_started: Option<i64>,
    awaiting_since: Option<i64>,
}

fn payload_status(payload: &Value) -> Option<&str> {
    payload.get("status").and_then(Value::as_str)
}

fn seconds_between(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    Some((to? - from?) / 1000)
}

/// Group the run events by run, in order of each run's first event.
fn runs(events: &[RunEvent], goals: &HashMap<TaskId, Option<GoalId>>) -> Vec<Track> {
    let mut order: Vec<RunId> = Vec::new();
    let mut tracks: HashMap<RunId, Track> = HashMap::new();
    for event in events {
        let (Some(run_id), Some(task_id)) = (&event.run_id, event.task_id) else {
            continue;
        };
        let at = timestamp_millis(&event.created_at);
        let track = tracks.entry(run_id.clone()).or_insert_with(|| {
            order.push(run_id.clone());
            Track {
                stats: RunStats {
                    run_id: run_id.clone(),
                    task_id,
                    goal_id: goals.get(&task_id).copied().flatten(),
                    status: None,
                    finished_event_id: None,
                    work: None,
                    validate: None,
                    wait_to_land: None,
                    startup: None,
                    resumes: 0,
                    review_verdict: None,
                    needs_session: 0,
                    failed: 0,
                },
                claimed: None,
                receipt: None,
                validated: None,
                agent_started: None,
                awaiting_since: None,
            }
        });
        let run = &mut track.stats;
        match event.kind.as_str() {
            "run_claimed" => track.claimed = track.claimed.or(at),
            "agent_started" => track.agent_started = track.agent_started.or(at),
            "first_commit_observed" if run.startup.is_none() => {
                run.startup = seconds_between(track.agent_started, at);
            }
            "receipt_observed" if track.receipt.is_none() => {
                track.receipt = at;
                run.work = seconds_between(track.claimed, at);
            }
            "validation_finished" if track.validated.is_none() => {
                track.validated = at;
                run.validate = seconds_between(track.receipt, at);
            }
            "integration_started" => run.status = Some("integrating".to_owned()),
            "resume_started" => run.resumes += 1,
            "review_finished" => {
                run.review_verdict = event
                    .payload
                    .get("verdict")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "run_integrated" => {
                run.wait_to_land = seconds_between(track.validated, at);
                run.status = Some("integrated".to_owned());
                run.finished_event_id.get_or_insert(event.id);
            }
            _ => {}
        }
        if let Some(status) = payload_status(&event.payload) {
            run.status = Some(status.to_owned());
            match status {
                // `integration_error` only puts back the status the run had
                // before the attempt, and `resume_finished` reports the run
                // still parked (ADR-0019); neither parks anything new.
                "needs_session"
                    if !matches!(event.kind.as_str(), "integration_error" | "resume_finished") =>
                {
                    run.needs_session += 1
                }
                "failed" => run.failed += 1,
                _ => {}
            }
            // The wait starts at the first `awaiting_integration`, like
            // `wait_to_land`; going back there after an error keeps it.
            if status == "awaiting_integration" && track.awaiting_since.is_none() {
                track.awaiting_since = at;
            }
            if matches!(status, "failed" | "interrupted") {
                run.finished_event_id.get_or_insert(event.id);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| tracks.remove(&id))
        .collect()
}

struct OpenAsk {
    task_id: Option<TaskId>,
    run_id: Option<RunId>,
    opened_ms: i64,
}

/// Asks (ADR-0022) whose `ask_opened` has no `ask_answered` yet. The two
/// events are paired by the payload's `ask_id` (or `id`), and by run and
/// task when neither is recorded.
fn open_asks(events: &[RunEvent]) -> Vec<OpenAsk> {
    let key = |event: &RunEvent| {
        let id = event
            .payload
            .get("ask_id")
            .or_else(|| event.payload.get("id"))
            .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned));
        (id, event.task_id, event.run_id.clone())
    };
    let mut open: Vec<(_, OpenAsk)> = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "ask_opened" => {
                if let Some(opened_ms) = timestamp_millis(&event.created_at) {
                    open.push((
                        key(event),
                        OpenAsk {
                            task_id: event.task_id,
                            run_id: event.run_id.clone(),
                            opened_ms,
                        },
                    ));
                }
            }
            "ask_answered" => {
                let answered = key(event);
                open.retain(|(opened, _)| *opened != answered);
            }
            _ => {}
        }
    }
    open.into_iter().map(|(_, ask)| ask).collect()
}

/// Unix milliseconds of a queue timestamp `YYYY-MM-DDTHH:MM:SS[.fff]Z`
/// (SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ')`); `None` when it does not parse.
pub fn timestamp_millis(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = text.split_once(['T', ' '])?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.splitn(3, ':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let seconds = time.next()?;
    let (second, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second = second.parse::<i64>().ok()?;
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id,
            task_id: Some(TaskId::new(task_id)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn reason_codes_are_counted_once_per_park_within_the_window() {
        let events = [
            event(
                1,
                1,
                "supervision_finished",
                json!({"code": "session_killed"}),
            ),
            event(
                2,
                1,
                "validation_finished",
                json!({"code": "evidence_missing"}),
            ),
            event(
                3,
                1,
                "evidence_missing",
                json!({"code": "evidence_missing"}),
            ),
            event(
                4,
                2,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            ),
            event(
                5,
                1,
                "runtime_error",
                json!({"message": "before the codes"}),
            ),
            event(
                6,
                1,
                "supervision_finished",
                json!({"code": "session_killed"}),
            ),
            event(
                7,
                1,
                "backend_call_failed",
                json!({"code": "backend_failed"}),
            ),
        ];
        let codes = reason_codes(&events, 0, 5, |_| true);
        assert_eq!(codes.count, 3);
        assert_eq!(
            codes.by_code,
            BTreeMap::from([
                ("evidence_missing".to_owned(), 1),
                ("rebase_conflict".to_owned(), 1),
                ("session_killed".to_owned(), 1),
            ])
        );
        assert_eq!(
            codes.by_kind["integration_deferred"],
            BTreeMap::from([("rebase_conflict".to_owned(), 1)])
        );
        assert_eq!(reason_codes(&events, 5, 7, |_| true).count, 1);
        let task_two = reason_codes(&events, 0, 6, |task| task == Some(TaskId::new(2)));
        assert_eq!(task_two.count, 1);
    }
}
