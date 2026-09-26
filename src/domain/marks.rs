//! Change marks (ADR-0051 decisions 10–13): when something that moves the
//! KPIs changed, for the KPI's periods to split at. Two sorts, never the same
//! change twice:
//!
//! - **recorded marks** are run events on the queue itself (no task, goal or
//!   run): the supervisor's start, handoff and stop, a change of the main
//!   checkout's `[run.env]` (its normalized hash), and a person's or a
//!   planner's `dagq mark` and its retraction;
//! - **derived marks** write nothing: they are read off the attributes
//!   `run_claimed` carries (task 197), where a claim's value differs from
//!   the previous claim's (the build identifier, Claude Code's version,
//!   `parallel`, and the host's Rust toolchain).
//!
//! Marks only split periods; they change no run and raise no attention.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    EventId, RunEvent,
    stats::{Cursor, timestamp_millis},
};

/// A supervisor started, or took over its registration by a handoff
/// (ADR-0045 decision 10): its build identifier, `parallel`, `mode` and
/// whether it updates itself.
pub const SUPERVISOR_STARTED: &str = "supervisor_started";
/// A supervisor stopped and removed its registration. One that went stale
/// records none: its interval ends at its last heartbeat or the next start.
pub const SUPERVISOR_STOPPED: &str = "supervisor_stopped";
/// The normalized `[run.env]` of the main checkout's `dagq.toml` hashes
/// differently from the last one recorded.
pub const RUN_ENV_CHANGED: &str = "run_env_changed";
/// A mark a person or a planner recorded with `dagq mark`.
pub const MARK_RECORDED: &str = "mark_recorded";
/// A retraction of a recorded mark (`dagq mark --retract`); the retracted
/// mark stays, and the KPIs do not split at it.
pub const MARK_RETRACTED: &str = "mark_retracted";
/// Every kind of recorded mark.
pub const RECORDED_KINDS: [&str; 5] = [
    SUPERVISOR_STARTED,
    SUPERVISOR_STOPPED,
    RUN_ENV_CHANGED,
    MARK_RECORDED,
    MARK_RETRACTED,
];
/// The marks `dagq mark --retract` retracts: the ones that stand for a
/// change a person may find was none. The supervisor's start and stop are
/// what happened to the process, and a retraction retracts nothing.
pub const RETRACTABLE_KINDS: [&str; 2] = [MARK_RECORDED, RUN_ENV_CHANGED];
/// The prefix of a derived mark's kind: `derived:<attribute>`.
pub const DERIVED_PREFIX: &str = "derived:";
/// The longest label `dagq mark` takes, in characters.
pub const MAX_LABEL_CHARS: usize = 120;

const RUN_CLAIMED: &str = "run_claimed";

/// The normalized `[run.env]`: the hash of its `KEY=VALUE` lines in key
/// order, and each key's own hash so the next change can name the keys it
/// changed without the values going into the queue (ADR-0051 decision 11).
/// Both are keyed with the queue's secret salt, kept outside the events:
/// a plain hash of a short value (`CARGO_BUILD_JOBS=4`) is found by
/// guessing, and the mark would spread the value after all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnvDigest {
    pub hash: String,
    pub keys: BTreeMap<String, String>,
}

/// [`RunEnvDigest`] of `[run.env]` as written (values unexpanded), keyed
/// with `salt`.
pub fn run_env_digest(table: &[(String, String)], salt: &str) -> RunEnvDigest {
    let entries: BTreeMap<&str, &str> = table
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let normalized: String = entries
        .iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect();
    RunEnvDigest {
        hash: keyed_hash(salt, &normalized),
        keys: entries
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    keyed_hash(salt, &format!("{key}={value}")),
                )
            })
            .collect(),
    }
}

fn keyed_hash(salt: &str, text: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(salt.as_bytes());
    digest.update([0]);
    digest.update(text.as_bytes());
    let mut hex = format!("{:x}", digest.finalize());
    hex.truncate(16);
    hex
}

