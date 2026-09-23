//! The maintainer's notification path (ADR-0016): the attention that
//! `status` derives from the queue as it is now, `events --after` that reads
//! `run_events` past a cursor, and `watch` that blocks until an attention
//! event arrives or the supervisors' health changes. Everything here reads
//! the queue and writes nothing; which transition is an attention is decided
//! by `domain`.
use crate::{
    domain::{
        ATTENTION_KINDS, Attention, RunEvent, SupervisorPulse, SupervisorRegistration,
        event_attention, run_attention, supervisor_attention,
    },
    infrastructure::{adapters::process_alive, sqlite::SqliteQueue},
    runtime::{supervisors, unix_time},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// A reason or error is cut to this many characters in the compact output.
const REASON_CHARS: usize = 300;

/// At most this many attention events come back from one `watch`; the
/// cursor then points at the last one returned.
const WATCH_LIMIT: usize = 100;

/// The health of every registered supervisor, in registration order.
pub fn pulses(registrations: &[SupervisorRegistration], now: i64) -> Vec<SupervisorPulse> {
    registrations
        .iter()
        .map(|r| SupervisorPulse::judge(r, process_alive(r.pid), now))
        .collect()
}

/// What waits for the maintainer now: stale or missing supervisors first,
/// then the latest run of every `in_progress` task that rests where only the
/// maintainer or the user moves it on, then every landed run whose push of
/// `main` failed with no successful push since. `kind` is the event that
/// brought the run there.
pub fn attention(
    queue: &SqliteQueue,
    registrations: &[SupervisorRegistration],
    now: i64,
) -> Result<Vec<Attention>> {
    let mut attention = supervisor_attention(&pulses(registrations, now));
    for run in queue.latest_runs_in_progress()? {
        let events = queue.run_events(&run.id)?;
        let exit_pending = events
            .iter()
            .rev()
            .find(|e| matches!(e.kind.as_str(), "exit_request_timed_out" | "session_exited"))
            .is_some_and(|e| e.kind == "exit_request_timed_out");
        let Some(next) = run_attention(run.status, exit_pending, false) else {
            continue;
        };
        let kind = events
            .iter()
            .rev()
            .find(|e| event_attention(&e.kind, &e.payload).is_some())
            .map_or_else(|| run.status.as_str().to_owned(), |e| e.kind.clone());
        attention.push(Attention {
            run_id: Some(run.id),
            task_id: Some(run.task_id),
            pid: None,
            status: run.status.as_str().into(),
            kind,
            last_error: run.last_error.as_deref().map(truncate),
            next,
        });
    }
    for run in queue.runs_with_pending_push()? {
        let Some(next) = run_attention(run.status, false, true) else {
            continue;
        };
        let error = queue
            .run_events(&run.id)?
            .into_iter()
            .rev()
            .find(|e| e.kind == "push_failed")
            .and_then(|e| e.payload.get("error").and_then(Value::as_str).map(truncate));
        attention.push(Attention {
            run_id: Some(run.id),
            task_id: Some(run.task_id),
            pid: None,
            status: run.status.as_str().into(),
            kind: "push_failed".into(),
            last_error: error,
            next,
        });
    }
    Ok(attention)
}

/// One event as the maintainer reads it: the row's ids and kind, and from the
/// payload only `status`, `exit_code` and a truncated `reason` (from
/// `reason`, `message` or `error`). Paths and receipts are left out.
/// An attention event also carries its `next`.
pub fn compact_event(event: &RunEvent) -> Value {
    let mut value = json!({"id": event.id, "kind": event.kind});
    let object = value.as_object_mut().expect("object literal");
    if let Some(task_id) = event.task_id {
        object.insert("task_id".into(), json!(task_id));
    }
    if let Some(goal_id) = event.goal_id {
        object.insert("goal_id".into(), json!(goal_id));
    }
    if let Some(run_id) = &event.run_id {
        object.insert("run_id".into(), json!(run_id));
    }
    let payload = &event.payload;
    if let Some(status) = payload.get("status").or_else(|| payload.get("to")) {
        object.insert("status".into(), status.clone());
    }
    if let Some(code) = payload.get("exit_code") {
        object.insert("exit_code".into(), code.clone());
    }
    if let Some(reason) = ["reason", "message", "error"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
    {
        object.insert("reason".into(), json!(truncate(reason)));
    }
    if let Some(next) = event_attention(&event.kind, payload) {
        object.insert("next".into(), json!(next));
    }
    object.insert("created_at".into(), json!(event.created_at));
    value
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(REASON_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Up to `limit` events with `after < id <= upto` in compact form, attention
/// only unless `all`, and the cursor to continue from: the last event
/// returned when the limit was reached, `upto` otherwise.
fn read_events(
    queue: &SqliteQueue,
    after: i64,
    upto: i64,
    limit: usize,
    all: bool,
) -> Result<(Vec<Value>, i64)> {
    let kinds = (!all).then_some(ATTENTION_KINDS);
    let mut events = Vec::new();
    let mut cursor = after;
    loop {
        // An attention kind can still be dropped by its payload, so pages
        // are read until the limit is filled or the range is exhausted.
        let page = queue.events_between(cursor, upto, kinds, limit)?;
        if page.is_empty() {
            return Ok((events, upto));
        }
        for event in page {
            cursor = event.id;
            if all || event_attention(&event.kind, &event.payload).is_some() {
                events.push(compact_event(&event));
                if events.len() == limit {
                    return Ok((events, cursor));
                }
            }
        }
    }
}

/// `events --after`: the events after `after`, oldest first.
pub fn events(db: &Path, after: i64, limit: usize, all: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let upto = queue.latest_event_id()?;
    let (events, cursor) = read_events(&queue, after, upto, limit.max(1), all)?;
    Ok(json!({"events": events, "cursor": cursor}))
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Cursor to wait past; `None` is the newest event when `watch` starts.
    pub after: Option<i64>,
    pub timeout: Duration,
    pub interval: Duration,
}

/// Block until an attention event past the cursor exists or the health of
/// the registered supervisors (the set of tokens, their PIDs, `alive` and
/// `stale`) differs from what it was when `watch` started, reading the queue
/// every `interval`. A timeout returns no events and the cursor unchanged.
/// Never writes and never integrates.
pub fn watch(db: &Path, options: &WatchOptions) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let after = match options.after {
        Some(after) => after,
        None => queue.latest_event_id()?,
    };
    let baseline = pulses(&queue.supervisors()?, unix_time());
    let deadline = Instant::now() + options.timeout;
    loop {
        let upto = queue.latest_event_id()?;
        let (events, cursor) = read_events(&queue, after, upto, WATCH_LIMIT, false)?;
        let registrations = queue.supervisors()?;
        let now = unix_time();
        let changed = pulses(&registrations, now) != baseline;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !events.is_empty() || changed || remaining.is_zero() {
            let cursor = if events.is_empty() && !changed {
                after
            } else {
                cursor
            };
            return Ok(json!({
                "events": events,
                "supervisors_changed": changed,
                "supervisors": supervisors(&registrations, &queue.run_leases()?, now),
                "cursor": cursor,
            }));
        }
        thread::sleep(options.interval.min(remaining));
    }
}
