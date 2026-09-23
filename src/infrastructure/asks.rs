//! Asks (ADR-0022): questions for a person kept as queue rows. Registering
//! and answering one also writes `ask_opened` / `ask_answered` to
//! `run_events`, so the change rides the cursor `status` and `watch` hand out.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, enum_col, event, json_col};
use crate::domain::{Ask, AskOutcome, NewAsk, SessionRole};

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
    /// has one) in the same transaction.
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
                .with_context(|| format!("run {run_id} does not exist"))?,
            (None, Some(task_id)) => {
                ensure!(
                    tx.query_row("SELECT count(*) FROM tasks WHERE id=?1", [task_id], |r| r
                        .get::<_, i64>(
                        0
                    ))? == 1,
                    "task {task_id} does not exist"
                );
                task_id
            }
            (None, None) => anyhow::bail!("an ask needs a task or a run"),
        };
        if let Some(existing) = tx
            .query_row(
                "SELECT * FROM asks WHERE task_id=?1 AND ifnull(run_id,'')=ifnull(?2,'')
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
        event(
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
        event(
            &tx,
            ask.task_id,
            ask.run_id.as_deref(),
            "ask_answered",
            json!({"ask_id": id, "kind": ask.kind}),
        )?;
        let answered = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(answered)
    }

    /// Mark an answered ask read by the maintainer. Writes no event. An
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

    pub fn read_ask(&self, id: i64) -> Result<Ask> {
        read_ask(&self.conn, id)
    }
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