impl RunEnvDigest {
    /// The payload of the `run_env_changed` to record after `last` (the
    /// payload of the latest one), or `None` when the hash is the same. With
    /// none recorded yet, an empty table records nothing (a queue without
    /// `[run.env]` gets no mark until it has one), any other the first mark.
    pub fn transition(&self, last: Option<&Value>) -> Option<Value> {
        let previous_keys: BTreeMap<String, String> = last
            .and_then(|payload| payload.get("keys"))
            .and_then(Value::as_object)
            .map(|keys| {
                keys.iter()
                    .filter_map(|(key, hash)| Some((key.clone(), hash.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let previous_hash = last
            .and_then(|payload| payload.get("hash"))
            .and_then(Value::as_str);
        match previous_hash {
            Some(hash) if hash == self.hash => return None,
            None if self.keys.is_empty() => return None,
            _ => {}
        }
        let changed: BTreeSet<&String> = self
            .keys
            .keys()
            .chain(previous_keys.keys())
            .filter(|key| self.keys.get(*key) != previous_keys.get(*key))
            .collect();
        Some(json!({
            "hash": self.hash,
            "previous_hash": previous_hash,
            "keys": self.keys,
            "changed": changed,
        }))
    }
}

/// The payload of a person's or a planner's mark: its label, note, who
/// recorded it and, for a mark of an earlier time (`--at`), when it took
/// effect.
pub fn mark_payload(
    label: &str,
    note: Option<&str>,
    at: Option<String>,
    by: &str,
) -> Result<Value, String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("a mark needs a label".into());
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "a mark's label is at most {MAX_LABEL_CHARS} characters; put the rest in --note"
        ));
    }
    Ok(json!({
        "label": label,
        "note": note.map(str::trim).filter(|note| !note.is_empty()),
        "at": at,
        "by": by,
    }))
}

/// The payload of the retraction of `target`, if it may be retracted: a
/// retractable mark ([`RETRACTABLE_KINDS`]) not retracted yet. `events`
/// holds at least the recorded marks.
pub fn retraction_payload(events: &[RunEvent], target: EventId, by: &str) -> Result<Value, String> {
    let mark = events
        .iter()
        .find(|event| event.id == target && is_queue_event(event))
        .filter(|event| RETRACTABLE_KINDS.contains(&event.kind.as_str()))
        .ok_or_else(|| {
            format!("event {target} is not a mark that can be retracted (dagq mark, or a [run.env] change)")
        })?;
    if let Some(retraction) = retractions(events).get(&target) {
        return Err(format!(
            "mark {target} was already retracted by {retraction}"
        ));
    }
    Ok(json!({
        "mark": target,
        "kind": mark.kind,
        "label": recorded_label(mark),
        "by": by,
    }))
}

/// One change mark, recorded or derived.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mark {
    /// The event of a recorded mark; `None` for a derived one.
    pub id: Option<EventId>,
    /// The event kind, or `derived:<attribute>`.
    pub kind: String,
    /// When the change took effect: the event's time, the `--at` of a
    /// person's mark, or the claim a derived mark was read off.
    pub at: String,
    /// When the event was written (the claim's, for a derived mark).
    pub recorded_at: String,
    pub label: String,
    /// The retraction of this mark; the KPIs do not split at it.
    pub retracted_by: Option<EventId>,
    /// The event's payload, or for a derived mark the attribute, its value
    /// before and after, and the claim.
    pub detail: Value,
}

/// Every mark in `events` (all of the queue's, ascending id) whose time is
/// after `since` and at or before `until`, oldest first.
pub fn marks(events: &[RunEvent], since: Option<Cursor>, until: Option<Cursor>) -> Vec<Mark> {
    let retracted = retractions(events);
    let mut marks: Vec<Mark> = events
        .iter()
        .filter(|event| is_queue_event(event) && RECORDED_KINDS.contains(&event.kind.as_str()))
        .map(|event| Mark {
            id: Some(event.id),
            kind: event.kind.clone(),
            at: event
                .payload
                .get("at")
                .and_then(Value::as_str)
                .filter(|_| event.kind == MARK_RECORDED)
                .unwrap_or(&event.created_at)
                .to_owned(),
            recorded_at: event.created_at.clone(),
            label: recorded_label(event),
            retracted_by: retracted.get(&event.id).copied(),
            detail: event.payload.clone(),
        })
        .collect();
    marks.extend(derived_marks(events));
    let bound = |cursor: Option<Cursor>| cursor.map(|cursor| cursor_millis(cursor, events));
    let (since, until) = (bound(since), bound(until));
    marks.retain(|mark| {
        let at = timestamp_millis(&mark.at).unwrap_or(i64::MIN);
        since.is_none_or(|since| at > since) && until.is_none_or(|until| at <= until)
    });
    marks.sort_by_key(|mark| {
        (
            timestamp_millis(&mark.at).unwrap_or(i64::MIN),
            mark.id.map(EventId::as_i64),
        )
    });
    marks
}

