//! The Claude sessions dagq uses, recorded as spans (ADR-0048 decisions 1,
//! 2 and 7): a span is a session open for one purpose, with its kind, its
//! session id and the `session_opened` / `session_closed` events that start
//! and end it. The runtime writes them next to the event that starts or ends
//! the span, in the same transaction; this module decides which spans an
//! event opens and closes.

use serde_json::{Value, json};

use super::EventId;

pub const SESSION_OPENED: &str = "session_opened";
pub const SESSION_CLOSED: &str = "session_closed";
/// The transcript's turns of a span, recorded while it is open and when it
/// closes (ADR-0048 decision 8).
pub const SESSION_TURNS: &str = "session_turns";

pub const WORKER: &str = "worker";
pub const RESUME: &str = "resume";
pub const REVISE: &str = "revise";
pub const REVIEW: &str = "review";
pub const TRIAGE: &str = "triage";
pub const OBSERVER: &str = "observer";
pub const PLAN_REVIEW: &str = "plan_review";
pub const RUNTIME_PLANNER: &str = "runtime_planner";
pub const INBOX: &str = "inbox";
pub const PLANNER: &str = "planner";

/// Every kind of span, in the order `stats` lists them.
pub const KINDS: [&str; 10] = [
    WORKER,
    RESUME,
    REVISE,
    REVIEW,
    TRIAGE,
    OBSERVER,
    PLAN_REVIEW,
    RUNTIME_PLANNER,
    INBOX,
    PLANNER,
];

/// The kinds of a run's own session: the worker's, a resume's, and the
/// revise it is sent back to.
const RUN_SESSION: [&str; 3] = [WORKER, RESUME, REVISE];

/// Why a span was closed.
pub const EXITED: &str = "exited";
pub const JOB_FINISHED: &str = "job_finished";
pub const NEXT_SPAN: &str = "next_span";
/// No event ended it: it was closed when the runtime found it had ended.
pub const INFERRED: &str = "inferred";

/// Where the events that may open or close spans are recorded, which says
/// which open spans they are about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// An event of a run: its spans.
    Run,
    /// An event of a proposal's plan review (on its first task): the spans
    /// of that proposal.
    Proposal,
    /// An event of the queue itself: the observer's spans.
    Queue,
}

/// The scope of an event of `kind`, when it may open or close a span.
pub fn scope(kind: &str) -> Option<Scope> {
    match kind {
        "agent_started" | "revise_requested" | "session_exited" | "workspace_closed"
        | "run_recovered" | "review_started" | "review_finished" | "review_failed"
        | "review_retried" | "triage_started" | "triage_finished" | "triage_failed" => {
            Some(Scope::Run)
        }
        "plan_review_started" | "plan_review_finished" | "plan_review_failed" => {
            Some(Scope::Proposal)
        }
        "observe_started" | "observe_finished" => Some(Scope::Queue),
        _ => None,
    }
}

/// A span that is open: its `session_opened` event and payload.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenSpan {
    pub opened_event_id: EventId,
    pub payload: Value,
}

impl OpenSpan {
    pub fn kind(&self) -> &str {
        self.payload["kind"].as_str().unwrap_or_default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.payload["session_id"].as_str()
    }
}

/// What the runtime knows of the run or proposal an event is about, beyond
/// the event itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanContext {
    /// The run's worktree, where its sessions and its review run.
    pub worktree: Option<String>,
    /// The run's directory, where its triage runs.
    pub run_dir: Option<String>,
    /// The run's worker workspace.
    pub workspace_id: Option<String>,
    /// The `resume_started` events the run has, this one's included.
    pub resumes: i64,
    /// The `revise_requested` events the run has, this one's included.
    pub revises: i64,
    /// The goals of the proposal's tasks (a plan review's), ascending.
    pub goal_ids: Vec<i64>,
}

/// A span to write: `Close` one that is open, or `Open` a new one with the
/// payload of its `session_opened`.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanChange {
    Close {
        span: OpenSpan,
        reason: &'static str,
    },
    Open(Value),
}

