//! Asks (ADR-0022): questions for a person kept as queue rows. Registering
//! and answering one also writes `ask_opened` / `ask_answered` to
//! `run_events`, so the change rides the cursor `status` and `watch` hand out.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, enum_col, json_col};
use crate::domain::Ask;
use crate::domain::{
    AskId, AskKind, AskOutcome, AskReason, HOLD_AFFECTED_HEADING, HoldOutcome, LANDING_OPTIONS,
    NewAsk, NewHold, RunId, RunStatus, TRIAGE_OPTIONS, TaskId, UPDATE_FAILED_OPTIONS,
    UPDATE_FAILED_SUBJECT,
};

pub use crate::application::AskQuery;

impl SqliteQueue {
    /// Register an ask, or return the open one of the same task, run and
    /// kind unchanged. A new ask writes `ask_opened` (with the run when it
    /// has one) in the same transaction. A `blocked` ask may name neither a
    /// task nor a run: the observer's threshold that belongs to no task.
    pub fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome> {
        ask.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = insert_ask(&tx, &ask)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Open the `queue_hold` ask of the hold's reason and subject with its
    /// run, or add the run to the open one (ADR-0047 decision 42). A new
    /// ask writes `ask_opened` on the queue; a run that joins an open one
    /// rewrites its question's list of runs and writes `ask_updated` on
    /// that run. A run already in it changes nothing.
    pub fn hold(&mut self, hold: NewHold) -> Result<HoldOutcome> {
        hold.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_id: TaskId = tx
            .query_row(
                "SELECT task_id FROM task_runs WHERE id=?1",
                [&hold.run_id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("run {} does not exist", hold.run_id))?;
        let open = tx
            .query_row(
                "SELECT * FROM asks WHERE kind='queue_hold' AND reason_category=?1
                 AND ifnull(subject,'')=ifnull(?2,'')
                 AND answered_at IS NULL AND closed_at IS NULL",
                params![hold.reason_category.as_str(), hold.subject],
                ask_row,
            )
            .optional()?;
        let run = hold.run_id.as_str().to_owned();
        let outcome = match open {
            Some(ask) if ask.affected.contains(&run) => HoldOutcome {
                ask,
                created: false,
                joined: false,
            },
            Some(ask) => {
                let mut affected = ask.affected.clone();
                affected.push(run);
                let base = ask
                    .question
                    .rsplit_once(&format!("\n\n{HOLD_AFFECTED_HEADING}"))
                    .map_or(ask.question.as_str(), |(base, _)| base);
                tx.execute(
                    "UPDATE asks SET affected=?2, question=?3 WHERE id=?1",
                    params![
                        ask.id,
                        serde_json::to_string(&affected)?,
                        NewHold::question_for(base, &affected)
                    ],
                )?;
                ask_event(
                    &tx,
                    Some(task_id),
                    Some(&hold.run_id),
                    "ask_updated",
                    json!({
                        "ask_id": ask.id,
                        "kind": ask.kind,
                        "reason_category": ask.reason_category,
                        "affected": affected,
                    }),
                )?;
                HoldOutcome {
                    ask: read_ask(&tx, ask.id)?,
                    created: false,
                    joined: true,
                }
            }
            None => {
                let affected = vec![run];
                tx.execute(
                    "INSERT INTO asks(kind,question,options,asked_by,reason_category,subject,affected)
                     VALUES ('queue_hold',?1,?2,?3,?4,?5,?6)",
                    params![
                        NewHold::question_for(&hold.question, &affected),
                        serde_json::to_string(&hold.options)?,
                        hold.asked_by,
                        hold.reason_category.as_str(),
                        hold.subject,
                        serde_json::to_string(&affected)?,
                    ],
                )?;
                let id = AskId::new(tx.last_insert_rowid());
                ask_event(
                    &tx,
                    None,
                    None,
                    "ask_opened",
                    json!({
                        "ask_id": id,
                        "kind": AskKind::QueueHold,
                        "asked_by": hold.asked_by,
                        "reason_category": hold.reason_category,
                        "affected": affected,
                    }),
                )?;
                HoldOutcome {
                    ask: read_ask(&tx, id)?,
                    created: true,
                    joined: true,
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The open `queue_hold` ask that holds the run, if any.
    pub fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM asks WHERE kind='queue_hold'
                 AND answered_at IS NULL AND closed_at IS NULL
                 AND EXISTS (SELECT 1 FROM json_each(asks.affected) WHERE value=?1)
                 ORDER BY id LIMIT 1",
                [run_id],
                ask_row,
            )
            .optional()?)
    }

    /// Write the answer of an open ask and record `ask_answered` (with the
    /// run when the ask has one).
    pub fn answer(&mut self, id: AskId, text: &str) -> Result<Ask> {
        ensure!(!text.trim().is_empty(), "answer must not be blank");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.is_open(), "ask {id} is not open");
        tx.execute(
            "UPDATE asks SET answer=?2, answered_at=?3 WHERE id=?1",
            params![id, text, self.generators.clock.now()],
        )?;
        let mut payload =
            json!({"ask_id": id, "kind": ask.kind, "reason_category": ask.reason_category});
        if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor types it into a running worker's terminal; the
            // answer of a run that stopped running is the inbox's.
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            payload["runtime_delivers"] = json!(status == RunStatus::Running.as_str());
        }
        if ask.kind == AskKind::ApproveLanding
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor lands, sends back or cancels a run awaiting
            // integration as answered (ADR-0027); any other answer, or one
            // for a run that moved on, is the inbox's to read.
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            payload["runtime_delivers"] = json!(
                status == RunStatus::AwaitingIntegration.as_str()
                    && LANDING_OPTIONS.contains(&text.trim())
            );
        }
        if ask.kind == AskKind::Decide
            && ask.asked_by == super::runtime_store::TRIAGE_ASKER
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor retries, resumes or cancels a triaged run as
            // answered (ADR-0024 decision 3); any other answer, or one for
            // a run that moved on, is a person's to read.
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            payload["runtime_delivers"] = json!(
                (status == RunStatus::Failed.as_str() || status == RunStatus::Interrupted.as_str())
                    && TRIAGE_OPTIONS.contains(&text.trim())
                    && ask.options.iter().any(|option| option == text.trim())
            );
        }
        if ask.kind == AskKind::PlannerQuestion {
            // The supervisor types it into the workspace of the runtime's
            // planner that works on its task, or opens one for a draft that
            // still waits (ADR-0041 decision 13); otherwise it is the
            // inbox's to deliver.
            let answered = Ask {
                answer: Some(text.to_owned()),
                ..ask.clone()
            };
            payload["runtime_delivers"] = json!(
                super::draft_planners::route_of(&tx, &answered)?
                    != crate::application::PlannerAnswerRoute::Person
            );
        }
        if ask.kind == AskKind::ApprovePlan {
            // The supervisor readies, sends back or cancels the proposal as
            // answered (ADR-0041 decision 11); any other answer, or one for
            // a proposal that moved on, is a person's to read.
            payload["runtime_delivers"] =
                json!(super::plan_reviews::plan_answer_applies(&tx, &ask, text)?);
        }
        if ask.kind == AskKind::Blocked && ask.subject.as_deref() == Some(UPDATE_FAILED_SUBJECT) {
            // The supervisor that updates the binary retries or leaves the
            // update as answered (ADR-0045 decision 17); any other answer
            // is a person's to read.
            payload["runtime_delivers"] = json!(UPDATE_FAILED_OPTIONS.contains(&text.trim()));
        }
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_ref(),
            "ask_answered",
            payload,
        )?;
        let answered = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(answered)
    }

    /// Mark an answered ask read (by the inbox, once the person acted on it). Writes no event. An
    /// open ask cannot be closed: `ask_answered` is the one event that ends
    /// an ask in `run_events` (what `stats` pairs with `ask_opened`), so an
    /// ask is withdrawn by answering it.
    pub fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.closed_at.is_none(), "ask {id} is already closed");
        ensure!(
            ask.answered_at.is_some(),
            "ask {id} is not answered yet; answer it (for example that it is withdrawn) before closing it"
        );
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![id, self.generators.clock.now()],
        )?;
        let closed = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Asks matching `query`, oldest first. A pure read.
    pub fn asks(&self, query: AskQuery) -> Result<Vec<Ask>> {
        let asks: Vec<Ask> = self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE (?1 OR closed_at IS NULL)
                 AND (NOT ?2 OR answered_at IS NULL) ORDER BY id",
            )?
            .query_map(params![query.all, query.open], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(asks
            .into_iter()
            .filter(|ask| query.role.is_none() || ask.waits_for() == query.role)
            .collect())
    }

    /// Open the task-less `blocked` ask of the automatic update with this
    /// `subject` (ADR-0045 decision 17): `update_failed` or
    /// `approve_update`. One of the same subject still open is about an
    /// older build, so it is answered `superseded` and closed first (by the
    /// runtime, which writes `ask_answered` with `runtime_closed`). Writes
    /// `ask_opened` like any ask.
    pub fn open_update_ask(
        &mut self,
        subject: &str,
        question: &str,
        options: &[&str],
        asked_by: &str,
    ) -> Result<Ask> {
        ensure!(!question.trim().is_empty(), "question must not be blank");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let open: Vec<Ask> = tx
            .prepare(
                "SELECT * FROM asks WHERE kind='blocked' AND task_id IS NULL AND subject=?1
                 AND answered_at IS NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([subject], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        for ask in open {
            tx.execute(
                "UPDATE asks SET answer='superseded', answered_at=?2, closed_at=?2 WHERE id=?1",
                params![ask.id, now],
            )?;
            ask_event(
                &tx,
                None,
                None,
                "ask_answered",
                json!({"ask_id": ask.id, "kind": ask.kind, "runtime_closed": true}),
            )?;
        }
        let reason = AskReason::Scope;
        tx.execute(
            "INSERT INTO asks(kind,question,options,asked_by,reason_category,subject)
             VALUES ('blocked',?1,?2,?3,?4,?5)",
            params![
                question,
                serde_json::to_string(options)?,
                asked_by,
                reason.as_str(),
                subject
            ],
        )?;
        let id = AskId::new(tx.last_insert_rowid());
        ask_event(
            &tx,
            None,
            None,
            "ask_opened",
            json!({
                "ask_id": id,
                "kind": AskKind::Blocked,
                "asked_by": asked_by,
                "reason_category": reason,
                "subject": subject,
            }),
        )?;
        let opened = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(opened)
    }

    /// The task-less `blocked` asks of the automatic update with this
    /// `subject` that were answered and nobody closed yet, oldest first:
    /// answers the supervisor still has to apply.
    pub fn update_answers(&self, subject: &str) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE kind='blocked' AND task_id IS NULL AND subject=?1
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([subject], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The answered `worker_question` asks of a run that nobody closed yet,
    /// oldest first: answers the supervisor still has to type into the
    /// worker's terminal.
    pub fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind='worker_question'
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([run_id], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the run has a `worker_question` nobody closed, answered or
    /// not: its worker stopped at the ask and waits for the answer.
    pub fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        self.has_unclosed_ask(run_id, AskKind::WorkerQuestion)
    }

    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    pub fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE run_id=?1 AND kind=?2
             AND closed_at IS NULL)",
            params![run_id, kind.as_str()],
            |r| r.get(0),
        )?)
    }

    /// The answered `approve_landing` asks about a run that nobody closed,
    /// oldest first: the answers the supervisor applies (ADR-0027).
    pub fn landing_answers(&self) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE kind='approve_landing' AND run_id IS NOT NULL
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Close an answered `worker_question` whose answer was typed into the
    /// worker's terminal, and record `ask_delivered` in the same transaction.
    /// An ask someone closed meanwhile is left as it is.
    pub fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        if ask.closed_at.is_some() {
            return Ok(ask);
        }
        ensure!(ask.answered_at.is_some(), "ask {id} is not answered");
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![id, self.generators.clock.now()],
        )?;
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_ref(),
            "ask_delivered",
            json!({"ask_id": id, "workspace_id": workspace_id}),
        )?;
        let closed = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Whether the run ever had a `stuck_exit` ask, closed or not.
    pub fn has_stuck_exit_ask(&self, run_id: &RunId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE run_id=?1 AND kind='stuck_exit')",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// Close every `stuck_exit` ask of the run nobody closed: its session
    /// exited, so nobody needs to answer it any more. An open one is
    /// answered with `answer` first and records `ask_answered` with
    /// `runtime_closed: true` (the one event that ends an ask, which is
    /// no attention); an answered one is only closed, like `ask close`.
    /// Returns the asks it closed, oldest first.
    pub fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::StuckExit, answer)
    }

    /// Close every `answer_prompt` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: the dialog it was about is gone
    /// (or the session ended), so nobody needs to answer it any more.
    pub fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::AnswerPrompt, answer)
    }

    /// The run's `stalled` ask nobody closed, answered or not: at most one
    /// is open, and an answered one the supervisor has not applied yet
    /// comes first (ADR-0043 decision 1).
    pub fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind='stalled' AND closed_at IS NULL
                 ORDER BY id LIMIT 1",
            )?
            .query_map([run_id], ask_row)?
            .next()
            .transpose()?)
    }

    /// Close every `stalled` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: the session moved on or ended,
    /// so nobody needs to answer it any more.
    pub fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::Stalled, answer)
    }

    /// Close every `approve_landing` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: a later review of the run
    /// failed and asks afresh (task 328), so an earlier question, or an
    /// answer to it not applied yet, no longer fits the run.
    pub fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::ApproveLanding, answer)
    }

    fn close_runtime_asks(
        &mut self,
        run_id: &RunId,
        kind: AskKind,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unclosed: Vec<Ask> = tx
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind=?2
                 AND closed_at IS NULL ORDER BY id",
            )?
            .query_map(params![run_id, kind.as_str()], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        let now = self.generators.clock.now();
        let mut closed = Vec::with_capacity(unclosed.len());
        for ask in unclosed {
            if ask.is_open() {
                tx.execute(
                    "UPDATE asks SET answer=?2, answered_at=?3 WHERE id=?1",
                    params![ask.id, answer, now],
                )?;
                ask_event(
                    &tx,
                    ask.task_id,
                    Some(run_id),
                    "ask_answered",
                    json!({"ask_id": ask.id, "kind": ask.kind, "runtime_closed": true}),
                )?;
            }
            tx.execute(
                "UPDATE asks SET closed_at=?2 WHERE id=?1",
                params![ask.id, now],
            )?;
            closed.push(read_ask(&tx, ask.id)?);
        }
        tx.commit()?;
        Ok(closed)
    }

    pub fn read_ask(&self, id: AskId) -> Result<Ask> {
        read_ask(&self.conn, id)
    }
}

