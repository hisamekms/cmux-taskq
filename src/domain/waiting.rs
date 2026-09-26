//! Runs waiting for a person outside the supervisor's slots (ADR-0062): the
//! run events that record a wait, the state they rebuild, and what
//! `status` and `stats` show of it. A wait is not a run status: it is the
//! supervisor's mark on a slot, and these events are its record.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

use super::{AskId, AskKind, EventId, RunEvent, TaskId, stats::timestamp_millis};

/// The run entered a wait (decision 5).
pub const RUN_WAITING_STARTED: &str = "run_waiting_started";
/// An ask of the table opened during the wait joined it.
pub const RUN_WAITING_ASK_ADDED: &str = "run_waiting_ask_added";
/// The wait ended; `cause` says how.
pub const RUN_WAITING_ENDED: &str = "run_waiting_ended";
/// The run whose wait ended is back in a slot.
pub const RUN_SLOT_REGAINED: &str = "run_slot_regained";
/// The run met the conditions of a wait while the waits were at their
/// limit, and stayed counted in its slot (decision 7).
pub const RUN_WAITING_DEFERRED: &str = "run_waiting_deferred";

/// The default of `--max-waiting` (decision 7).
pub const DEFAULT_MAX_WAITING: usize = 4;

/// The asks a run in `phase` waits for (decision 1's table): the first
/// session's `worker_question`, `answer_prompt` and `stalled`, and the
/// `stuck_exit` and `answer_prompt` of the `/exit` after a verdict.
pub fn waits_for(phase: WaitPhase, kind: &AskKind) -> bool {
    match phase {
        WaitPhase::Session => matches!(
            kind,
            AskKind::WorkerQuestion | AskKind::AnswerPrompt | AskKind::Stalled
        ),
        WaitPhase::Exit => matches!(kind, AskKind::StuckExit | AskKind::AnswerPrompt),
    }
}

/// The phase a waiting run keeps: the worker's first session or the
/// `/exit` after its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitPhase {
    Session,
    Exit,
}

impl WaitPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Exit => "exit",
        }
    }
}

/// How a wait ended (decision 2, and `wrapper_silent` / `phase_changed`,
/// which this implementation adds: a silent wrapper's session is sent
/// `/exit` from its slot, and a run rebuilt in another phase after a
/// handoff or an adoption no longer waits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitCause {
    Answered,
    DialogCleared,
    SessionMoved,
    SessionExited,
    QueueHold,
    RunEnded,
    LeaseLost,
    WrapperSilent,
    PhaseChanged,
}

impl WaitCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::DialogCleared => "dialog_cleared",
            Self::SessionMoved => "session_moved",
            Self::SessionExited => "session_exited",
            Self::QueueHold => "queue_hold",
            Self::RunEnded => "run_ended",
            Self::LeaseLost => "lease_lost",
            Self::WrapperSilent => "wrapper_silent",
            Self::PhaseChanged => "phase_changed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Answered,
            Self::DialogCleared,
            Self::SessionMoved,
            Self::SessionExited,
            Self::QueueHold,
            Self::RunEnded,
            Self::LeaseLost,
            Self::WrapperSilent,
            Self::PhaseChanged,
        ]
        .into_iter()
        .find(|cause| cause.as_str() == text)
    }

    /// Whether the run goes back to a slot at once, whatever `--parallel`
    /// says (decision 10): a person already moved its session, the queue
    /// is held, or the supervisor has to send to it now. The answer of a
    /// `worker_question` and a session that exited wait for a free slot
    /// (decision 9).
    pub const fn returns_at_once(self) -> bool {
        !matches!(self, Self::Answered | Self::SessionExited)
    }

    /// Whether the run has no slot to go back to (decision 5): no
    /// `run_slot_regained` follows.
    pub const fn leaves_the_run(self) -> bool {
        matches!(self, Self::RunEnded | Self::LeaseLost)
    }
}

/// Where a run stands in its latest wait, by its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitState {
    /// The asks the wait holds: the one that started it and those added.
    pub asks: Vec<(AskId, String)>,
    pub phase: String,
    /// The run's status when the wait started.
    pub status: String,
    /// When the wait started, in milliseconds.
    pub since_ms: i64,
    /// When and how it ended; `None` while it lasts. An ended wait here
    /// waits for its slot (returning).
    pub ended: Option<(i64, WaitCause)>,
}

