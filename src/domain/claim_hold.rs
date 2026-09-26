//! Holding new claims (task 327): the supervisor claims no new run while a
//! reason holds, and leaves the runs in flight alone. One judgement names
//! the first reason that holds; the supervisor records `claim_held` when
//! the hold starts (or its reason changes) and `claim_resumed` when it
//! ends, as queue events, so `status` shows a hold in progress and `stats`
//! the time spent held apart from the `idle_slots` alert. The reasons are
//! the free disk space below what a run needs (task 377) and the 1-minute
//! load average above `supervise --max-load`; later reasons join
//! [`HoldReason`] and [`ClaimHold::judge`].
//!
//! Landings are held the same way (task 377): the supervisor starts no
//! landing's verification while the free disk space is below what it
//! needs, and records `landing_held` / `landing_resumed` with the same
//! payloads, so `status` and `stats` show them in the same shape
//! ([`LANDINGS`]).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::{EventId, RunEvent, TaskId, stats::timestamp_millis};

/// Recorded when the supervisor starts holding its claims, or holds them
/// for another reason (`reason`, `value`, `threshold`, `message`,
/// `supervisor`).
pub const CLAIM_HELD: &str = "claim_held";
/// Recorded when the hold ends and claims resume (`reason` of the hold
/// that ended, `supervisor`).
pub const CLAIM_RESUMED: &str = "claim_resumed";
/// The two kinds, for reading the latest of them.
pub const CLAIM_HOLD_KINDS: [&str; 2] = [CLAIM_HELD, CLAIM_RESUMED];
/// Recorded when the supervisor starts holding the landings' verification
/// (task 377), with the payload of `claim_held`.
pub const LANDING_HELD: &str = "landing_held";
/// Recorded when landings resume, with the payload of `claim_resumed`.
pub const LANDING_RESUMED: &str = "landing_resumed";
/// The two kinds, for reading the latest of them.
pub const LANDING_HOLD_KINDS: [&str; 2] = [LANDING_HELD, LANDING_RESUMED];

/// What a hold holds: new claims or the landings' verification, each
/// recorded by its pair of queue events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldKinds {
    pub held: &'static str,
    pub resumed: &'static str,
    /// The hold is of the supervisor that recorded it, not of the host:
    /// another live supervisor does not end it (a landing waits in one
    /// supervisor's slot).
    pub own: bool,
}

impl HoldKinds {
    /// Both kinds, for reading the latest of them.
    pub const fn kinds(self) -> [&'static str; 2] {
        [self.held, self.resumed]
    }
}

/// The holds on new claims.
pub const CLAIMS: HoldKinds = HoldKinds {
    held: CLAIM_HELD,
    resumed: CLAIM_RESUMED,
    own: false,
};
/// The holds on the landings' verification.
pub const LANDINGS: HoldKinds = HoldKinds {
    held: LANDING_HELD,
    resumed: LANDING_RESUMED,
    own: true,
};

/// The default of `supervise --max-load`: the 1-minute load average above
/// which no new run is claimed. On the 8-core host this queue runs on,
/// the `backend_call_failed` events by load band (`stats --full` of
/// 2026-09-26) were 1 at 0-4, 1 at 8-16, 56 at 16-32, 257 at 32-64 and
/// 116 above 64: cmux's timeouts start past twice the cores.
pub const DEFAULT_MAX_LOAD: f64 = 16.0;

/// Why new claims are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// The free disk space of the queue's directory is below what a run
    /// needs (task 377, [`super::disk`]).
    DiskSpace,
    /// The 1-minute load average is above `--max-load`.
    LoadAverage,
}

impl HoldReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DiskSpace => "disk_space",
            Self::LoadAverage => "load_average",
        }
    }
}

/// What the supervisor judges before it claims.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HoldInputs {
    /// The 1-minute load average; `None` when it could not be read.
    pub load_average: Option<f64>,
    /// `--max-load`; `None` holds for no load.
    pub max_load: Option<f64>,
    /// The free bytes of the queue's directory; `None` when they could not
    /// be read.
    pub free_bytes: Option<u64>,
    /// The free bytes a new run (or a landing) needs; `None` holds for no
    /// disk space.
    pub needed_bytes: Option<u64>,
}

/// A hold on new claims: its reason, and the value that crossed the
/// threshold.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimHold {
    pub reason: HoldReason,
    pub value: f64,
    pub threshold: f64,
}