/// Register `ask` inside the caller's write transaction, or return the open
/// one of the same task, run and kind unchanged (see [`SqliteQueue::ask`]).
pub(super) fn insert_ask(tx: &Connection, ask: &NewAsk) -> Result<AskOutcome> {
    let task_id = match (&ask.run_id, ask.task_id) {
        (Some(run_id), _) => tx
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                r.get::<_, TaskId>(0)
            })
            .optional()?
            .with_context(|| format!("run {run_id} does not exist"))
            .map(Some)?,
        (None, Some(task_id)) => {
            ensure!(
                tx.query_row("SELECT count(*) FROM tasks WHERE id=?1", [task_id], |r| r
                    .get::<_, i64>(
                    0
                ))? == 1,
                "task {task_id} does not exist"
            );
            Some(task_id)
        }
        // `validate` admits this for a blocked ask only.
        (None, None) => None,
    };
    if let Some(finding_id) = ask.finding_id {
        super::findings::read_finding(tx, finding_id)?;
    }
    if let Some(existing) = tx
            .query_row(
                "SELECT * FROM asks WHERE ifnull(task_id,0)=ifnull(?1,0) AND ifnull(run_id,'')=ifnull(?2,'')
                 AND kind=?3 AND ifnull(finding_id,0)=ifnull(?4,0)
                 AND answered_at IS NULL AND closed_at IS NULL",
                params![task_id, ask.run_id, ask.kind.as_str(), ask.finding_id],
                ask_row,
            )
            .optional()?
        {
            return Ok(AskOutcome {
                ask: existing,
                created: false,
            });
        }
    tx.execute(
        "INSERT INTO asks(kind,task_id,run_id,question,options,asked_by,reason_category,finding_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            ask.kind.as_str(),
            task_id,
            ask.run_id,
            ask.question,
            serde_json::to_string(&ask.options)?,
            ask.asked_by,
            ask.reason_category.as_str(),
            ask.finding_id
        ],
    )?;
    let id = AskId::new(tx.last_insert_rowid());
    ask_event(
        tx,
        task_id,
        ask.run_id.as_ref(),
        "ask_opened",
        json!({
            "ask_id": id,
            "kind": ask.kind,
            "asked_by": ask.asked_by,
            "reason_category": ask.reason_category,
        }),
    )?;
    let created = read_ask(tx, id)?;
    Ok(AskOutcome {
        ask: created,
        created: true,
    })
}

/// An ask's event: on its task (and run), or, for a task-less `blocked`
/// ask, on nothing.
fn ask_event(
    conn: &Connection,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,?2,?3,?4)",
        params![task_id, run_id, kind, serde_json::to_string(&payload)?],
    )?;
    Ok(())
}

pub(super) fn read_ask(conn: &Connection, id: AskId) -> Result<Ask> {
    conn.query_row("SELECT * FROM asks WHERE id=?1", [id], ask_row)
        .optional()?
        .with_context(|| format!("ask {id} does not exist"))
}

pub(super) fn ask_row(row: &Row<'_>) -> rusqlite::Result<Ask> {
    Ok(Ask {
        id: row.get("id")?,
        kind: enum_col(row, "kind")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        question: row.get("question")?,
        options: json_col(row, "options")?,
        answer: row.get("answer")?,
        asked_by: row.get("asked_by")?,
        reason_category: enum_col(row, "reason_category")?,
        subject: row.get("subject")?,
        affected: json_col(row, "affected")?,
        created_at: row.get("created_at")?,
        answered_at: row.get("answered_at")?,
        closed_at: row.get("closed_at")?,
        finding_id: row.get("finding_id")?,
    })
}