impl WaitState {
    /// The run's latest wait, from its events (ascending): `None` without
    /// one, or once it is back in a slot or left the run.
    pub fn of(events: &[RunEvent]) -> Option<Self> {
        let start = events.iter().rposition(|e| e.kind == RUN_WAITING_STARTED)?;
        let started = &events[start];
        let mut state = Self {
            asks: ask_of(&started.payload).into_iter().collect(),
            phase: text(&started.payload, "phase"),
            status: text(&started.payload, "status"),
            since_ms: timestamp_millis(&started.created_at).unwrap_or(0),
            ended: None,
        };
        for event in &events[start + 1..] {
            match event.kind.as_str() {
                RUN_WAITING_ASK_ADDED => state.asks.extend(ask_of(&event.payload)),
                RUN_WAITING_ENDED if state.ended.is_none() => {
                    let cause = event.payload["cause"]
                        .as_str()
                        .and_then(WaitCause::parse)
                        .unwrap_or(WaitCause::RunEnded);
                    if cause.leaves_the_run() {
                        return None;
                    }
                    state.ended = Some((timestamp_millis(&event.created_at).unwrap_or(0), cause));
                }
                RUN_SLOT_REGAINED => return None,
                // The supervisor that kept the wait lost the run: another
                // lease began, it gave the lease up, or `recover` took it.
                // An adoption and a handoff keep the wait (they write none
                // of these).
                "lease_acquired" | "lease_released" | "run_recovered" | "runtime_error" => {
                    return None;
                }
                _ => {}
            }
        }
        Some(state)
    }

    /// The asks as `status` shows them.
    pub fn asks_json(&self) -> Value {
        Value::Array(
            self.asks
                .iter()
                .map(|(id, kind)| serde_json::json!({"id": id, "kind": kind}))
                .collect(),
        )
    }
}

/// The runs out of a supervisor's slots, as `--max-waiting` counts them
/// and `status` shows them (ADR-0071 (f2)): those that wait for a person,
/// and those whose wait ended and that wait for a slot to go back to
/// (returning). Both hold a live session, so both count toward the limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitCount {
    pub waiting: usize,
    pub returning: usize,
}

impl WaitCount {
    /// The count of waits, each `true` when its wait ended (returning).
    pub fn of(ended: impl IntoIterator<Item = bool>) -> Self {
        let mut count = Self::default();
        for ended in ended {
            count.add(ended);
        }
        count
    }

    pub fn add(&mut self, ended: bool) {
        if ended {
            self.returning += 1;
        } else {
            self.waiting += 1;
        }
    }

    /// What `--max-waiting` bounds: the waits and the returning runs.
    pub fn count(&self) -> usize {
        self.waiting + self.returning
    }
}

/// The asks of every wait of the run that ended: none of them starts a
/// wait again (decision 2).
pub fn consumed_asks(events: &[RunEvent]) -> Vec<AskId> {
    let mut consumed = Vec::new();
    let mut current = Vec::new();
    for event in events {
        match event.kind.as_str() {
            RUN_WAITING_STARTED => current = ask_of(&event.payload).into_iter().collect(),
            RUN_WAITING_ASK_ADDED => current.extend(ask_of(&event.payload)),
            RUN_WAITING_ENDED => consumed.extend(current.drain(..).map(|(id, _)| id)),
            _ => {}
        }
    }
    consumed
}

/// The asks a `run_waiting_deferred` was recorded for (once per ask).
pub fn deferred_asks(events: &[RunEvent]) -> Vec<AskId> {
    events
        .iter()
        .filter(|e| e.kind == RUN_WAITING_DEFERRED)
        .filter_map(|e| ask_of(&e.payload).map(|(id, _)| id))
        .collect()
}

fn ask_of(payload: &Value) -> Option<(AskId, String)> {
    Some((
        AskId::new(payload["ask_id"].as_i64()?),
        text(payload, "ask_kind"),
    ))
}

fn text(payload: &Value, key: &str) -> String {
    payload[key].as_str().unwrap_or_default().to_owned()
}

/// Seconds, summed and in the middle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Durations {
    pub count: i64,
    pub total_secs: i64,
    pub median_secs: Option<i64>,
    pub max_secs: Option<i64>,
}

impl Durations {
    fn of(mut secs: Vec<i64>) -> Self {
        secs.sort_unstable();
        Self {
            count: i64::try_from(secs.len()).unwrap_or(i64::MAX),
            total_secs: secs.iter().sum(),
            median_secs: (!secs.is_empty()).then(|| secs[secs.len() / 2]),
            max_secs: secs.last().copied(),
        }
    }
}

