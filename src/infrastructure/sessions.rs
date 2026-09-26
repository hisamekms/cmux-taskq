//! The spans of the Claude sessions dagq uses (ADR-0048 decision 2): after
//! an event that starts or ends one is inserted, the `session_opened` /
//! `session_closed` events [`crate::domain::sessions::changes`] decides are
//! written in the same transaction, at the same time as that event.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::sqlite::json_col;
use crate::domain::{
    EventId, RunId, TaskId,
    sessions::{
        INFERRED, JOB_FINISHED, OpenSpan, PLAN_REVIEW, SESSION_CLOSED, SESSION_OPENED, Scope,
        SpanChange, SpanContext, changes, scope,
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
    for change in changes(kind, payload, &open, &context) {
        let payload = match &change {
            SpanChange::Close { span, reason } => SpanChange::closed_payload(span, reason),
            SpanChange::Open(payload) => payload.clone(),
        };
        let kind = match change {
            SpanChange::Close { .. } => SESSION_CLOSED,
            SpanChange::Open(_) => SESSION_OPENED,
        };
        insert(conn, event_id, task_id, run_id, kind, &payload)?;
    }
    Ok(())
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
        conn.execute(
            "INSERT INTO run_events(task_id,kind,payload) VALUES (?1,?2,?3)",
            params![
                task_id,
                SESSION_CLOSED,
                serde_json::to_string(&SpanChange::closed_payload(&span, reason))?
            ],
        )?;
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
        revises: count("revise_requested")?,
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

/// Insert a span event at the time of the event `at`.
fn insert(
    conn: &Connection,
    at: EventId,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: &str,
    payload: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,created_at)
         SELECT ?1,?2,?3,?4,created_at FROM run_events WHERE id=?5",
        params![task_id, run_id, kind, serde_json::to_string(payload)?, at],
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
    }
}