impl SpanChange {
    /// The payload of the `session_closed` of a `Close`.
    pub fn closed_payload(span: &OpenSpan, reason: &str) -> Value {
        json!({
            "opened_event_id": span.opened_event_id,
            "kind": span.kind(),
            "session_id": span.session_id(),
            "reason": reason,
        })
    }
}

/// The spans an event of `kind` with `payload` closes and opens, given the
/// spans open in its scope (oldest first) and what `context` knows. Closes
/// come first. An event that is not about spans changes none.
pub fn changes(
    kind: &str,
    payload: &Value,
    open: &[OpenSpan],
    context: &SpanContext,
) -> Vec<SpanChange> {
    let close = |kinds: &[&str], reason: &'static str| {
        open.iter()
            .filter(|span| kinds.contains(&span.kind()))
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason,
            })
            .collect::<Vec<_>>()
    };
    let text = |key: &str| payload.get(key).and_then(Value::as_str);
    match kind {
        // A session starting while another is still recorded open: that
        // one ended without a `session_exited` (ADR-0048 decision 7).
        "agent_started" => {
            let mut changes = close(&RUN_SESSION, INFERRED);
            let (span, attempt) = if context.resumes > 0 {
                (RESUME, context.resumes)
            } else {
                (WORKER, 1)
            };
            changes.push(SpanChange::Open(json!({
                "kind": span,
                "session_id": text("session_id"),
                "cwd": context.worktree,
                "transcript_path": null,
                "attempt": attempt,
                "workspace_id": (span == WORKER).then_some(&context.workspace_id),
            })));
            changes
        }
        // The same session goes on as a revise.
        "revise_requested" => {
            let mut changes = close(&RUN_SESSION, NEXT_SPAN);
            let session_id = open
                .iter()
                .rev()
                .find(|span| RUN_SESSION.contains(&span.kind()))
                .and_then(OpenSpan::session_id)
                .map(str::to_owned);
            changes.push(SpanChange::Open(json!({
                "kind": REVISE,
                "session_id": session_id,
                "cwd": context.worktree,
                "transcript_path": null,
                "attempt": context.revises,
                "workspace_id": text("workspace_id"),
            })));
            changes
        }
        "session_exited" => close(&RUN_SESSION, EXITED),
        // The session went with its workspace, or the run was recovered.
        "workspace_closed" => close(&RUN_SESSION, INFERRED),
        // A recovered run's review died with its supervisor too.
        "run_recovered" => close(&[WORKER, RESUME, REVISE, REVIEW], INFERRED),
        "review_started" => {
            let mut changes = close(&[REVIEW], INFERRED);
            changes.push(job(REVIEW, payload, context.worktree.as_deref()));
            changes
        }
        "review_finished" | "review_failed" | "review_retried" => close(&[REVIEW], JOB_FINISHED),
        // A run is triaged once its session is gone.
        "triage_started" => {
            let mut changes = close(&[WORKER, RESUME, REVISE, REVIEW, TRIAGE], INFERRED);
            changes.push(job(TRIAGE, payload, context.run_dir.as_deref()));
            changes
        }
        "triage_finished" | "triage_failed" => close(&[TRIAGE], JOB_FINISHED),
        "plan_review_started" => {
            let mut changes = close(&[PLAN_REVIEW], INFERRED);
            let mut opened = job(PLAN_REVIEW, payload, text("cwd"));
            if let SpanChange::Open(opened) = &mut opened {
                opened["proposal_id"] = payload["proposal_id"].clone();
                opened["plan_review_id"] = payload["plan_review_id"].clone();
                opened["goal_ids"] = json!(context.goal_ids);
            }
            changes.push(opened);
            changes
        }
        "plan_review_finished" | "plan_review_failed" => open
            .iter()
            .filter(|span| {
                span.kind() == PLAN_REVIEW
                    && span.payload["plan_review_id"] == payload["plan_review_id"]
            })
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason: JOB_FINISHED,
            })
            .collect(),
        // An observer that died with its supervisor recorded no finish.
        "observe_started" => {
            let mut changes = close(&[OBSERVER], INFERRED);
            changes.push(job(OBSERVER, payload, text("dir")));
            changes
        }
        "observe_finished" => open
            .iter()
            .filter(|span| span.kind() == OBSERVER && span.payload["cwd"] == payload["dir"])
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason: JOB_FINISHED,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The span of a headless job, whose session id the runtime gave it
/// (ADR-0048 decision 4) and recorded in the event that starts it.
fn job(kind: &str, payload: &Value, cwd: Option<&str>) -> SpanChange {
    SpanChange::Open(json!({
        "kind": kind,
        "session_id": payload.get("session_id").and_then(Value::as_str),
        "cwd": cwd,
        "transcript_path": null,
        "attempt": payload.get("attempt").and_then(Value::as_i64),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: i64, payload: Value) -> OpenSpan {
        OpenSpan {
            opened_event_id: EventId::new(id),
            payload,
        }
    }

    fn opened(change: &SpanChange) -> &Value {
        match change {
            SpanChange::Open(payload) => payload,
            SpanChange::Close { .. } => panic!("expected an open, got {change:?}"),
        }
    }

    fn closed(change: &SpanChange) -> (i64, &'static str) {
        match change {
            SpanChange::Close { span, reason } => (span.opened_event_id.as_i64(), *reason),
            SpanChange::Open(_) => panic!("expected a close, got {change:?}"),
        }
    }

    #[test]
    fn scopes_cover_the_starts_and_ends_of_every_recorded_kind() {
        assert_eq!(scope("agent_started"), Some(Scope::Run));
        assert_eq!(scope("triage_failed"), Some(Scope::Run));
        assert_eq!(scope("plan_review_failed"), Some(Scope::Proposal));
        assert_eq!(scope("observe_finished"), Some(Scope::Queue));
        assert_eq!(scope("run_claimed"), None);
        assert_eq!(scope(SESSION_OPENED), None);
        assert_eq!(KINDS.len(), 10);
    }

    /// The worker's session opens with the run's id, goes on as a revise,
    /// and ends at its exit.
    #[test]
    fn a_worker_session_opens_switches_to_revise_and_exits() {
        let context = SpanContext {
            worktree: Some("/wt".into()),
            workspace_id: Some("W".into()),
            ..SpanContext::default()
        };
        let started = changes(
            "agent_started",
            &json!({"pid": 1, "session_id": "run-1"}),
            &[],
            &context,
        );
        assert_eq!(started.len(), 1);
        let payload = opened(&started[0]);
        assert_eq!(payload["kind"], WORKER);
        assert_eq!(payload["session_id"], "run-1");
        assert_eq!(payload["cwd"], "/wt");
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["workspace_id"], "W");

        let worker = span(10, payload.clone());
        let revise_context = SpanContext {
            revises: 1,
            ..context.clone()
        };
        let revised = changes(
            "revise_requested",
            &json!({"workspace_id": "W"}),
            std::slice::from_ref(&worker),
            &revise_context,
        );
        assert_eq!(closed(&revised[0]), (10, NEXT_SPAN));
        let payload = opened(&revised[1]);
        assert_eq!(payload["kind"], REVISE);
        assert_eq!(payload["session_id"], "run-1");
        assert_eq!(payload["attempt"], 1);

        let revise = span(12, payload.clone());
        let exited = changes("session_exited", &json!({}), &[revise], &context);
        assert_eq!(exited.len(), 1);
        assert_eq!(closed(&exited[0]), (12, EXITED));
        let closed_payload = SpanChange::closed_payload(&worker, EXITED);
        assert_eq!(closed_payload["opened_event_id"], 10);
        assert_eq!(closed_payload["kind"], WORKER);
        assert_eq!(closed_payload["session_id"], "run-1");
    }

    /// A resume's session is `resume`; one still open when the next starts
    /// is closed as inferred.
    #[test]
    fn a_resume_opens_its_own_span_and_infers_the_end_of_a_lost_one() {
        let context = SpanContext {
            resumes: 2,
            ..SpanContext::default()
        };
        let lost = span(5, json!({"kind": WORKER, "session_id": "r"}));
        let started = changes(
            "agent_started",
            &json!({"session_id": "r"}),
            &[lost],
            &context,
        );
        assert_eq!(closed(&started[0]), (5, INFERRED));
        let payload = opened(&started[1]);
        assert_eq!(payload["kind"], RESUME);
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["workspace_id"], Value::Null);
        for (kind, closes) in [("workspace_closed", 1), ("run_recovered", 2)] {
            let open = span(7, json!({"kind": RESUME}));
            let review = span(8, json!({"kind": REVIEW}));
            let changed = changes(kind, &json!({}), &[open, review], &context);
            assert_eq!(changed.len(), closes, "{kind}");
            assert_eq!(closed(&changed[0]), (7, INFERRED));
        }
    }

    /// Headless jobs open with the session id they were given and close at
    /// their finish; one restarted without a finish is inferred.
    #[test]
    fn jobs_open_with_their_session_id_and_close_at_their_finish() {
        let context = SpanContext {
            worktree: Some("/wt".into()),
            run_dir: Some("/run".into()),
            ..SpanContext::default()
        };
        let review = changes(
            "review_started",
            &json!({"attempt": 2, "session_id": "s-review"}),
            &[span(1, json!({"kind": REVIEW}))],
            &context,
        );
        assert_eq!(closed(&review[0]), (1, INFERRED));
        let payload = opened(&review[1]);
        assert_eq!(payload["kind"], REVIEW);
        assert_eq!(payload["session_id"], "s-review");
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["cwd"], "/wt");
        for kind in ["review_finished", "review_failed", "review_retried"] {
            let changed = changes(kind, &json!({}), &[span(3, payload.clone())], &context);
            assert_eq!(closed(&changed[0]), (3, JOB_FINISHED), "{kind}");
        }

        let triage = changes(
            "triage_started",
            &json!({"attempt": 1, "session_id": "s-triage"}),
            &[span(4, json!({"kind": WORKER}))],
            &context,
        );
        assert_eq!(closed(&triage[0]), (4, INFERRED));
        let payload = opened(&triage[1]);
        assert_eq!(payload["kind"], TRIAGE);
        assert_eq!(payload["cwd"], "/run");
        for kind in ["triage_finished", "triage_failed"] {
            let changed = changes(kind, &json!({}), &[span(6, payload.clone())], &context);
            assert_eq!(closed(&changed[0]), (6, JOB_FINISHED), "{kind}");
        }

        let plan_context = SpanContext {
            goal_ids: vec![3, 4],
            ..SpanContext::default()
        };
        let plan = changes(
            "plan_review_started",
            &json!({"proposal_id": 9, "plan_review_id": 2, "attempt": 1, "session_id": "s-plan"}),
            &[],
            &plan_context,
        );
        let payload = opened(&plan[0]);
        assert_eq!(payload["kind"], PLAN_REVIEW);
        assert_eq!(payload["proposal_id"], 9);
        assert_eq!(payload["plan_review_id"], 2);
        assert_eq!(payload["goal_ids"], json!([3, 4]));
        let other = span(8, json!({"kind": PLAN_REVIEW, "plan_review_id": 1}));
        let finished = changes(
            "plan_review_failed",
            &json!({"plan_review_id": 2}),
            &[other, span(9, payload.clone())],
            &plan_context,
        );
        assert_eq!(finished.len(), 1);
        assert_eq!(closed(&finished[0]), (9, JOB_FINISHED));

        let observe = changes(
            "observe_started",
            &json!({"mode": "hourly", "dir": "/obs/1", "session_id": "s-obs"}),
            &[span(10, json!({"kind": OBSERVER, "cwd": "/obs/0"}))],
            &SpanContext::default(),
        );
        assert_eq!(closed(&observe[0]), (10, INFERRED));
        let payload = opened(&observe[1]);
        assert_eq!(payload["cwd"], "/obs/1");
        let finished = changes(
            "observe_finished",
            &json!({"dir": "/obs/1"}),
            &[span(11, payload.clone())],
            &SpanContext::default(),
        );
        assert_eq!(closed(&finished[0]), (11, JOB_FINISHED));
        assert!(changes("run_claimed", &json!({}), &[], &context).is_empty());
    }
}