/// The time a cursor stands for: its own, or the time of the latest event
/// at or before its id (the epoch when none is).
fn cursor_millis(cursor: Cursor, events: &[RunEvent]) -> i64 {
    match cursor {
        Cursor::Time(millis) => millis,
        Cursor::Event(id) => events
            .iter()
            .filter(|event| event.id <= id)
            .filter_map(|event| timestamp_millis(&event.created_at))
            .max()
            .unwrap_or(i64::MIN),
    }
}

/// The time of `cursor` as the queue writes times, for a mark's `--at`:
/// `None` for an event id no event has.
pub fn cursor_time(cursor: Cursor, events: &[RunEvent]) -> Option<String> {
    match cursor {
        Cursor::Time(millis) => Some(utc_text(millis)),
        Cursor::Event(id) => events
            .iter()
            .find(|event| event.id == id)
            .map(|event| event.created_at.clone()),
    }
}

/// Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (SQLite's
/// `strftime('%Y-%m-%dT%H:%M:%fZ')`).
pub fn utc_text(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let rest = millis.rem_euclid(86_400_000);
    // The civil date of a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

fn is_queue_event(event: &RunEvent) -> bool {
    event.task_id.is_none() && event.goal_id.is_none() && event.run_id.is_none()
}

/// Each retracted mark and the first retraction of it.
fn retractions(events: &[RunEvent]) -> HashMap<EventId, EventId> {
    let mut retracted = HashMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == MARK_RETRACTED && is_queue_event(event))
    {
        if let Some(mark) = event.payload.get("mark").and_then(Value::as_i64) {
            retracted.entry(EventId::new(mark)).or_insert(event.id);
        }
    }
    retracted
}

/// A line for a person reading the list.
fn recorded_label(event: &RunEvent) -> String {
    let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
    let version = text("dagq_version").unwrap_or("unknown build");
    let parallel = event
        .payload
        .get("parallel")
        .map_or(Value::Null, Clone::clone);
    match event.kind.as_str() {
        SUPERVISOR_STARTED if event.payload.get("handoff") == Some(&Value::Bool(true)) => {
            format!("supervisor handed off to {version}, parallel {parallel}")
        }
        SUPERVISOR_STARTED => format!("supervisor started: {version}, parallel {parallel}"),
        SUPERVISOR_STOPPED => format!("supervisor stopped: {version}"),
        RUN_ENV_CHANGED => {
            let changed: Vec<&str> = event
                .payload
                .get("changed")
                .and_then(Value::as_array)
                .map(|keys| keys.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!("[run.env] changed: {}", changed.join(", "))
        }
        MARK_RETRACTED => format!(
            "retracted mark {}: {}",
            event.payload.get("mark").map_or(Value::Null, Clone::clone),
            text("label").unwrap_or_default()
        ),
        _ => text("label").unwrap_or_default().to_owned(),
    }
}

/// The attributes of `run_claimed` a derived mark is read off, and the
/// key of the same value on `supervisor_started` (a start that already
/// marks the change).
const DERIVED: [(&str, Option<&str>); 4] = [
    ("dagq_version", Some("dagq_version")),
    ("claude_version", None),
    ("parallel", Some("parallel")),
    ("toolchain", None),
];

/// The value of a derived attribute on a claim; `None` when the claim did
/// not record it. The toolchain is `rustc`'s release and host together.
fn claim_value(payload: &Value, attribute: &str) -> Option<String> {
    let value = |key: &str| match payload.get(key)? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    };
    if attribute == "toolchain" {
        return Some(format!(
            "{} {}",
            value("rustc_release")?,
            value("rustc_host")?
        ));
    }
    value(attribute)
}

