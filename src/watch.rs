//! The inbox's notification path (ADR-0016, ADR-0024 decision 6): the attention that
//! `status` derives from the queue as it is now, `events --after` that reads
//! `run_events` past a cursor, and `watch` that blocks until an attention
//! event arrives or the supervisors' health changes. Everything here reads
//! the queue and writes nothing; which transition is an attention is decided
//! by `domain`.
use crate::{
    domain::{
        ASK_EVENT_KINDS, ATTENTION_KINDS, ATTENTION_ROLE, AskKind, Attention, AttentionNext,
        LANDING_OPTIONS, RunEvent, RunStatus, SessionRole, SupervisorPulse, SupervisorRegistration,
        TRIAGE_OPTIONS, TriageState, event_attention, run_attention, supervisor_attention,
        triage_state,
    },
    infrastructure::{
        adapters::process_alive,
        asks::AskQuery,
        runtime_store::{TRIAGE_ASKER, lease_is_stale},
        sqlite::SqliteQueue,
    },
    runtime::supervisors,
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

/// What waits for a person now: stale or missing supervisors first,
/// then the latest run of every `in_progress` task that rests where only a
/// person or the supervisor moves it on or that is unfinished without a lease,
/// then every landed run whose push of `main` failed with no successful push
/// since, then every ask nobody closed: an open one as `ask_opened` for the
/// inbox, an answered one as `ask_answered` for the person to act on
/// through the inbox (ADR-0022, ADR-0024 decision 6).
/// `kind` is the event that brought the run there (for a run without
/// a lease, its latest `runtime_error`).
pub fn attention(
    queue: &SqliteQueue,
    registrations: &[SupervisorRegistration],
    now: i64,
) -> Result<Vec<Attention>> {
    let mut attention = supervisor_attention(&pulses(registrations, now));
    for mut run in queue.latest_runs_in_progress()? {
        let leased = queue.run_lease(run.id())?.is_some();
        if !leased {
            // A supervisor leaves the unfinished statuses before it releases
            // the lease, so a run read before a release and its lease read
            // after it would look abandoned: judge it by its status now.
            run = queue.run(run.id())?;
        }
        // A run whose review raised a concern waits in its
        // `approve_landing` ask, which is the attention (ADR-0027).
        if run.status() == RunStatus::AwaitingIntegration
            && !leased
            && queue.has_unclosed_ask(run.id(), AskKind::ApproveLanding)?
        {
            continue;
        }
        let events = queue.run_events(run.id())?;
        let exit_pending = events
            .iter()
            .rev()
            .find(|e| matches!(e.kind.as_str(), "exit_request_timed_out" | "session_exited"))
            .is_some_and(|e| e.kind == "exit_request_timed_out");
        let Some(next) = run_attention(run.status(), exit_pending, false, leased) else {
            continue;
        };
        // A failed or interrupted run is the supervisor's triage until it
        // finished (its verdict moved the task or the run on, or its ask is
        // the attention) or failed (a person's).
        let next = match (next, triage_state(&events)) {
            (AttentionNext::Triaging, TriageState::Finished) => continue,
            (AttentionNext::Triaging, TriageState::Failed) => AttentionNext::TriageByHand,
            (next, _) => next,
        };
        let kind = events
            .iter()
            .rev()
            .find(|e| match next {
                // The error the owner gave up with, whatever its payload.
                AttentionNext::RecoverRun => e.kind == "runtime_error",
                // Whatever parked the run for a session last.
                AttentionNext::Resuming => {
                    e.payload.get("status").and_then(Value::as_str)
                        == Some(RunStatus::NeedsSession.as_str())
                }
                // An ask about the run is its own attention, not the run's.
                _ => {
                    !ASK_EVENT_KINDS.contains(&e.kind.as_str())
                        && event_attention(&e.kind, &e.payload).is_some()
                }
            })
            .map_or_else(|| run.status().as_str().to_owned(), |e| e.kind.clone());
        // After a failed headless review the run is a person's to review.
        let next = match next {
            AttentionNext::ReviewAndIntegrate if kind == "review_failed" => {
                AttentionNext::ReviewByHand
            }
            next => next,
        };
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            status: run.status().as_str().into(),
            kind,
            last_error: run.last_error().map(truncate),
            next,
        });
    }
    for run in queue.runs_with_pending_push()? {
        let Some(next) = run_attention(run.status(), false, true, false) else {
            continue;
        };
        let error = queue
            .run_events(run.id())?
            .into_iter()
            .rev()
            .find(|e| e.kind == "push_failed")
            .and_then(|e| e.payload.get("error").and_then(Value::as_str).map(truncate));
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            status: run.status().as_str().into(),
            kind: "push_failed".into(),
            last_error: error,
            next,
        });
    }
    for ask in queue.asks(AskQuery::default())? {
        let (status, kind, next) = if ask.is_open() {
            (
                "open",
                "ask_opened",
                AttentionNext::AnswerAsk { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor holding a running worker's lease types the
            // answer into its terminal; a failed send, a run no longer
            // running or one nobody supervises leaves it to the inbox.
            let failed = queue.run_events(run_id)?.iter().any(|e| {
                e.kind == "ask_delivery_failed"
                    && e.payload.get("ask_id").and_then(Value::as_i64) == Some(ask.id)
            });
            if failed {
                (
                    "answered",
                    "ask_delivery_failed",
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            } else if queue.run(run_id)?.status() == RunStatus::Running
                && queue
                    .run_lease(run_id)?
                    .is_some_and(|lease| !lease_is_stale(&lease, now))
            {
                (
                    "answered",
                    "ask_answered",
                    AttentionNext::DeliveringAnswer { ask_id: ask.id },
                )
            } else {
                (
                    "answered",
                    "ask_answered",
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            }
        } else if ask.kind == AskKind::Decide
            && ask.asked_by == TRIAGE_ASKER
            && let Some(run_id) = ask.run_id.as_ref()
            && matches!(
                queue.run(run_id)?.status(),
                RunStatus::Failed | RunStatus::Interrupted
            )
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| TRIAGE_OPTIONS.contains(&answer.trim()))
        {
            // The supervisor retries, resumes or cancels the triaged run.
            (
                "answered",
                "ask_answered",
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveLanding
            && let Some(run_id) = ask.run_id.as_ref()
            && queue.run(run_id)?.status() == RunStatus::AwaitingIntegration
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| LANDING_OPTIONS.contains(&answer.trim()))
        {
            // The supervisor lands, sends back or cancels the run itself.
            (
                "answered",
                "ask_answered",
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else {
            (
                "answered",
                "ask_answered",
                AttentionNext::ReadAnswer { ask_id: ask.id },
            )
        };
        attention.push(Attention {
            run_id: ask.run_id,
            task_id: ask.task_id,
            pid: None,
            ask_id: Some(ask.id),
            status: status.into(),
            kind: kind.into(),
            last_error: None,
            next,
        });
    }
    Ok(attention)
}

/// Whether attention is for `role`: all of it is the inbox's
/// ([`ATTENTION_ROLE`]), and without a role everything is shown. The
/// supervisors' health follows the same rule: the person restarts them.
pub fn for_role(role: Option<SessionRole>) -> bool {
    role.is_none_or(|role| role == ATTENTION_ROLE)
}

/// One event as the inbox reads it: the row's ids and kind, and from the
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
    if let Some(ask_id) = payload.get("ask_id") {
        object.insert("ask_id".into(), ask_id.clone());
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
/// for `role` only unless `all`, and the cursor to continue from: the last
/// event returned when the limit was reached, `upto` otherwise.
fn read_events(
    queue: &SqliteQueue,
    after: i64,
    upto: i64,
    limit: usize,
    all: bool,
    role: Option<SessionRole>,
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
pub fn events(db: &Path, after: i64, limit: usize, all: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let upto = queue.latest_event_id()?;
    let (events, cursor) = read_events(&queue, after, upto, limit.max(1), all, None)?;
    Ok(json!({"events": events, "cursor": cursor}))
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Cursor to wait past; `None` is the newest event when `watch` starts.
    pub after: Option<i64>,
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
    let baseline = pulses(&queue.supervisors()?, queue.generators().clock.now());
    let deadline = Instant::now() + options.timeout;
    loop {
        let upto = queue.latest_event_id()?;
        let (events, cursor) = read_events(&queue, after, upto, WATCH_LIMIT, false, options.role)?;
        let registrations = queue.supervisors()?;
        let now = queue.generators().clock.now();
        let changed = for_role(options.role) && pulses(&registrations, now) != baseline;
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
