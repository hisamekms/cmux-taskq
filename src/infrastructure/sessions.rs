//! The spans of the Claude sessions dagq uses (ADR-0048 decision 2): after
//! an event that starts or ends one is inserted, the `session_opened` /
//! `session_closed` events [`crate::domain::sessions::changes`] decides are
//! written in the same transaction, at the same time as that event. A span
//! that closes takes its transcript's turns with it (its active time,
//! decision 8), and the supervisor records the finished turns of the spans
//! still open ([`record_open_turns`]).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::{sqlite::json_col, transcripts::ClaudeTranscripts};
use crate::{
    application::{TranscriptSource, Transcripts},
    domain::{
        EventId, RunId, TaskId,
        sessions::{
            INFERRED, JOB_FINISHED, OpenSpan, PLAN_REVIEW, SESSION_CLOSED, SESSION_OPENED,
            SESSION_TURNS, Scope, SpanChange, SpanContext, changes, scope,
        },
        stats::rfc3339_millis,
        transcript::{Transcript, Turn, Unreadable, millis_text, span_turns, turns},
    },
};

/// Write the spans the event `event_id` (of `kind`, with `payload`, just
/// inserted on `task_id` and `run_id`) opens and closes.
pub(super) fn follow(
    conn: &Connection,
    event_id: EventId,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: &str,
    payload: &Value,
) -> Result<()> {
    let Some(scope) = scope(kind) else {
        return Ok(());
    };
    let (open, context) = match (scope, task_id, run_id) {
        (Scope::Run, Some(task_id), Some(run_id)) => (
            open_spans(
                conn,
                "o.task_id=?1 AND o.run_id=?2",
                "c.task_id=?1 AND c.run_id=?2",
                params![task_id, run_id],
            )?,
            run_context(conn, run_id)?,
        ),
        (Scope::Proposal, Some(task_id), None) => {
            let proposal = payload["proposal_id"].as_i64();
            (
                open_spans(
                    conn,
                    "o.task_id=?1 AND o.run_id IS NULL AND json_extract(o.payload,'$.proposal_id')=?2",
                    "c.task_id=?1 AND c.run_id IS NULL",
                    params![task_id, proposal],
                )?,
                SpanContext {
                    goal_ids: proposal_goals(conn, proposal)?,
                    ..SpanContext::default()
                },
            )
        }
        (Scope::Queue, None, None) => (
            open_spans(
                conn,
                "o.task_id IS NULL AND o.goal_id IS NULL AND json_extract(o.payload,'$.kind')='observer'",
                "c.task_id IS NULL AND c.goal_id IS NULL",
                params![],
            )?,
            SpanContext::default(),
        ),
        _ => return Ok(()),
    };
    let changes = changes(kind, payload, &open, &context);
    if changes.is_empty() {
        return Ok(());
    }
    let at: String = conn.query_row(
        "SELECT created_at FROM run_events WHERE id=?1",
        [event_id],
        |r| r.get(0),
    )?;
    // The spans switch when the revise was sent (`sent_at`), so that its
    // turn is the revise's: a `revise_requested` written after the revise
    // was typed (as before task 241) comes later than that.
    let at = payload["sent_at"]
        .as_i64()
        .filter(|_| kind == "revise_requested")
        .map(|secs| millis_text(secs * 1000))
        .filter(|sent| rfc3339_millis(sent) < rfc3339_millis(&at))
        .unwrap_or(at);
    for change in changes {
        match change {
            SpanChange::Close { span, reason } => {
                close(conn, &at, task_id, run_id, &span, reason)?;
            }
            SpanChange::Open(payload) => {
                insert_at(conn, task_id, run_id, SESSION_OPENED, &payload, &at)?;
            }
        }
    }
    Ok(())
}

/// The time now, in the form of `created_at`.
fn now(conn: &Connection) -> Result<String> {
    Ok(
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
            r.get(0)
        })?,
    )
}