/// Where a claim's attribute differs from the previous claim that recorded
/// it (ADR-0051 decision 10). A claim without the attribute is skipped, so
/// a first value is no mark. A change a supervisor's start between the two
/// claims already announces (the same new build or `parallel`) is that
/// start's mark and derives none.
fn derived_marks(events: &[RunEvent]) -> Vec<Mark> {
    let claims: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == RUN_CLAIMED && event.run_id.is_some())
        .collect();
    let starts: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == SUPERVISOR_STARTED && is_queue_event(event))
        .collect();
    let mut marks = Vec::new();
    for (attribute, start_key) in DERIVED {
        let mut last: Option<(String, EventId)> = None;
        for claim in &claims {
            let Some(value) = claim_value(&claim.payload, attribute) else {
                continue;
            };
            if let Some((previous, previous_id)) = &last
                && previous != &value
            {
                let announced = start_key.is_some_and(|key| {
                    starts.iter().any(|start| {
                        start.id > *previous_id
                            && start.id < claim.id
                            && claim_value(&start.payload, key).as_ref() == Some(&value)
                    })
                });
                if !announced {
                    marks.push(Mark {
                        id: None,
                        kind: format!("{DERIVED_PREFIX}{attribute}"),
                        at: claim.created_at.clone(),
                        recorded_at: claim.created_at.clone(),
                        label: format!("{attribute} {previous} → {value}"),
                        retracted_by: None,
                        detail: json!({
                            "attribute": attribute,
                            "from": previous,
                            "to": value,
                            "run_id": claim.run_id,
                            "task_id": claim.task_id,
                            "claim_event": claim.id,
                        }),
                    });
                }
            }
            last = Some((value, claim.id));
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, TaskId};

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn queue_event(id: i64, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: at.into(),
        }
    }

    fn claim(id: i64, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            task_id: Some(TaskId::new(id)),
            run_id: Some(RunId::new(format!("00000000-0000-4000-8000-{id:012}")).unwrap()),
            ..queue_event(id, RUN_CLAIMED, payload, at)
        }
    }

    fn attributes(version: &str, parallel: i64, rustc: Option<&str>) -> Value {
        json!({
            "dagq_version": version,
            "claude_version": "2.1.0",
            "rustc_release": rustc,
            "rustc_host": rustc.map(|_| "aarch64-apple-darwin"),
            "parallel": parallel,
        })
    }

    const SALT: &str = "the queue's secret";

    #[test]
    fn the_run_env_hashes_are_keyed_with_the_salt() {
        let table = pairs(&[("CARGO_BUILD_JOBS", "4")]);
        let digest = run_env_digest(&table, SALT);
        assert_eq!(digest, run_env_digest(&table, SALT));
        let other = run_env_digest(&table, "another queue's secret");
        assert_ne!(digest.hash, other.hash);
        assert_ne!(digest.keys, other.keys);
        // Neither is the plain hash a guess of the value would find.
        let plain = |text: &str| format!("{:x}", Sha256::digest(text.as_bytes()));
        let payload = digest.transition(None).unwrap().to_string();
        for guess in ["CARGO_BUILD_JOBS=4", "CARGO_BUILD_JOBS=4\n"] {
            assert!(!payload.contains(&plain(guess)[..16]), "{payload}");
        }
        assert!(!payload.contains(SALT));
    }

    #[test]
    fn the_run_env_hash_ignores_the_order_and_changes_with_any_value() {
        let a = run_env_digest(&pairs(&[("B", "2"), ("A", "1")]), SALT);
        assert_eq!(a, run_env_digest(&pairs(&[("A", "1"), ("B", "2")]), SALT));
        assert_eq!(a.hash.len(), 16);
        assert_ne!(
            a.hash,
            run_env_digest(&pairs(&[("A", "1"), ("B", "3")]), SALT).hash
        );
        assert_ne!(a.hash, run_env_digest(&pairs(&[("A", "1")]), SALT).hash);
    }

    #[test]
    fn run_env_marks_name_the_changed_keys_only_on_a_change() {
        let empty = run_env_digest(&[], SALT);
        assert_eq!(empty.transition(None), None, "no table, no first mark");
        let first = run_env_digest(&pairs(&[("CARGO_BUILD_JOBS", "4"), ("A", "x")]), SALT);
        let payload = first.transition(None).unwrap();
        assert_eq!(payload["changed"], json!(["A", "CARGO_BUILD_JOBS"]));
        assert_eq!(payload["previous_hash"], Value::Null);
        assert!(!payload.to_string().contains("\"4\""), "no value is kept");
        assert_eq!(first.transition(Some(&payload)), None);
        let next = run_env_digest(
            &pairs(&[
                ("CARGO_BUILD_JOBS", "2"),
                ("A", "x"),
                ("RUSTC_WRAPPER", "sccache"),
            ]),
            SALT,
        );
        let changed = next.transition(Some(&payload)).unwrap();
        assert_eq!(
            changed["changed"],
            json!(["CARGO_BUILD_JOBS", "RUSTC_WRAPPER"])
        );
        assert_eq!(changed["previous_hash"], json!(first.hash));
        let removed = empty.transition(Some(&changed)).unwrap();
        assert_eq!(
            removed["changed"],
            json!(["A", "CARGO_BUILD_JOBS", "RUSTC_WRAPPER"])
        );
    }

    #[test]
    fn a_mark_needs_a_short_label() {
        let payload = mark_payload(" parallel 4→3 ", Some(" "), None, "human").unwrap();
        assert_eq!(
            payload,
            json!({"label": "parallel 4→3", "note": null, "at": null, "by": "human"})
        );
        assert!(mark_payload("  ", None, None, "human").is_err());
        assert!(mark_payload(&"x".repeat(MAX_LABEL_CHARS + 1), None, None, "human").is_err());
    }

    #[test]
    fn retracting_takes_a_retractable_mark_once() {
        let mut events = vec![
            queue_event(
                1,
                MARK_RECORDED,
                json!({"label": "host arm64"}),
                "2026-09-26T00:00:00.000Z",
            ),
            queue_event(2, SUPERVISOR_STARTED, json!({}), "2026-09-26T00:00:01.000Z"),
        ];
        let payload = retraction_payload(&events, EventId::new(1), "planner").unwrap();
        assert_eq!(payload["label"], json!("host arm64"));
        assert!(retraction_payload(&events, EventId::new(2), "human").is_err());
        assert!(retraction_payload(&events, EventId::new(9), "human").is_err());
        events.push(queue_event(
            3,
            MARK_RETRACTED,
            payload,
            "2026-09-26T00:00:02.000Z",
        ));
        let error = retraction_payload(&events, EventId::new(1), "human").unwrap_err();
        assert!(error.contains("already retracted by 3"), "{error}");
        let listed = marks(&events, None, None);
        assert_eq!(listed[0].retracted_by, Some(EventId::new(3)));
        assert_eq!(listed[2].label, "retracted mark 1: host arm64");
    }

    #[test]
    fn marks_are_listed_by_the_time_they_took_effect_within_the_window() {
        let events = vec![
            queue_event(
                1,
                SUPERVISOR_STARTED,
                json!({"dagq_version": "0.4.0+a", "parallel": 3, "handoff": false}),
                "2026-09-26T01:00:00.000Z",
            ),
            queue_event(
                2,
                MARK_RECORDED,
                json!({"label": "host arm64", "at": "2026-09-26T00:30:00.000Z"}),
                "2026-09-26T02:00:00.000Z",
            ),
            queue_event(
                3,
                RUN_ENV_CHANGED,
                json!({"changed": ["RUSTC_WRAPPER"]}),
                "2026-09-26T03:00:00.000Z",
            ),
            queue_event(4, "observe_started", json!({}), "2026-09-26T04:00:00.000Z"),
            queue_event(
                5,
                SUPERVISOR_STOPPED,
                json!({"dagq_version": "0.4.0+a"}),
                "2026-09-26T05:00:00.000Z",
            ),
        ];
        let labels = |since, until| -> Vec<String> {
            marks(&events, since, until)
                .into_iter()
                .map(|mark| mark.label)
                .collect()
        };
        assert_eq!(
            labels(None, None),
            [
                "host arm64",
                "supervisor started: 0.4.0+a, parallel 3",
                "[run.env] changed: RUSTC_WRAPPER",
                "supervisor stopped: 0.4.0+a",
            ]
        );
        assert_eq!(
            labels(
                Some(Cursor::Event(EventId::new(1))),
                Some(Cursor::Event(EventId::new(4)))
            ),
            ["[run.env] changed: RUSTC_WRAPPER"]
        );
        let at = timestamp_millis("2026-09-26T00:45:00Z").unwrap();
        assert_eq!(labels(None, Some(Cursor::Time(at))), ["host arm64"]);
    }

    #[test]
    fn derived_marks_follow_the_claims_and_skip_what_a_start_announced() {
        let events = vec![
            claim(1, attributes("v1", 4, None), "2026-09-26T00:00:00.000Z"),
            claim(
                2,
                attributes("v1", 4, Some("1.89.0")),
                "2026-09-26T00:01:00.000Z",
            ),
            // A manual claim records no attribute and is skipped.
            claim(3, json!({}), "2026-09-26T00:02:00.000Z"),
            claim(
                4,
                attributes("v1", 4, Some("1.90.0")),
                "2026-09-26T00:03:00.000Z",
            ),
            queue_event(
                5,
                SUPERVISOR_STARTED,
                json!({"dagq_version": "v2", "parallel": 3}),
                "2026-09-26T00:04:00.000Z",
            ),
            claim(
                6,
                attributes("v2", 3, Some("1.90.0")),
                "2026-09-26T00:05:00.000Z",
            ),
            // A start with the same values changes nothing.
            queue_event(
                7,
                SUPERVISOR_STARTED,
                json!({"dagq_version": "v2", "parallel": 3}),
                "2026-09-26T00:06:00.000Z",
            ),
            claim(
                8,
                attributes("v2", 3, Some("1.90.0")),
                "2026-09-26T00:07:00.000Z",
            ),
            claim(
                9,
                attributes("v3", 3, Some("1.90.0")),
                "2026-09-26T00:08:00.000Z",
            ),
        ];
        let listed = marks(&events, None, None);
        let kinds: Vec<(&str, &str)> = listed
            .iter()
            .map(|mark| (mark.kind.as_str(), mark.label.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    "derived:toolchain",
                    "toolchain 1.89.0 aarch64-apple-darwin → 1.90.0 aarch64-apple-darwin"
                ),
                (SUPERVISOR_STARTED, "supervisor started: v2, parallel 3"),
                (SUPERVISOR_STARTED, "supervisor started: v2, parallel 3"),
                ("derived:dagq_version", "dagq_version v2 → v3"),
            ]
        );
        assert_eq!(listed[0].id, None);
        assert_eq!(listed[0].at, "2026-09-26T00:03:00.000Z");
        assert_eq!(listed[0].detail["claim_event"], json!(4));
        assert_eq!(listed[0].detail["task_id"], json!(4));
    }

    #[test]
    fn times_are_written_like_the_queue_writes_them() {
        for text in [
            "2026-09-26T08:52:00.000Z",
            "1970-01-01T00:00:00.000Z",
            "2000-02-29T23:59:59.999Z",
        ] {
            assert_eq!(utc_text(timestamp_millis(text).unwrap()), text);
        }
        let events = vec![queue_event(
            7,
            "observe_started",
            json!({}),
            "2026-09-26T00:00:00.000Z",
        )];
        assert_eq!(
            cursor_time(Cursor::Event(EventId::new(7)), &events).as_deref(),
            Some("2026-09-26T00:00:00.000Z")
        );
        assert_eq!(cursor_time(Cursor::Event(EventId::new(8)), &events), None);
        assert_eq!(
            cursor_time(Cursor::Time(0), &events).as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
    }
}