impl ClaimHold {
    /// The first reason that holds, or `None` when claims may go on: the
    /// free disk space below the bytes needed (the disk fills whatever the
    /// load), then the load above `--max-load`. A value that could not be
    /// read holds nothing.
    pub fn judge(inputs: &HoldInputs) -> Option<Self> {
        if let (Some(free), Some(needed)) = (inputs.free_bytes, inputs.needed_bytes)
            && free < needed
        {
            return Some(Self {
                reason: HoldReason::DiskSpace,
                value: free as f64,
                threshold: needed as f64,
            });
        }
        match (inputs.load_average, inputs.max_load) {
            (Some(value), Some(threshold)) if value > threshold => Some(Self {
                reason: HoldReason::LoadAverage,
                value,
                threshold,
            }),
            _ => None,
        }
    }

    /// Why nothing is claimed, for the log and the event.
    pub fn message(&self) -> String {
        match self.reason {
            HoldReason::DiskSpace => format!(
                "the free disk space {} of the queue's directory is below the {} a new run needs: no new run is claimed until the ended runs' worktrees are cleaned or a person frees the disk; the runs in flight go on",
                super::disk::gib(self.value),
                super::disk::gib(self.threshold)
            ),
            HoldReason::LoadAverage => format!(
                "the 1-minute load average {:.2} is above --max-load {:.2}: no new run is claimed until it falls back; the runs in flight go on",
                self.value, self.threshold
            ),
        }
    }

    /// Why no landing starts its verification, for the log and the event.
    pub fn landing_message(&self) -> String {
        format!(
            "the free disk space {} of the queue's directory is below the {} a landing's verification needs: no run lands until the ended runs' worktrees are cleaned or a person frees the disk; the runs stay awaiting integration",
            super::disk::gib(self.value),
            super::disk::gib(self.threshold)
        )
    }

    /// The message of a hold of `kinds`.
    pub fn message_for(&self, kinds: HoldKinds) -> String {
        if kinds == LANDINGS {
            self.landing_message()
        } else {
            self.message()
        }
    }
}

/// The event the supervisor `token` records when its judgement (`hold`)
/// differs from the latest [`CLAIM_HOLD_KINDS`] event on the queue
/// (`last`): `claim_held` when it holds and the last one is not a hold in
/// place for the same reason, `claim_resumed` when it holds nothing and the
/// last one is a hold, else none. A hold is in place while the supervisor
/// that recorded it runs (`live` of its token; this supervisor's own always
/// is): the load is the host's, so two supervisors on one queue do not
/// record it in turns, while one started after the holder stopped or died
/// records the hold anew under its own token, which `status` and `stats`
/// show; a hold nobody runs any more ends with the next `claim_resumed`.
pub fn transition(
    hold: Option<&ClaimHold>,
    last: Option<&RunEvent>,
    token: &str,
    live: impl Fn(&str) -> bool,
) -> Option<(&'static str, Value)> {
    transition_of(CLAIMS, hold, last, token, live)
}

/// [`transition`] for the holds of `kinds`: `last` is the latest of its
/// two events.
pub fn transition_of(
    kinds: HoldKinds,
    hold: Option<&ClaimHold>,
    last: Option<&RunEvent>,
    token: &str,
    live: impl Fn(&str) -> bool,
) -> Option<(&'static str, Value)> {
    let held = last.filter(|event| event.kind == kinds.held);
    let held_reason = held
        .filter(|event| {
            text(event, "supervisor").is_some_and(|holder| holder == token || live(holder))
        })
        .and_then(|event| text(event, "reason"));
    match hold {
        Some(hold) if held_reason != Some(hold.reason.as_str()) => Some((
            kinds.held,
            json!({
                "reason": hold.reason,
                "value": hold.value,
                "threshold": hold.threshold,
                "message": hold.message_for(kinds),
                "supervisor": token,
            }),
        )),
        // Another live supervisor's own hold is its to end.
        None if kinds.own
            && held
                .and_then(|event| text(event, "supervisor"))
                .is_some_and(|holder| holder != token && live(holder)) =>
        {
            None
        }
        None if held.is_some() => Some((
            kinds.resumed,
            json!({"reason": held.and_then(|event| text(event, "reason")), "supervisor": token}),
        )),
        _ => None,
    }
}

