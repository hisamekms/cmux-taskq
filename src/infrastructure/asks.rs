//! Asks (ADR-0022): questions for a person kept as queue rows. Registering
//! and answering one also writes `ask_opened` / `ask_answered` to
//! `run_events`, so the change rides the cursor `status` and `watch` hand out.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, enum_col, json_col};
use crate::domain::{
    Ask, AskKind, AskOutcome, LANDING_OPTIONS, NewAsk, RunStatus, SessionRole, TRIAGE_OPTIONS,
};

/// Which asks `asks` lists. By default the ones nobody closed; `all` adds
/// the closed ones, `open` keeps only the unanswered ones, and `role` keeps
/// those that wait for that role ([`Ask::waits_for`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AskQuery {
    pub all: bool,
    pub open: bool,
    pub role: Option<SessionRole>,
}

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
        let task_id = match (&ask.run_id, ask.task_id) {
            (Some(run_id), _) => tx
                .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get::<_, i64>(0)
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
        if let Some(existing) = tx
            .query_row(
                "SELECT * FROM asks WHERE ifnull(task_id,0)=ifnull(?1,0) AND ifnull(run_id,'')=ifnull(?2,'')
                 AND kind=?3 AND answered_at IS NULL AND closed_at IS NULL",
                params![task_id, ask.run_id, ask.kind.as_str()],
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
            "INSERT INTO asks(kind,task_id,run_id,question,options,asked_by)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                ask.kind.as_str(),
                task_id,
                ask.run_id,
                ask.question,
                serde_json::to_string(&ask.options)?,
                ask.asked_by
            ],
        )?;
        let id = tx.last_insert_rowid();
        ask_event(
            &tx,
            task_id,
            ask.run_id.as_deref(),
            "ask_opened",
            json!({"ask_id": id, "kind": ask.kind, "asked_by": ask.asked_by}),
        )?;
        let created = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(AskOutcome {
            ask: created,
            created: true,
        })
    }

    /// Write the answer of an open ask and record `ask_answered` (with the
    /// run when the ask has one).
    pub fn answer(&mut self, id: i64, text: &str) -> Result<Ask> {
        ensure!(!text.trim().is_empty(), "answer must not be blank");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.is_open(), "ask {id} is not open");
        tx.execute(
            "UPDATE asks SET answer=?2, answered_at=unixepoch() WHERE id=?1",
            params![id, text],
        )?;
        let mut payload = json!({"ask_id": id, "kind": ask.kind});
        if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_deref()
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
            && let Some(run_id) = ask.run_id.as_deref()
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
            && let Some(run_id) = ask.run_id.as_deref()
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
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_deref(),
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
    pub fn close_ask(&mut self, id: i64) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.closed_at.is_none(), "ask {id} is already closed");
        ensure!(
            ask.answered_at.is_some(),
            "ask {id} is not answered yet; answer it (for example that it is withdrawn) before closing it"
        );
        tx.execute("UPDATE asks SET closed_at=unixepoch() WHERE id=?1", [id])?;
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

    /// The answered `worker_question` asks of a run that nobody closed yet,
    /// oldest first: answers the supervisor still has to type into the
    /// worker's terminal.
    pub fn undelivered_answers(&self, run_id: &str) -> Result<Vec<Ask>> {
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
    pub fn has_unclosed_worker_question(&self, run_id: &str) -> Result<bool> {
        self.has_unclosed_ask(run_id, AskKind::WorkerQuestion)
    }

    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    pub fn has_unclosed_ask(&self, run_id: &str, kind: AskKind) -> Result<bool> {
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
    pub fn ask_delivered(&mut self, id: i64, workspace_id: &str) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        if ask.closed_at.is_some() {
            return Ok(ask);
        }
        ensure!(ask.answered_at.is_some(), "ask {id} is not answered");
        tx.execute("UPDATE asks SET closed_at=unixepoch() WHERE id=?1", [id])?;
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_deref(),
            "ask_delivered",
            json!({"ask_id": id, "workspace_id": workspace_id}),
        )?;
        let closed = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Whether the run ever had a `stuck_exit` ask, closed or not.
    pub fn has_stuck_exit_ask(&self, run_id: &str) -> Result<bool> {
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
    pub fn close_stuck_exit_asks(&mut self, run_id: &str, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::StuckExit, answer)
    }

    /// Close every `answer_prompt` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: the dialog it was about is gone
    /// (or the session ended), so nobody needs to answer it any more.
    pub fn close_answer_prompt_asks(&mut self, run_id: &str, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::AnswerPrompt, answer)
    }

    fn close_runtime_asks(
        &mut self,
        run_id: &str,
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
        let mut closed = Vec::with_capacity(unclosed.len());
        for ask in unclosed {
            if ask.is_open() {
                tx.execute(
                    "UPDATE asks SET answer=?2, answered_at=unixepoch() WHERE id=?1",
                    params![ask.id, answer],
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
                "UPDATE asks SET closed_at=unixepoch() WHERE id=?1",
                [ask.id],
            )?;
            closed.push(read_ask(&tx, ask.id)?);
        }
        tx.commit()?;
        Ok(closed)
    }

    pub fn read_ask(&self, id: i64) -> Result<Ask> {
        read_ask(&self.conn, id)
    }
}

/// An ask's event: on its task (and run), or, for a task-less `blocked`
/// ask, on nothing.
fn ask_event(
    conn: &Connection,
    task_id: Option<i64>,
    run_id: Option<&str>,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,?2,?3,?4)",
        params![task_id, run_id, kind, serde_json::to_string(&payload)?],
    )?;
    Ok(())
}

fn read_ask(conn: &Connection, id: i64) -> Result<Ask> {
    conn.query_row("SELECT * FROM asks WHERE id=?1", [id], ask_row)
        .optional()?
        .with_context(|| format!("ask {id} does not exist"))
}

fn ask_row(row: &Row<'_>) -> rusqlite::Result<Ask> {
    Ok(Ask {
        id: row.get("id")?,
        kind: enum_col(row, "kind")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        question: row.get("question")?,
        options: json_col(row, "options")?,
        answer: row.get("answer")?,
        asked_by: row.get("asked_by")?,
        created_at: row.get("created_at")?,
        answered_at: row.get("answered_at")?,
        closed_at: row.get("closed_at")?,
    })
}