/// Write the `session_closed` of `span` at `now`, with the turns of its
/// transcript not recorded yet and its active time. A span closed as `inferred` ends at its transcript's last
/// record when that is earlier (ADR-0048 decision 7). A transcript that
/// cannot be read leaves the active time unrecorded, and says why.
fn close(
    conn: &Connection,
    now: &str,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    span: &OpenSpan,
    reason: &str,
) -> Result<()> {
    let mut payload = SpanChange::closed_payload(span, reason);
    let mut closed_at = now.to_owned();
    match (read(span), times(conn, span, now)?) {
        (Ok(transcript), Some((start, now_ms))) => {
            let mut end = now_ms;
            if reason == INFERRED
                && let Some(last) = transcript.last_at()
            {
                end = last.clamp(start, now_ms);
            }
            if Some(end) != rfc3339_millis(now) {
                closed_at = millis_text(end);
            }
            let recorded = recorded_turns(conn, span.opened_event_id)?;
            let through = recorded.iter().map(|turn| turn.end).max();
            let new = span_turns(turns(&transcript.records).all(), start, Some(end), through);
            if !new.is_empty() {
                let turns_payload = turns_payload(span, &new);
                insert_at(conn, task_id, run_id, SESSION_TURNS, &turns_payload, now)?;
            }
            let millis: i64 = recorded.iter().chain(&new).map(|turn| turn.millis()).sum();
            payload["active"] = json!("recorded");
            payload["active_secs"] = json!(millis / 1000);
        }
        (Err(unreadable), _) => {
            unavailable(span, &unreadable);
            payload["active"] = json!("unavailable");
            payload["active_unavailable"] = json!(unreadable.code);
        }
        (Ok(_), None) => {
            payload["active"] = json!("unavailable");
            payload["active_unavailable"] = json!("span_time_unparsable");
        }
    }
    insert_at(conn, task_id, run_id, SESSION_CLOSED, &payload, &closed_at)
}

/// The transcript of `span`.
fn read(span: &OpenSpan) -> Result<Transcript, Unreadable> {
    let text = |key: &str| span.payload[key].as_str().map(str::to_owned);
    ClaudeTranscripts::from_env().read(&TranscriptSource {
        session_id: text("session_id"),
        cwd: text("cwd"),
        transcript_path: text("transcript_path"),
    })
}

/// Say in the log why the active time of `span` is not recorded.
fn unavailable(span: &OpenSpan, unreadable: &Unreadable) {
    info!(
        code = unreadable.code,
        version = unreadable.version.as_deref().unwrap_or("unknown"),
        "session span {} ({}): active time not recorded, {}: {} (Claude Code {})",
        span.opened_event_id,
        span.kind(),
        unreadable.code,
        unreadable.detail,
        unreadable.version.as_deref().unwrap_or("version unknown"),
    );
}

/// Unix milliseconds of the start of `span` and of `now`.
fn times(conn: &Connection, span: &OpenSpan, now: &str) -> Result<Option<(i64, i64)>> {
    let start: String = conn.query_row(
        "SELECT created_at FROM run_events WHERE id=?1",
        [span.opened_event_id],
        |r| r.get(0),
    )?;
    Ok(rfc3339_millis(&start)
        .zip(rfc3339_millis(now))
        .map(|(start, now)| (start, now.max(start))))
}

/// The turns of the span opened by `opened` recorded so far.
fn recorded_turns(conn: &Connection, opened: EventId) -> Result<Vec<Turn>> {
    let payloads: Vec<Value> = conn
        .prepare(&format!(
            "SELECT payload FROM run_events WHERE kind='{SESSION_TURNS}'
               AND json_extract(payload,'$.opened_event_id')=?1 ORDER BY id"
        ))?
        .query_map([opened], |r| json_col(r, "payload"))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(payloads
        .iter()
        .flat_map(|payload| payload["turns"].as_array().cloned().unwrap_or_default())
        .filter_map(|turn| {
            let time = |at: usize| {
                turn.get(at)
                    .and_then(Value::as_str)
                    .and_then(rfc3339_millis)
            };
            Some(Turn {
                start: time(0)?,
                end: time(1)?,
            })
        })
        .collect())
}