/// `stats`'s `waiting` (decision 13), over the events of its window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WaitingStats {
    /// `run_waiting_started`, by the kind of the ask that started it.
    pub started: BTreeMap<String, i64>,
    /// `waited_secs` of the waits that ended, by the kind of the ask that
    /// started them: the slot time the waits would have held.
    pub waited: BTreeMap<String, Durations>,
    /// `slot_wait_secs` of the runs back in a slot.
    pub slot_wait: Durations,
    /// Runs that went back past `--parallel` (decision 10).
    pub over_parallel: i64,
    /// `run_waiting_deferred`: how often the limit was reached.
    pub deferred: i64,
}

/// Aggregate the waits with `after < id <= upto` whose task `counts`
/// accepts.
pub fn waiting_stats(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> WaitingStats {
    let mut stats = WaitingStats::default();
    // The kind of the ask that started each run's latest wait.
    let mut started_by: BTreeMap<&str, String> = BTreeMap::new();
    let mut waited: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut slot_wait = Vec::new();
    for event in events {
        let run = event.run_id.as_ref().map_or("", |id| id.as_str());
        let inside = event.id > after && event.id <= upto && counts(event.task_id);
        match event.kind.as_str() {
            RUN_WAITING_STARTED => {
                let kind = text(&event.payload, "ask_kind");
                if inside {
                    *stats.started.entry(kind.clone()).or_default() += 1;
                }
                started_by.insert(run, kind);
            }
            RUN_WAITING_ENDED if inside => {
                let kind = started_by.get(run).cloned().unwrap_or_default();
                waited
                    .entry(kind)
                    .or_default()
                    .push(event.payload["waited_secs"].as_i64().unwrap_or(0));
            }
            RUN_SLOT_REGAINED if inside => {
                slot_wait.push(event.payload["slot_wait_secs"].as_i64().unwrap_or(0));
                if event.payload["over_parallel"] == true {
                    stats.over_parallel += 1;
                }
            }
            RUN_WAITING_DEFERRED if inside => stats.deferred += 1,
            _ => {}
        }
    }
    stats.waited = waited
        .into_iter()
        .map(|(kind, secs)| (kind, Durations::of(secs)))
        .collect();
    stats.slot_wait = Durations::of(slot_wait);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use serde_json::json;

    fn event(id: i64, run: &str, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: at.into(),
        }
    }

    fn started(id: i64, ask: i64, kind: &str) -> RunEvent {
        event(
            id,
            "r1",
            RUN_WAITING_STARTED,
            json!({"ask_id": ask, "ask_kind": kind, "phase": "exit", "status": "awaiting_integration"}),
            "2026-09-26T04:10:00.000Z",
        )
    }

    #[test]
    fn the_limit_counts_the_waits_and_the_returning_runs() {
        let count = WaitCount::of([false, true, false]);
        assert_eq!(
            count,
            WaitCount {
                waiting: 2,
                returning: 1
            }
        );
        assert_eq!(count.count(), 3);
        assert_eq!(WaitCount::default().count(), 0);
    }

    #[test]
    fn the_state_follows_the_latest_wait_until_its_run_is_back_in_a_slot() {
        assert_eq!(WaitState::of(&[]), None);
        let mut events = vec![started(1, 7, "stuck_exit")];
        let state = WaitState::of(&events).unwrap();
        assert_eq!(state.asks, [(AskId::new(7), "stuck_exit".to_owned())]);
        assert_eq!(state.phase, "exit");
        assert_eq!(state.status, "awaiting_integration");
        assert_eq!(state.ended, None);
        assert_eq!(state.asks_json(), json!([{"id": 7, "kind": "stuck_exit"}]));
        events.push(event(
            2,
            "r1",
            RUN_WAITING_ASK_ADDED,
            json!({"ask_id": 8, "ask_kind": "answer_prompt"}),
            "2026-09-26T04:11:00.000Z",
        ));
        events.push(event(
            3,
            "r1",
            RUN_WAITING_ENDED,
            json!({"ask_id": 7, "ask_kind": "stuck_exit", "cause": "session_exited", "waited_secs": 60}),
            "2026-09-26T04:12:00.000Z",
        ));
        let state = WaitState::of(&events).unwrap();
        assert_eq!(state.asks.len(), 2);
        assert_eq!(
            state.ended.map(|(_, cause)| cause),
            Some(WaitCause::SessionExited)
        );
        assert_eq!(consumed_asks(&events), [AskId::new(7), AskId::new(8)]);
        events.push(event(
            4,
            "r1",
            RUN_SLOT_REGAINED,
            json!({"slot_wait_secs": 5, "over_parallel": false}),
            "2026-09-26T04:12:05.000Z",
        ));
        assert_eq!(WaitState::of(&events), None);
        // A wait that left the run has nothing to go back to.
        let left = [
            started(1, 7, "worker_question"),
            event(
                2,
                "r1",
                RUN_WAITING_ENDED,
                json!({"cause": "lease_lost"}),
                "2026-09-26T04:12:00.000Z",
            ),
        ];
        assert_eq!(WaitState::of(&left), None);
        // A run `recover` took, or that began another lease, waits no more.
        for kind in [
            "run_recovered",
            "lease_acquired",
            "lease_released",
            "runtime_error",
        ] {
            let lost = [
                started(1, 7, "worker_question"),
                event(2, "r1", kind, json!({}), "2026-09-26T04:12:00.000Z"),
            ];
            assert_eq!(WaitState::of(&lost), None, "{kind}");
        }
    }

    #[test]
    fn the_table_and_the_causes() {
        assert!(waits_for(WaitPhase::Session, &AskKind::WorkerQuestion));
        assert!(waits_for(WaitPhase::Session, &AskKind::Stalled));
        assert!(!waits_for(WaitPhase::Session, &AskKind::StuckExit));
        assert!(waits_for(WaitPhase::Exit, &AskKind::StuckExit));
        assert!(waits_for(WaitPhase::Exit, &AskKind::AnswerPrompt));
        assert!(!waits_for(WaitPhase::Exit, &AskKind::WorkerQuestion));
        assert_eq!(WaitPhase::Exit.as_str(), "exit");
        for cause in [
            WaitCause::Answered,
            WaitCause::DialogCleared,
            WaitCause::SessionMoved,
            WaitCause::SessionExited,
            WaitCause::QueueHold,
            WaitCause::RunEnded,
            WaitCause::LeaseLost,
            WaitCause::WrapperSilent,
            WaitCause::PhaseChanged,
        ] {
            assert_eq!(WaitCause::parse(cause.as_str()), Some(cause));
        }
        assert_eq!(WaitCause::parse("other"), None);
        assert!(!WaitCause::Answered.returns_at_once());
        assert!(!WaitCause::SessionExited.returns_at_once());
        assert!(WaitCause::DialogCleared.returns_at_once());
        assert!(WaitCause::LeaseLost.leaves_the_run());
        assert!(!WaitCause::QueueHold.leaves_the_run());
    }

    #[test]
    fn stats_count_the_waits_and_their_times_in_the_window() {
        let events = vec![
            started(1, 7, "stuck_exit"),
            event(
                2,
                "r1",
                RUN_WAITING_ENDED,
                json!({"cause": "session_exited", "waited_secs": 100}),
                "2026-09-26T04:12:00.000Z",
            ),
            event(
                3,
                "r1",
                RUN_SLOT_REGAINED,
                json!({"slot_wait_secs": 5, "over_parallel": false}),
                "2026-09-26T04:12:05.000Z",
            ),
            event(
                4,
                "r2",
                RUN_WAITING_STARTED,
                json!({"ask_id": 9, "ask_kind": "answer_prompt"}),
                "2026-09-26T04:13:00.000Z",
            ),
            event(
                5,
                "r2",
                RUN_WAITING_ENDED,
                json!({"cause": "dialog_cleared", "waited_secs": 30}),
                "2026-09-26T04:13:30.000Z",
            ),
            event(
                6,
                "r2",
                RUN_SLOT_REGAINED,
                json!({"slot_wait_secs": 0, "over_parallel": true}),
                "2026-09-26T04:13:30.000Z",
            ),
            event(
                7,
                "r3",
                RUN_WAITING_DEFERRED,
                json!({"ask_id": 10, "ask_kind": "worker_question"}),
                "2026-09-26T04:14:00.000Z",
            ),
        ];
        let all = waiting_stats(&events, EventId::new(0), EventId::new(7), |_| true);
        assert_eq!(all.started["stuck_exit"], 1);
        assert_eq!(all.started["answer_prompt"], 1);
        assert_eq!(all.waited["stuck_exit"].total_secs, 100);
        assert_eq!(all.waited["answer_prompt"].median_secs, Some(30));
        assert_eq!(all.slot_wait.max_secs, Some(5));
        assert_eq!(all.slot_wait.count, 2);
        assert_eq!(all.over_parallel, 1);
        assert_eq!(all.deferred, 1);
        assert_eq!(deferred_asks(&events), [AskId::new(10)]);
        // A wait started before the window counts its end by its kind.
        let window = waiting_stats(&events, EventId::new(1), EventId::new(3), |_| true);
        assert!(window.started.is_empty());
        assert_eq!(window.waited["stuck_exit"].count, 1);
        let none = waiting_stats(&events, EventId::new(0), EventId::new(7), |_| false);
        assert_eq!(none, WaitingStats::default());
    }
}
