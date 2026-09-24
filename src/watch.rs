//! The inbox's notification path (ADR-0016, ADR-0024 decision 6):
//! `events --after` that reads `run_events` past a cursor, and `watch` that
//! blocks until an attention event arrives or the supervisors' health
//! changes. The attention `status` derives from the queue as it is now is
//! [`crate::application::health::attention`]. Everything here reads the
//! queue and writes nothing; which transition is an attention is decided
//! by `domain`.
pub use crate::application::health::compact_event;
use crate::{
    application::health::{for_role, pulses, supervisors},
    domain::{ATTENTION_KINDS, EventId, SessionRole, event_attention},
    infrastructure::{adapters::SystemProcesses, sqlite::SqliteQueue},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// At most this many attention events come back from one `watch`; the
/// cursor then points at the last one returned.
const WATCH_LIMIT: usize = 100;

/// Up to `limit` events with `after < id <= upto` in compact form, attention
/// for `role` only unless `all`, and the cursor to continue from: the last
/// event returned when the limit was reached, `upto` otherwise.
fn read_events(
    queue: &SqliteQueue,
    after: EventId,
    upto: EventId,
    limit: usize,
    all: bool,
    role: Option<SessionRole>,
) -> Result<(Vec<Value>, EventId)> {
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
            if all || (event_attention(&event.kind, &event.payload).is_some() && for_role(role)) {
                events.push(compact_event(&event));
                if events.len() == limit {
                    return Ok((events, cursor));
                }
            }
        }
    }
}

/// `events --after`: the events after `after`, oldest first.
pub fn events(db: &Path, after: EventId, limit: usize, all: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let upto = queue.latest_event_id()?;
    let (events, cursor) = read_events(&queue, after, upto, limit.max(1), all, None)?;
    Ok(json!({"events": events, "cursor": cursor}))
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Cursor to wait past; `None` is the newest event when `watch` starts.
    pub after: Option<EventId>,
    pub timeout: Duration,
    pub interval: Duration,
    /// Only the attention addressed to this role (ADR-0022); `None` is all.
    pub role: Option<SessionRole>,
}

/// Block until an attention event past the cursor exists or the health of
/// the registered supervisors (the set of tokens, their PIDs, `alive` and
/// `stale`) differs from what it was when `watch` started, reading the queue
/// every `interval`. With a `role`, only the attention events addressed to
/// it count, the supervisors' health included. A timeout
/// returns no events and the cursor unchanged. Never writes and never
/// integrates.
pub fn watch(db: &Path, options: &WatchOptions) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let after = match options.after {
        Some(after) => after,
        None => queue.latest_event_id()?,
    };
    let baseline = pulses(
        &queue.supervisors()?,
        queue.generators().clock.now(),
        &SystemProcesses,
    );
    let deadline = Instant::now() + options.timeout;
    loop {
        let upto = queue.latest_event_id()?;
        let (events, cursor) = read_events(&queue, after, upto, WATCH_LIMIT, false, options.role)?;
        let registrations = queue.supervisors()?;
        let now = queue.generators().clock.now();
        let changed =
            for_role(options.role) && pulses(&registrations, now, &SystemProcesses) != baseline;
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
                "supervisors": supervisors(&registrations, &queue.run_leases()?, now, &SystemProcesses),
                "cursor": cursor,
            }));
        }
        thread::sleep(options.interval.min(remaining));
    }
}