/// The payload of a `session_turns` of `span`: its turns as
/// `[start, end]`, and `through`, the end of the last one.
fn turns_payload(span: &OpenSpan, turns: &[Turn]) -> Value {
    json!({
        "opened_event_id": span.opened_event_id,
        "kind": span.kind(),
        "session_id": span.session_id(),
        "turns": turns
            .iter()
            .map(|turn| [millis_text(turn.start), millis_text(turn.end)])
            .collect::<Vec<_>>(),
        "through": turns.iter().map(|turn| turn.end).max().map(millis_text),
    })
}

/// Record the finished turns (those the next input followed) of every span
/// still open that are not recorded yet, one `session_turns` per span, on
/// the task and run of its `session_opened` (ADR-0048 decision 8). A
/// transcript that cannot be read now is left for the next time. Returns
/// how many spans got turns. The transcript is read outside the write lock;
/// the span is checked again under it, since the process that closes it
/// (a session's wrapper) records its remaining turns itself.
pub(super) fn record_open_turns(conn: &Connection) -> Result<usize> {
    let open = open_spans(conn, "1=1", "1=1", params![])?;
    let mut recorded = 0;
    for span in open {
        let transcript = match read(&span) {
            Ok(transcript) => transcript,
            Err(unreadable) => {
                debug!(
                    "session span {} ({}): turns not read now, {}: {}",
                    span.opened_event_id,
                    span.kind(),
                    unreadable.code,
                    unreadable.detail
                );
                continue;
            }
        };
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let closed: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM run_events WHERE kind='{SESSION_CLOSED}'
                   AND json_extract(payload,'$.opened_event_id')=?1)"
            ),
            [span.opened_event_id],
            |r| r.get(0),
        )?;
        if closed {
            continue;
        }
        let now = now(&tx)?;
        let Some((start, _)) = times(&tx, &span, &now)? else {
            continue;
        };
        let through = recorded_turns(&tx, span.opened_event_id)?
            .iter()
            .map(|turn| turn.end)
            .max();
        let new = span_turns(turns(&transcript.records).complete, start, None, through);
        if new.is_empty() {
            continue;
        }
        let (task_id, run_id): (Option<TaskId>, Option<RunId>) = tx.query_row(
            "SELECT task_id, run_id FROM run_events WHERE id=?1",
            [span.opened_event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        insert_at(
            &tx,
            task_id,
            run_id.as_ref(),
            SESSION_TURNS,
            &turns_payload(&span, &new),
            &now,
        )?;
        tx.commit()?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Close the span of the plan review `plan_review_id` when it is still open:
/// its row was finished as `interrupted` without a `plan_review_finished` /
/// `plan_review_failed` (ADR-0048 decision 7). `inferred` says its
/// supervisor was gone; otherwise the job ended when the proposal moved on.
pub(super) fn close_plan_review(
    conn: &Connection,
    plan_review_id: i64,
    inferred: bool,
) -> Result<()> {
    let open = open_spans(
        conn,
        "o.run_id IS NULL AND json_extract(o.payload,'$.kind')=?1 AND json_extract(o.payload,'$.plan_review_id')=?2",
        "c.run_id IS NULL",
        params![PLAN_REVIEW, plan_review_id],
    )?;
    let reason = if inferred { INFERRED } else { JOB_FINISHED };
    for span in open {
        let task_id: Option<TaskId> = conn.query_row(
            "SELECT task_id FROM run_events WHERE id=?1",
            [span.opened_event_id],
            |r| r.get(0),
        )?;
        close(conn, &now(conn)?, task_id, None, &span, reason)?;
    }
    Ok(())
}

/// The `session_opened` events matching `opened` that no `session_closed`
/// matching `closed` names, oldest first. Both conditions share `params`.
fn open_spans(
    conn: &Connection,
    opened: &str,
    closed: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<OpenSpan>> {
    let sql = format!(
        "SELECT o.id, o.payload FROM run_events o
         WHERE o.kind='{SESSION_OPENED}' AND {opened}
           AND NOT EXISTS (SELECT 1 FROM run_events c
                           WHERE c.kind='{SESSION_CLOSED}' AND {closed}
                             AND json_extract(c.payload,'$.opened_event_id')=o.id)
         ORDER BY o.id"
    );
    Ok(conn
        .prepare(&sql)?
        .query_map(params, |r| {
            Ok(OpenSpan {
                opened_event_id: r.get("id")?,
                payload: json_col(r, "payload")?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn run_context(conn: &Connection, run_id: &RunId) -> Result<SpanContext> {
    let run = conn
        .query_row(
            "SELECT worktree_path, run_dir, workspace_id FROM task_runs WHERE id=?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (worktree, run_dir, workspace_id) = run.unwrap_or((None, None, None));
    let count = |kind: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind=?2",
            params![run_id, kind],
            |r| r.get(0),
        )?)
    };
    Ok(SpanContext {
        worktree,
        run_dir,
        workspace_id,
        resumes: count("resume_started")?,
        revises: count("revise_requested")? - count("revise_unsent")?,
        goal_ids: Vec::new(),
    })
}

fn proposal_goals(conn: &Connection, proposal: Option<i64>) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(
            "SELECT DISTINCT goal_id FROM tasks
             WHERE proposal_id=?1 AND goal_id IS NOT NULL ORDER BY goal_id",
        )?
        .query_map([proposal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Insert a span event at `created_at`.
fn insert_at(
    conn: &Connection,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: &str,
    payload: &Value,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5)",
        params![
            task_id,
            run_id,
            kind,
            serde_json::to_string(payload)?,
            created_at
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::TaskStore,
        domain::{NewTask, RunEvent},
        infrastructure::sqlite::{SqliteQueue, event, event_row},
    };
    use serde_json::json;

    fn task(queue: &mut SqliteQueue) -> TaskId {
        queue
            .add(NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                goal_id: None,
                context: String::new(),
                kind: None,
            })
            .unwrap()
            .id()
    }

    fn spans(queue: &SqliteQueue) -> Vec<RunEvent> {
        queue
            .conn
            .prepare("SELECT * FROM run_events WHERE kind IN ('session_opened','session_closed') ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The events of a run, a plan review and the observer write their
    /// spans next to them, at their time; every span opened is closed once.
    #[test]
    fn events_open_and_close_their_spans_at_their_time() {
        let dir = tempfile::tempdir().unwrap();
        // No transcripts: every span closes without its active time.
        ClaudeTranscripts::use_config_dir_in_test(dir.path());
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = task(&mut queue);
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES (?1,?2,'running','claude','claude','b','/wt','W','/run')",
                params![run, task_id],
            )
            .unwrap();
        let conn = &queue.conn;
        let record = |kind: &str, payload: Value| {
            event(conn, task_id, Some(&run), kind, payload).unwrap();
        };
        record("run_claimed", json!({}));
        record("agent_started", json!({"session_id": run}));
        record("revise_requested", json!({"workspace_id": "W"}));
        record(
            "review_started",
            json!({"attempt": 1, "session_id": "s-review"}),
        );
        record("review_finished", json!({"verdict": "pass"}));
        record("session_exited", json!({"exit_code": 0}));
        record("resume_started", json!({}));
        record("agent_started", json!({"session_id": run}));
        // The resume's session was lost: the triage closes it as inferred.
        record(
            "triage_started",
            json!({"attempt": 1, "session_id": "s-triage"}),
        );
        record("triage_failed", json!({}));
        event(
            conn,
            task_id,
            None,
            "plan_review_started",
            json!({"proposal_id": 1, "plan_review_id": 7, "attempt": 1, "session_id": "s-plan"}),
        )
        .unwrap();
        close_plan_review(conn, 7, true).unwrap();
        // Closed once: its finish finds nothing open.
        event(
            conn,
            task_id,
            None,
            "plan_review_failed",
            json!({"proposal_id": 1, "plan_review_id": 7}),
        )
        .unwrap();
        let observed = queue
            .record_queue_event(
                "observe_started",
                json!({"mode": "hourly", "dir": "/obs", "session_id": "s-obs"}),
            )
            .unwrap();
        queue
            .record_queue_event("observe_finished", json!({"dir": "/obs"}))
            .unwrap();

        let events = spans(&queue);
        let described: Vec<(String, String, Value)> = events
            .iter()
            .map(|e| {
                (
                    e.kind.clone(),
                    e.payload["kind"].as_str().unwrap().to_owned(),
                    e.payload
                        .get("reason")
                        .cloned()
                        .unwrap_or_else(|| e.payload["session_id"].clone()),
                )
            })
            .collect();
        let row =
            |kind: &str, span: &str, detail: Value| (kind.to_owned(), span.to_owned(), detail);
        assert_eq!(
            described,
            vec![
                row(SESSION_OPENED, "worker", json!(run)),
                row(SESSION_CLOSED, "worker", json!("next_span")),
                row(SESSION_OPENED, "revise", json!(run)),
                row(SESSION_OPENED, "review", json!("s-review")),
                row(SESSION_CLOSED, "review", json!("job_finished")),
                row(SESSION_CLOSED, "revise", json!("exited")),
                row(SESSION_OPENED, "resume", json!(run)),
                row(SESSION_CLOSED, "resume", json!("inferred")),
                row(SESSION_OPENED, "triage", json!("s-triage")),
                row(SESSION_CLOSED, "triage", json!("job_finished")),
                row(SESSION_OPENED, "plan_review", json!("s-plan")),
                row(SESSION_CLOSED, "plan_review", json!("inferred")),
                row(SESSION_OPENED, "observer", json!("s-obs")),
                row(SESSION_CLOSED, "observer", json!("job_finished")),
            ]
        );
        assert_eq!(events[0].payload["cwd"], "/wt");
        assert_eq!(events[0].payload["workspace_id"], "W");
        assert_eq!(events[6].payload["attempt"], 1);
        assert_eq!(events[8].payload["cwd"], "/run");
        assert_eq!(events[1].payload["opened_event_id"], json!(events[0].id));
        assert!(events[12].id > observed);
        // Each span event has the time of the event that wrote it.
        let time = |id: EventId| -> String {
            queue
                .conn
                .query_row("SELECT created_at FROM run_events WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        let started: EventId = queue
            .conn
            .query_row(
                "SELECT min(id) FROM run_events WHERE kind='agent_started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(time(events[0].id), time(started));
        assert_eq!(time(events[12].id), time(observed));
        for closed in events.iter().filter(|e| e.kind == SESSION_CLOSED) {
            assert_eq!(closed.payload["active"], "unavailable");
        }
        assert_eq!(
            events[1].payload["active_unavailable"],
            "transcript_missing"
        );
    }

    const RUN: &str = "22222222-2222-4222-8222-222222222222";

    /// A queue with one running run in `/wt`, its transcripts under
    /// `config`.
    fn run_queue(dir: &std::path::Path) -> (SqliteQueue, TaskId, RunId) {
        ClaudeTranscripts::use_config_dir_in_test(&dir.join("config"));
        let mut queue = SqliteQueue::init(dir.join("q.db")).unwrap();
        let task_id = task(&mut queue);
        let run = RunId::new(RUN).unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES (?1,?2,'running','claude','claude','b','/wt','W','/run')",
                params![run, task_id],
            )
            .unwrap();
        (queue, task_id, run)
    }

    /// Move every event after `after` to `secs` seconds ago; returns that
    /// time in unix milliseconds.
    fn retime(conn: &Connection, after: i64, secs: i64) -> i64 {
        conn.execute(
            "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now',?1) WHERE id>?2",
            params![format!("-{secs} seconds"), after],
        )
        .unwrap();
        let at: String = conn
            .query_row("SELECT max(created_at) FROM run_events", [], |r| r.get(0))
            .unwrap();
        rfc3339_millis(&at).unwrap()
    }

    fn latest(conn: &Connection) -> i64 {
        conn.query_row("SELECT max(id) FROM run_events", [], |r| r.get(0))
            .unwrap()
    }

    /// Write the transcript of the run's session: `turns` as (input, last
    /// output) offsets in seconds from `base`, and an unanswered input at
    /// `pending` when given.
    fn transcript(dir: &std::path::Path, base: i64, turns: &[(i64, i64)], pending: Option<i64>) {
        let project = dir.join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(base + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283", "message": {"content": content}})
            .to_string()
        };
        let mut lines = Vec::new();
        for &(input, output) in turns {
            lines.push(line("user", input, json!("go")));
            lines.push(line("assistant", output, json!([{"type": "text"}])));
        }
        if let Some(pending) = pending {
            lines.push(line("user", pending, json!("more")));
        }
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
    }

    fn of_kind(queue: &SqliteQueue, kind: &str) -> Vec<RunEvent> {
        queue
            .conn
            .prepare("SELECT * FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The finished turns of an open span are recorded as they come; the
    /// span's close records the rest once and its active time.
    #[test]
    fn a_span_records_its_turns_while_open_and_its_active_time_at_its_close() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            "agent_started",
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        // The first turn is finished (the next input came); the second not.
        transcript(dir.path(), start, &[(5, 25), (40, 50)], None);
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        // Nothing new: nothing written.
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        let recorded = of_kind(&queue, SESSION_TURNS);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].payload["kind"], "worker");
        assert_eq!(recorded[0].payload["turns"].as_array().unwrap().len(), 1);
        assert_eq!(
            recorded[0].payload["through"],
            json!(millis_text(start + 25_000))
        );
        assert_eq!(recorded[0].run_id.as_ref(), Some(&run));

        event(
            conn,
            task_id,
            Some(&run),
            "session_exited",
            json!({"exit_code": 0}),
        )
        .unwrap();
        let recorded = of_kind(&queue, SESSION_TURNS);
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded[1].payload["turns"],
            json!([[millis_text(start + 40_000), millis_text(start + 50_000)]])
        );
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], "exited");
        assert_eq!(closed.payload["active"], "recorded");
        assert_eq!(closed.payload["active_secs"], 30);
    }

    /// A revise typed into the session before its `revise_requested` was
    /// written switches the spans when it was sent: its turn is the
    /// revise's, not the worker's.
    #[test]
    fn a_revise_switches_the_spans_when_it_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            "agent_started",
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        // The worker's turn, then the revise typed at 60 s, answered by 80 s.
        transcript(dir.path(), start, &[(5, 25), (61, 80)], None);
        let sent = (start + 60_000) / 1000;
        event(
            conn,
            task_id,
            Some(&run),
            "revise_requested",
            json!({"attempt": 1, "sent_at": sent}),
        )
        .unwrap();
        let spans = spans(&queue);
        assert_eq!(spans[1].payload["active_secs"], 20);
        assert_eq!(spans[1].created_at, millis_text(sent * 1000));
        assert_eq!(spans[2].kind, SESSION_OPENED);
        assert_eq!(spans[2].created_at, millis_text(sent * 1000));
        event(
            conn,
            task_id,
            Some(&run),
            "session_exited",
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[1];
        assert_eq!(closed.payload["kind"], "revise");
        assert_eq!(closed.payload["active_secs"], 19);
    }

    /// A span closed as inferred ends at its transcript's last record; its
    /// turns are cut there. One whose transcript cannot be read says why,
    /// and the event that closed it is written all the same.
    #[test]
    fn an_inferred_close_ends_at_the_transcript_and_an_unreadable_one_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            "agent_started",
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript(dir.path(), start, &[(5, 20)], Some(30));
        event(conn, task_id, Some(&run), "workspace_closed", json!({})).unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], "inferred");
        assert_eq!(closed.payload["active_secs"], 15);
        assert_eq!(closed.created_at, millis_text(start + 30_000));

        // The resume's session: its transcript is not JSON.
        event(conn, task_id, Some(&run), "resume_started", json!({})).unwrap();
        let before = latest(conn);
        event(
            conn,
            task_id,
            Some(&run),
            "agent_started",
            json!({"session_id": RUN}),
        )
        .unwrap();
        retime(conn, before, 10);
        std::fs::write(
            dir.path().join(format!("config/projects/-wt/{RUN}.jsonl")),
            "not json",
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            "session_exited",
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[1];
        assert_eq!(closed.payload["kind"], "resume");
        assert_eq!(closed.payload["active"], "unavailable");
        assert_eq!(
            closed.payload["active_unavailable"],
            "transcript_unparsable"
        );
        assert_eq!(of_kind(&queue, "session_exited").len(), 1);
        // An open span whose transcript cannot be read is left for later.
        event(
            conn,
            task_id,
            Some(&run),
            "agent_started",
            json!({"session_id": RUN}),
        )
        .unwrap();
        assert_eq!(record_open_turns(conn).unwrap(), 0);
    }
}