/// One reason's holds in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReasonHolds {
    pub count: i64,
    pub secs: i64,
}

/// The hold in progress: the latest `claim_held` no `claim_resumed` or
/// stop of its supervisor followed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenHold {
    pub reason: String,
    pub supervisor: Option<String>,
    pub since: String,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
}

/// The holds on new claims (`stats`' `claim_holds`): those that started in
/// the window, by reason, with the seconds each lasted (one still open
/// lasts to the window's end), and the hold in progress now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClaimHolds {
    pub count: i64,
    pub secs: i64,
    pub by_reason: BTreeMap<String, ReasonHolds>,
    pub held: Option<OpenHold>,
}

/// Aggregate the holds of `events` (ascending id) that started with
/// `after < id <= upto`; `end_ms` ends one still open. A hold ends at the
/// next `claim_held` or `claim_resumed`, or at the `supervisor_stopped` of
/// its supervisor. Holds belong to no task, so they count only when
/// `counts` accepts no task (no `--goal`); `held` is the queue's now
/// either way.
pub fn claim_holds(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimHolds {
    holds_of(CLAIMS, events, after, upto, end_ms, counts)
}

/// [`claim_holds`] for the holds of `kinds` (`stats`' `landing_holds` for
/// [`LANDINGS`]).
pub fn holds_of(
    kinds: HoldKinds,
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimHolds {
    let mut stats = ClaimHolds::default();
    let mut open: Option<(&RunEvent, i64)> = None;
    let close = |stats: &mut ClaimHolds, (event, start): (&RunEvent, i64), end: i64| {
        if event.id <= after || event.id > upto || !counts(event.task_id) {
            return;
        }
        // A hold that ends after the window counts to the window's end.
        let secs = (end.min(end_ms) - start).max(0) / 1000;
        let reason = text(event, "reason").unwrap_or("unknown").to_owned();
        let entry = stats.by_reason.entry(reason).or_default();
        entry.count += 1;
        entry.secs += secs;
        stats.count += 1;
        stats.secs += secs;
    };
    for event in events {
        let at = || timestamp_millis(&event.created_at).unwrap_or(end_ms);
        match event.kind.as_str() {
            kind if kind == kinds.held => {
                if let Some(held) = open.take() {
                    close(&mut stats, held, at());
                }
                open = Some((event, at()));
            }
            kind if kind == kinds.resumed => {
                if let Some(held) = open.take() {
                    close(&mut stats, held, at());
                }
            }
            "supervisor_stopped"
                if open.is_some_and(|(held, _)| {
                    text(held, "supervisor") == text(event, "supervisor")
                }) =>
            {
                if let Some(held) = open.take() {
                    close(&mut stats, held, at());
                }
            }
            _ => {}
        }
    }
    if let Some(held) = open {
        let number = |key: &str| held.0.payload.get(key).and_then(Value::as_f64);
        stats.held = Some(OpenHold {
            reason: text(held.0, "reason").unwrap_or("unknown").to_owned(),
            supervisor: text(held.0, "supervisor").map(str::to_owned),
            since: held.0.created_at.clone(),
            value: number("value"),
            threshold: number("threshold"),
        });
        close(&mut stats, held, end_ms);
    }
    stats
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-26T01:{at}.000Z"),
        }
    }

    fn load(value: Option<f64>, max: Option<f64>) -> Option<ClaimHold> {
        ClaimHold::judge(&HoldInputs {
            load_average: value,
            max_load: max,
            ..HoldInputs::default()
        })
    }

    fn disk(free: Option<u64>, needed: Option<u64>, load: Option<f64>) -> Option<ClaimHold> {
        ClaimHold::judge(&HoldInputs {
            load_average: load,
            max_load: Some(16.0),
            free_bytes: free,
            needed_bytes: needed,
        })
    }

    #[test]
    fn the_disk_holds_below_the_bytes_needed_before_the_load() {
        const GIB: u64 = 1 << 30;
        let hold = disk(Some(GIB), Some(4 * GIB), Some(40.0)).unwrap();
        assert_eq!(hold.reason, HoldReason::DiskSpace);
        assert_eq!((hold.value, hold.threshold), (GIB as f64, 4.0 * GIB as f64));
        assert!(hold.message().contains("1.0 GiB"), "{}", hold.message());
        assert!(hold.message().contains("4.0 GiB a new run needs"));
        assert!(hold.landing_message().contains("no run lands"));
        assert_eq!(hold.message_for(LANDINGS), hold.landing_message());
        assert_eq!(hold.message_for(CLAIMS), hold.message());
        assert_eq!(HoldReason::DiskSpace.as_str(), "disk_space");
        // Enough space, or nothing to judge by: the load decides.
        assert_eq!(
            disk(Some(4 * GIB), Some(4 * GIB), Some(40.0))
                .unwrap()
                .reason,
            HoldReason::LoadAverage
        );
        assert_eq!(disk(None, Some(4 * GIB), Some(1.0)), None);
        assert_eq!(disk(Some(GIB), None, Some(1.0)), None);
    }

    #[test]
    fn landings_are_held_and_summed_by_their_own_kinds() {
        let hold = disk(Some(1), Some(2), None).unwrap();
        let (kind, payload) = transition_of(LANDINGS, Some(&hold), None, "s", |_| true).unwrap();
        assert_eq!(kind, LANDING_HELD);
        assert_eq!(payload["reason"], json!("disk_space"));
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("no run lands")
        );
        // A claim hold is no landing hold.
        let claim = event(1, CLAIM_HELD, payload.clone(), "00:00");
        assert_eq!(
            transition_of(LANDINGS, None, Some(&claim), "s", |_| true),
            None
        );
        let held = event(2, LANDING_HELD, payload, "00:00");
        assert_eq!(
            transition_of(LANDINGS, Some(&hold), Some(&held), "s", |_| true),
            None
        );
        let (kind, _) = transition_of(LANDINGS, None, Some(&held), "s", |_| true).unwrap();
        assert_eq!(kind, LANDING_RESUMED);
        // Another live supervisor with no landing waiting leaves it; one
        // that stopped does not hold it any more.
        assert_eq!(
            transition_of(LANDINGS, None, Some(&held), "t", |_| true),
            None
        );
        let (kind, _) = transition_of(LANDINGS, None, Some(&held), "t", |_| false).unwrap();
        assert_eq!(kind, LANDING_RESUMED);
        let events = [claim, held, event(3, LANDING_RESUMED, json!({}), "00:30")];
        let end = timestamp_millis("2026-09-26T01:03:00.000Z").unwrap();
        let landings = holds_of(
            LANDINGS,
            &events,
            EventId::new(0),
            EventId::new(3),
            end,
            |_| true,
        );
        assert_eq!((landings.count, landings.secs), (1, 30));
        assert_eq!(landings.by_reason["disk_space"].count, 1);
        assert_eq!(landings.held, None);
        assert_eq!(LANDINGS.kinds(), LANDING_HOLD_KINDS);
        assert_eq!(CLAIMS.kinds(), CLAIM_HOLD_KINDS);
    }

    #[test]
    fn the_load_holds_above_the_threshold_only() {
        let hold = load(Some(20.5), Some(16.0)).unwrap();
        assert_eq!(hold.reason, HoldReason::LoadAverage);
        assert_eq!((hold.value, hold.threshold), (20.5, 16.0));
        assert!(hold.message().contains("20.50"), "{}", hold.message());
        assert!(hold.message().contains("--max-load 16.00"));
        assert_eq!(load(Some(16.0), Some(16.0)), None);
        assert_eq!(load(Some(3.0), Some(16.0)), None);
        assert_eq!(load(None, Some(16.0)), None);
        assert_eq!(load(Some(99.0), None), None);
        assert_eq!(HoldReason::LoadAverage.as_str(), "load_average");
    }

    #[test]
    fn a_hold_is_recorded_when_it_starts_and_when_it_ends() {
        let hold = load(Some(20.0), Some(16.0)).unwrap();
        let (kind, payload) = transition(Some(&hold), None, "s", |_| true).unwrap();
        assert_eq!(kind, CLAIM_HELD);
        assert_eq!(payload["reason"], json!("load_average"));
        assert_eq!(payload["value"], json!(20.0));
        assert_eq!(payload["threshold"], json!(16.0));
        assert_eq!(payload["supervisor"], json!("s"));
        let held = event(1, CLAIM_HELD, payload, "00:00");
        // Held already: nothing more, however high the load goes.
        let higher = load(Some(40.0), Some(16.0)).unwrap();
        assert_eq!(transition(Some(&higher), Some(&held), "s", |_| true), None);
        // Another supervisor's hold is the host's too: held already, and
        // it ends that hold when the load falls.
        assert_eq!(transition(Some(&hold), Some(&held), "t", |_| true), None);
        let (kind, payload) = transition(None, Some(&held), "t", |_| true).unwrap();
        assert_eq!(kind, CLAIM_RESUMED);
        assert_eq!(payload["supervisor"], json!("t"));
        // A holder that stopped or died holds nothing: the supervisor
        // started after it records the hold anew under its own token, and
        // ends it when the load falls.
        let stopped = |holder: &str| holder != "s";
        let (kind, payload) = transition(Some(&hold), Some(&held), "t", stopped).unwrap();
        assert_eq!(kind, CLAIM_HELD);
        assert_eq!(payload["supervisor"], json!("t"));
        assert_eq!(transition(Some(&hold), Some(&held), "s", |_| false), None);
        let (kind, payload) = transition(None, Some(&held), "t", stopped).unwrap();
        assert_eq!(kind, CLAIM_RESUMED);
        assert_eq!(payload["reason"], json!("load_average"));
        // The load fell: resumed, naming the hold's reason.
        let (kind, payload) = transition(None, Some(&held), "s", |_| true).unwrap();
        assert_eq!(kind, CLAIM_RESUMED);
        assert_eq!(
            payload,
            json!({"reason": "load_average", "supervisor": "s"})
        );
        let resumed = event(2, CLAIM_RESUMED, payload, "00:10");
        assert_eq!(transition(None, Some(&resumed), "s", |_| true), None);
        assert_eq!(transition(None, None, "s", |_| true), None);
        assert_eq!(
            transition(Some(&hold), Some(&resumed), "s", |_| true)
                .unwrap()
                .0,
            CLAIM_HELD
        );
    }

    #[test]
    fn holds_are_summed_by_reason_and_the_open_one_is_the_hold_now() {
        let held = |id, at, who: &str| {
            event(
                id,
                CLAIM_HELD,
                json!({"reason": "load_average", "value": 20.0, "threshold": 16.0, "supervisor": who}),
                at,
            )
        };
        let events = [
            held(1, "00:00", "s"),
            event(2, "run_claimed", json!({}), "00:05"),
            event(3, CLAIM_RESUMED, json!({"reason": "load_average"}), "00:30"),
            held(4, "01:00", "s"),
            // Another supervisor's stop ends nothing; its own does.
            event(5, "supervisor_stopped", json!({"supervisor": "t"}), "01:10"),
            event(6, "supervisor_stopped", json!({"supervisor": "s"}), "01:20"),
            held(7, "02:00", "u"),
            event(8, CLAIM_HELD, json!({"supervisor": "u"}), "02:05"),
        ];
        let end = timestamp_millis("2026-09-26T01:03:00.000Z").unwrap();
        let all = claim_holds(&events, EventId::new(0), EventId::new(8), end, |_| true);
        assert_eq!(all.count, 4);
        assert_eq!(all.secs, 30 + 20 + 5 + 55);
        assert_eq!(
            all.by_reason["load_average"],
            ReasonHolds { count: 3, secs: 55 }
        );
        assert_eq!(all.by_reason["unknown"], ReasonHolds { count: 1, secs: 55 });
        let now = all.held.unwrap();
        assert_eq!(now.reason, "unknown");
        assert_eq!(now.supervisor.as_deref(), Some("u"));
        assert_eq!(now.since, "2026-09-26T01:02:05.000Z");

        // Only the holds that started in the window; the hold now either way.
        let later = claim_holds(&events, EventId::new(3), EventId::new(6), end, |_| true);
        assert_eq!(later.count, 1);
        assert_eq!(later.secs, 20);
        assert!(later.held.is_some());
        // A hold that ends after the window counts to the window's end.
        let upto = timestamp_millis("2026-09-26T01:00:10.000Z").unwrap();
        let cut = claim_holds(&events, EventId::new(0), EventId::new(2), upto, |_| true);
        assert_eq!((cut.count, cut.secs), (1, 10));
        let goal = claim_holds(&events, EventId::new(0), EventId::new(8), end, |task| {
            task.is_some()
        });
        assert_eq!(goal.count, 0);
        assert!(goal.held.is_some());
        let resumed = claim_holds(&events[..3], EventId::new(0), EventId::new(3), end, |_| {
            true
        });
        assert_eq!(resumed.held, None);
    }
}
