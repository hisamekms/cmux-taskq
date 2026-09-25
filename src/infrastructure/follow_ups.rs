//! Follow-up triage (ADR-0037): the follow_up drafts `integrate` registers,
//! the task lease their headless job holds, and the runtime's application of
//! the job's verdict and of a person's answer to the `follow_up` ask. Every
//! change to a draft is one transaction that re-checks the draft first, so a
//! draft is never applied twice.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::{
    asks::{AskQuery, insert_ask},
    runtime_store::{HEARTBEAT_TIMEOUT_SECS, TRIAGE_ASKER},
    sqlite::{SqliteQueue, event_row, insert_task, read_task, transition_task},
};
use crate::application::{FollowUpApplied, FollowUpJob, FollowUpStart, FollowUpStore};
use crate::domain::{
    Ask, AskId, AskKind, FOLLOW_UP_OPTIONS, FollowUpAction, FollowUpDecision, FollowUpProposal,
    FollowUpVerdict, GoalId, NewAsk, RunEvent, RunId, Task, TaskAction, TaskId, TaskStatus,
    follow_up::{FollowUpFacts, MAX_FOLLOW_UP_TRIAGE_ATTEMPTS, adopt_override},
};

/// `reason` of the task lease a follow-up triage holds.
pub const FOLLOW_UP_TRIAGE: &str = "follow_up_triage";

/// `asked_by` of the `follow_up` asks: the supervisor that ran the job.
pub const FOLLOW_UP_ASKER: &str = TRIAGE_ASKER;

/// Follow-up triages the whole queue runs at once, so drafts left from
/// before it never block the workers' claims (ADR-0037 decision 3).
const FOLLOW_UP_TRIAGE_SLOTS: i64 = 1;

/// The draft targets: `draft` tasks a `follow_up_registered` names, not yet
/// triaged to an end. Drafts registered before follow-up triage existed
/// match too.
const TARGETS: &str = "SELECT t.* FROM tasks t WHERE t.status='draft'
    AND EXISTS(SELECT 1 FROM run_events e WHERE e.kind='follow_up_registered'
               AND json_extract(e.payload,'$.task_id')=t.id)
    AND NOT EXISTS(SELECT 1 FROM run_events e WHERE e.task_id=t.id
               AND e.kind IN ('follow_up_triage_finished','follow_up_triage_failed'))";

/// Where a draft came from: the task and run whose receipt proposed it.
struct Origin {
    task_id: TaskId,
    run_id: Option<RunId>,
}

impl SqliteQueue {
    /// The follow_up drafts a follow-up triage decides, oldest first.
    pub fn follow_up_drafts(&self) -> Result<Vec<Task>> {
        let tasks: Vec<TaskId> = self
            .conn
            .prepare(&format!("{TARGETS} ORDER BY t.id"))?
            .query_map([], |r| r.get("id"))?
            .collect::<rusqlite::Result<_>>()?;
        tasks
            .into_iter()
            .map(|id| read_task(&self.conn, id))
            .collect()
    }

    /// Take the draft for its job: in one transaction, check that it is
    /// still a target, that no fresh lease holds it (a stale one is
    /// replaced) and that no other follow-up triage runs, then lease it to
    /// `token` and record `lease_acquired` and `follow_up_triage_started`
    /// (`attempt`). A draft whose job started
    /// [`MAX_FOLLOW_UP_TRIAGE_ATTEMPTS`] times is not started again: its
    /// `follow_up_triage_failed` is recorded instead.
    pub fn begin_follow_up_triage(&mut self, draft: TaskId, token: &str) -> Result<FollowUpStart> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if !is_target(&tx, draft)? {
            return Ok(FollowUpStart::Skipped);
        }
        let fresh = now - HEARTBEAT_TIMEOUT_SECS;
        let previous: Option<(String, i64)> = tx
            .query_row(
                "SELECT supervisor_token, heartbeat_at FROM task_leases WHERE task_id=?1",
                [draft],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if previous.as_ref().is_some_and(|(_, at)| *at >= fresh) {
            return Ok(FollowUpStart::Skipped);
        }
        let running: i64 = tx.query_row(
            "SELECT count(*) FROM task_leases WHERE reason=?1 AND heartbeat_at>=?2 AND task_id<>?3",
            params![FOLLOW_UP_TRIAGE, fresh, draft],
            |r| r.get(0),
        )?;
        if running >= FOLLOW_UP_TRIAGE_SLOTS {
            return Ok(FollowUpStart::Skipped);
        }
        let task = read_task(&tx, draft)?;
        let started = draft_events(&tx, draft)?
            .iter()
            .filter(|e| e.kind == "follow_up_triage_started")
            .count();
        if started >= MAX_FOLLOW_UP_TRIAGE_ATTEMPTS {
            tx.execute("DELETE FROM task_leases WHERE task_id=?1", [draft])?;
            draft_event(
                &tx,
                &task,
                "follow_up_triage_failed",
                json!({
                    "attempt": started,
                    "error": format!(
                        "the follow-up triage was started {started} times without a verdict (at most {MAX_FOLLOW_UP_TRIAGE_ATTEMPTS})"
                    ),
                    "duration_secs": 0,
                    "status": task.status().as_str(),
                }),
            )?;
            tx.commit()?;
            return Ok(FollowUpStart::Exhausted { attempts: started });
        }
        tx.execute("DELETE FROM task_leases WHERE task_id=?1", [draft])?;
        tx.execute(
            "INSERT INTO task_leases(task_id,supervisor_token,reason,heartbeat_at) VALUES (?1,?2,?3,?4)",
            params![draft, token, FOLLOW_UP_TRIAGE, now],
        )?;
        let attempt = started + 1;
        draft_event(
            &tx,
            &task,
            "lease_acquired",
            json!({
                "pid": std::process::id(),
                "reason": FOLLOW_UP_TRIAGE,
                "previous_token": previous.map(|(token, _)| token),
            }),
        )?;
        draft_event(
            &tx,
            &task,
            "follow_up_triage_started",
            json!({"attempt": attempt}),
        )?;
        tx.commit()?;
        Ok(FollowUpStart::Started {
            draft: Box::new(task),
            attempt,
        })
    }

    /// Apply the job's verdict under its lease, in one transaction
    /// (ADR-0037 decisions 5 and 6): `adopt` registers the proposal as a
    /// `ready` task of the draft's goal and cancels the draft, `drop`
    /// cancels the draft, and `ask` opens a `follow_up` ask about it. An
    /// `adopt` of an invalid proposal, of a draft whose goal is closed or
    /// missing, of a draft two follow-ups from a person, or without
    /// acceptance becomes an ask, the reason in `overridden`. Records
    /// `follow_up_triage_finished` and releases the lease. `Ok(None)` means
    /// the lease was lost and nothing was written.
    pub fn finish_follow_up_triage(
        &mut self,
        draft: TaskId,
        token: &str,
        job: &FollowUpJob,
        verdict: &FollowUpVerdict,
    ) -> Result<Option<FollowUpApplied>> {
        let mut tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if !holds_task_lease(&tx, draft, token, now)? {
            return Ok(None);
        }
        let task = read_task(&tx, draft)?;
        if task.status() != TaskStatus::Draft {
            // A person readied or canceled it during the job: nothing is
            // applied and no job outcome is recorded; the lease goes.
            tx.execute("DELETE FROM task_leases WHERE task_id=?1", [draft])?;
            draft_event(
                &tx,
                &task,
                "lease_released",
                json!({"reason": FOLLOW_UP_TRIAGE}),
            )?;
            tx.commit()?;
            return Ok(Some(FollowUpApplied {
                action: FollowUpAction::Closed,
                draft: task,
                new_task: None,
                ask: None,
                overridden: None,
            }));
        }
        let depth = depth(&tx, draft)?;
        let goal_open = goal_open(&tx, task.goal_id())?;
        // The proposal as it can be applied, or why it cannot.
        let proposal = verdict.task.as_ref().map(|p| check_proposal(&tx, draft, p));
        let overridden = match (verdict.verdict, &proposal) {
            (FollowUpDecision::Adopt, None) => {
                Some("the adopt verdict proposed no task".to_owned())
            }
            (FollowUpDecision::Adopt, Some(Err(why))) => Some(why.clone()),
            (FollowUpDecision::Adopt, Some(Ok(p))) => {
                adopt_override(FollowUpFacts { goal_open, depth }, p)
            }
            _ => None,
        };
        let timestamp = self.generators.clock.timestamp();
        let (mut overridden, mut proposal) = (overridden, proposal);
        let mut adopted = None;
        if let (FollowUpDecision::Adopt, Some(Ok(p)), None) =
            (verdict.verdict, proposal.clone(), &overridden)
        {
            let origin = origin(&tx, draft)?;
            // A registration the store refuses (a task → goal dependency
            // that would close a cycle, ADR-0038) rolls back to here and
            // becomes an ask.
            let savepoint = tx.savepoint()?;
            match adopt(
                &savepoint,
                &task,
                &p,
                task.goal_id(),
                depth,
                &origin,
                Adopter::Job,
                &timestamp,
            ) {
                Ok(new) => {
                    savepoint.commit()?;
                    adopted = Some(new);
                }
                Err(error) => {
                    drop(savepoint);
                    let why = format!("the proposed task could not be registered: {error:#}");
                    overridden = Some(why.clone());
                    proposal = Some(Err(why));
                }
            }
        }
        let mut applied = FollowUpApplied {
            action: FollowUpAction::Asked,
            draft: task.clone(),
            new_task: None,
            ask: None,
            overridden: overridden.clone(),
        };
        match (verdict.verdict, &adopted) {
            (_, Some(new)) => {
                applied.action = FollowUpAction::Adopted;
                applied.new_task = Some(new.clone());
            }
            (FollowUpDecision::Drop, _) => {
                transition_task(&tx, draft, TaskAction::Cancel, &timestamp)?;
                applied.action = FollowUpAction::Dropped;
            }
            _ => {
                let valid = proposal.as_ref().and_then(|p| p.as_ref().ok());
                let question = ask_question(&task, verdict, overridden.as_deref(), valid, job);
                let options: Vec<String> = FOLLOW_UP_OPTIONS
                    .iter()
                    .filter(|o| valid.is_some() || **o != "adopt")
                    .map(|o| (*o).to_owned())
                    .collect();
                let ask = NewAsk {
                    kind: AskKind::FollowUp,
                    task_id: Some(draft),
                    run_id: None,
                    question,
                    options,
                    asked_by: FOLLOW_UP_ASKER.to_owned(),
                };
                ask.validate()?;
                applied.ask = Some(insert_ask(&tx, &ask)?);
            }
        }
        tx.execute("DELETE FROM task_leases WHERE task_id=?1", [draft])?;
        let after = read_task(&tx, draft)?;
        draft_event(
            &tx,
            &after,
            "lease_released",
            json!({"reason": FOLLOW_UP_TRIAGE}),
        )?;
        draft_event(
            &tx,
            &after,
            "follow_up_triage_finished",
            json!({
                "attempt": job.attempt,
                "verdict": verdict.verdict,
                "reason": verdict.reason,
                "question": verdict.question,
                "overridden": overridden,
                "task": verdict.task,
                "action": applied.action,
                "new_task_id": applied.new_task.as_ref().map(Task::id),
                "ask_id": applied.ask.as_ref().map(|a| a.ask.id),
                "depth": depth,
                "duration_secs": job.duration_secs,
                "status": after.status().as_str(),
            }),
        )?;
        tx.commit()?;
        applied.draft = after;
        Ok(Some(applied))
    }

    /// Record that the draft's job failed (ADR-0037 decision 8) and release
    /// its lease, leaving the draft as it is; a person decides it. `false`
    /// when another process holds a fresh lease on the draft, and then
    /// nothing is written.
    pub fn fail_follow_up_triage(
        &mut self,
        draft: TaskId,
        token: &str,
        job: &FollowUpJob,
        error: &str,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let lease: Option<(String, i64)> = tx
            .query_row(
                "SELECT supervisor_token, heartbeat_at FROM task_leases WHERE task_id=?1",
                [draft],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if lease
            .as_ref()
            .is_some_and(|(owner, at)| owner != token && *at >= now - HEARTBEAT_TIMEOUT_SECS)
        {
            return Ok(false);
        }
        let task = read_task(&tx, draft)?;
        let ended = draft_events(&tx, draft)?.iter().any(|e| {
            matches!(
                e.kind.as_str(),
                "follow_up_triage_finished" | "follow_up_triage_failed"
            )
        });
        if ended || task.status() != TaskStatus::Draft {
            // The verdict was applied, or a person decided the draft:
            // there is no failure to record, only a lease to give up.
            if tx.execute(
                "DELETE FROM task_leases WHERE task_id=?1 AND supervisor_token=?2",
                params![draft, token],
            )? == 1
            {
                draft_event(
                    &tx,
                    &task,
                    "lease_released",
                    json!({"reason": FOLLOW_UP_TRIAGE}),
                )?;
                tx.commit()?;
            }
            return Ok(false);
        }
        if tx.execute("DELETE FROM task_leases WHERE task_id=?1", [draft])? == 1 {
            draft_event(
                &tx,
                &task,
                "lease_released",
                json!({"reason": FOLLOW_UP_TRIAGE}),
            )?;
        }
        draft_event(
            &tx,
            &task,
            "follow_up_triage_failed",
            json!({
                "attempt": job.attempt,
                "error": error,
                "duration_secs": job.duration_secs,
                "status": task.status().as_str(),
            }),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The answered `follow_up` asks nobody closed, oldest first: the
    /// answers the supervisor applies.
    pub fn follow_up_answers(&self) -> Result<Vec<Ask>> {
        Ok(self
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| ask.kind == AskKind::FollowUp && ask.answered_at.is_some())
            .collect())
    }

    /// Apply a person's answer to a `follow_up` ask and close the ask, in
    /// one transaction (ADR-0037 decision 7), re-checking that the ask is
    /// answered and unclosed and that no fresh lease holds its draft:
    /// `adopt` registers the job's last proposal (checked again now) as a
    /// `ready` task at depth 0 and cancels the draft, `cancel` cancels the
    /// draft, `keep_draft` leaves it for the planner. A draft that is no
    /// longer `draft` only has its ask closed. Records `follow_up_decided`.
    /// `Ok(None)` means the answer is none the runtime applies (free text,
    /// an option the ask did not offer, or `adopt` without a valid
    /// proposal) or the draft is leased; nothing is written then.
    pub fn decide_follow_up(&mut self, ask_id: AskId) -> Result<Option<FollowUpApplied>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let ask = super::asks::read_ask(&tx, ask_id)?;
        ensure!(
            ask.kind == AskKind::FollowUp,
            "ask {ask_id} is a {} ask, not a follow_up one",
            ask.kind.as_str()
        );
        let (Some(answer), None) = (ask.answer.as_deref(), ask.closed_at) else {
            bail!("ask {ask_id} is not an answered, unclosed ask");
        };
        let answer = answer.trim().to_owned();
        let draft = ask.task_id.context("a follow_up ask names its draft")?;
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_leases WHERE task_id=?1 AND heartbeat_at>=?2)",
            params![draft, now - HEARTBEAT_TIMEOUT_SECS],
            |r| r.get(0),
        )?;
        if leased {
            return Ok(None);
        }
        let task = read_task(&tx, draft)?;
        let timestamp = self.generators.clock.timestamp();
        let mut applied = FollowUpApplied {
            action: FollowUpAction::Closed,
            draft: task.clone(),
            new_task: None,
            ask: None,
            overridden: None,
        };
        if task.status() == TaskStatus::Draft {
            if !FOLLOW_UP_OPTIONS.contains(&answer.as_str()) || !ask.options.contains(&answer) {
                return Ok(None);
            }
            match answer.as_str() {
                "adopt" => {
                    let Some(Ok(proposal)) =
                        last_proposal(&tx, draft)?.map(|p| check_proposal(&tx, draft, &p))
                    else {
                        return Ok(None);
                    };
                    // A person chose it: a closed or missing goal takes no
                    // task, so the task is registered without one.
                    let goal = task
                        .goal_id()
                        .filter(|_| goal_open(&tx, task.goal_id()).unwrap_or(false));
                    let origin = origin(&tx, draft)?;
                    // A registration the store refuses is not applied: the
                    // transaction rolls back and the answer is a person's.
                    let Ok(new) = adopt(
                        &tx,
                        &task,
                        &proposal,
                        goal,
                        0,
                        &origin,
                        Adopter::Person { ask_id },
                        &timestamp,
                    ) else {
                        return Ok(None);
                    };
                    applied.action = FollowUpAction::Adopted;
                    applied.new_task = Some(new);
                }
                "cancel" => {
                    transition_task(&tx, draft, TaskAction::Cancel, &timestamp)?;
                    applied.action = FollowUpAction::Dropped;
                }
                _ => applied.action = FollowUpAction::KeptDraft,
            }
        }
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            params![ask_id, now],
        )?;
        let after = read_task(&tx, draft)?;
        draft_event(
            &tx,
            &after,
            "follow_up_decided",
            json!({
                "ask_id": ask_id,
                "answer": answer,
                "action": applied.action,
                "new_task_id": applied.new_task.as_ref().map(Task::id),
                "status": after.status().as_str(),
            }),
        )?;
        tx.commit()?;
        applied.draft = after;
        Ok(Some(applied))
    }

    /// Whether the answer of a `follow_up` ask is one the supervisor
    /// applies: its draft is still a draft, the answer is an option the ask
    /// offered, and for `adopt` the job's proposal is still valid.
    pub(super) fn follow_up_answer_applies(
        conn: &Connection,
        ask: &Ask,
        answer: &str,
    ) -> Result<bool> {
        let Some(draft) = ask.task_id else {
            return Ok(false);
        };
        if read_task(conn, draft)?.status() != TaskStatus::Draft {
            // Nothing is left to apply: the supervisor closes the ask.
            return Ok(true);
        }
        if !FOLLOW_UP_OPTIONS.contains(&answer) || !ask.options.iter().any(|o| o == answer) {
            return Ok(false);
        }
        if answer != "adopt" {
            return Ok(true);
        }
        Ok(last_proposal(conn, draft)?.is_some_and(|p| check_proposal(conn, draft, &p).is_ok()))
    }

    /// Whether the supervisor applies the answer the `follow_up` ask has
    /// (see [`SqliteQueue::decide_follow_up`]); `false` for an unanswered
    /// or closed ask.
    pub fn applies_follow_up_answer(&self, ask: &Ask) -> Result<bool> {
        match (&ask.answer, ask.closed_at, ask.kind) {
            (Some(answer), None, AskKind::FollowUp) => {
                Self::follow_up_answer_applies(&self.conn, ask, answer.trim())
            }
            _ => Ok(false),
        }
    }

    pub fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        depth(&self.conn, task)
    }

    pub fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        ensure!(depth >= 0, "follow_up_depth must not be negative");
        ensure!(
            self.conn.execute(
                "UPDATE tasks SET follow_up_depth=?2 WHERE id=?1",
                params![task, depth],
            )? == 1,
            "task {task} does not exist"
        );
        Ok(())
    }
}

impl FollowUpStore for SqliteQueue {
    fn follow_up_drafts(&self) -> Result<Vec<Task>> {
        SqliteQueue::follow_up_drafts(self)
    }
    fn begin_follow_up_triage(&mut self, draft: TaskId, token: &str) -> Result<FollowUpStart> {
        SqliteQueue::begin_follow_up_triage(self, draft, token)
    }
    fn finish_follow_up_triage(
        &mut self,
        draft: TaskId,
        token: &str,
        job: &FollowUpJob,
        verdict: &FollowUpVerdict,
    ) -> Result<Option<FollowUpApplied>> {
        SqliteQueue::finish_follow_up_triage(self, draft, token, job, verdict)
    }
    fn fail_follow_up_triage(
        &mut self,
        draft: TaskId,
        token: &str,
        job: &FollowUpJob,
        error: &str,
    ) -> Result<bool> {
        SqliteQueue::fail_follow_up_triage(self, draft, token, job, error)
    }
    fn follow_up_answers(&self) -> Result<Vec<Ask>> {
        SqliteQueue::follow_up_answers(self)
    }
    fn decide_follow_up(&mut self, ask_id: AskId) -> Result<Option<FollowUpApplied>> {
        SqliteQueue::decide_follow_up(self, ask_id)
    }
    fn applies_follow_up_answer(&self, ask: &Ask) -> Result<bool> {
        SqliteQueue::applies_follow_up_answer(self, ask)
    }
    fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        SqliteQueue::follow_up_depth(self, task)
    }
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        SqliteQueue::set_follow_up_depth(self, task, depth)
    }
}

fn is_target(conn: &Connection, draft: TaskId) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS({TARGETS} AND t.id=?1)"),
        [draft],
        |r| r.get(0),
    )?)
}

fn holds_task_lease(conn: &Connection, draft: TaskId, token: &str, now: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_leases WHERE task_id=?1 AND supervisor_token=?2
         AND heartbeat_at>=?3)",
        params![draft, token, now - HEARTBEAT_TIMEOUT_SECS],
        |r| r.get(0),
    )?)
}

fn depth(conn: &Connection, task: TaskId) -> Result<i64> {
    conn.query_row(
        "SELECT follow_up_depth FROM tasks WHERE id=?1",
        [task],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("task {task} does not exist"))
}

/// Whether the task's goal takes tasks: it has one and it is not closed.
fn goal_open(conn: &Connection, goal: Option<GoalId>) -> Result<bool> {
    let Some(goal) = goal else {
        return Ok(false);
    };
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM goals WHERE id=?1 AND closed_at IS NULL)",
        [goal],
        |r| r.get(0),
    )?)
}

fn draft_events(conn: &Connection, draft: TaskId) -> Result<Vec<RunEvent>> {
    Ok(conn
        .prepare("SELECT * FROM run_events WHERE task_id=?1 AND run_id IS NULL ORDER BY id")?
        .query_map([draft], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// An event of the draft: its task and goal, and no run.
fn draft_event(conn: &Connection, task: &Task, kind: &str, payload: Value) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,goal_id,kind,payload) VALUES (?1,?2,?3,?4)",
        params![
            task.id(),
            task.goal_id(),
            kind,
            serde_json::to_string(&payload)?
        ],
    )?;
    Ok(())
}

/// The run whose receipt proposed the draft, from its `follow_up_registered`.
fn origin(conn: &Connection, draft: TaskId) -> Result<Origin> {
    conn.query_row(
        "SELECT task_id, run_id FROM run_events WHERE kind='follow_up_registered'
         AND json_extract(payload,'$.task_id')=?1 ORDER BY id LIMIT 1",
        [draft],
        |r| {
            Ok(Origin {
                task_id: r.get(0)?,
                run_id: r.get(1)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("task {draft} has no follow_up_registered"))
}

/// The proposal of the draft's latest `follow_up_triage_finished`.
fn last_proposal(conn: &Connection, draft: TaskId) -> Result<Option<FollowUpProposal>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM run_events WHERE task_id=?1 AND kind='follow_up_triage_finished'
             ORDER BY id DESC LIMIT 1",
            [draft],
            |r| r.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&payload)?;
    Ok(serde_json::from_value(payload["task"].clone()).ok())
}

/// The proposal if it can replace `draft` now (ADR-0037 decision 4): it
/// passes [`FollowUpProposal::check`], every task it depends on exists and
/// is not canceled nor the draft, and moving the draft's dependents onto it
/// makes no cycle. Otherwise why not.
fn check_proposal(
    conn: &Connection,
    draft: TaskId,
    proposal: &FollowUpProposal,
) -> Result<FollowUpProposal, String> {
    proposal.check()?;
    let lookup = |sql: &str, id: i64| -> Result<Option<String>, String> {
        conn.query_row(sql, [id], |r| r.get(0))
            .optional()
            .map_err(|error| format!("the proposal could not be checked: {error}"))
    };
    for id in &proposal.depends_on {
        if *id == draft.as_i64() {
            return Err(format!(
                "the proposed task depends on the draft {draft} it replaces"
            ));
        }
        match lookup("SELECT status FROM tasks WHERE id=?1", *id)?.as_deref() {
            None => {
                return Err(format!(
                    "the proposed task depends on task {id}, which does not exist"
                ));
            }
            Some("canceled") => {
                return Err(format!(
                    "the proposed task depends on task {id}, which is canceled"
                ));
            }
            Some(_) => {}
        }
        // The new task takes the draft's dependents: a predecessor that
        // (transitively) depends on the draft would close a cycle.
        let reaches_draft: bool = conn
            .query_row(
                "WITH RECURSIVE ancestors(id) AS (
                    SELECT ?1 UNION
                    SELECT d.predecessor_id FROM task_dependencies d JOIN ancestors a ON d.task_id=a.id
                 ) SELECT EXISTS(SELECT 1 FROM ancestors WHERE id=?2)",
                params![id, draft],
                |r| r.get(0),
            )
            .map_err(|error| format!("the proposal could not be checked: {error}"))?;
        if reaches_draft {
            return Err(format!(
                "the proposed task depends on task {id}, which depends on the draft {draft}: taking the draft's dependents would make a cycle"
            ));
        }
    }
    Ok(proposal.clone())
}

/// Who adopted a draft.
#[derive(Clone, Copy)]
enum Adopter {
    Job,
    Person { ask_id: AskId },
}

/// Replace `draft` with the `ready` task `proposal` in `goal`, inside the
/// caller's transaction: register it with its dependencies, move the
/// draft's dependents onto it, record where it came from in its context and
/// `follow_up_adopted` (on both), give it `depth`, and cancel the draft.
#[allow(clippy::too_many_arguments)]
fn adopt(
    tx: &Connection,
    draft: &Task,
    proposal: &FollowUpProposal,
    goal: Option<GoalId>,
    depth: i64,
    origin: &Origin,
    by: Adopter,
    now: &str,
) -> Result<Task> {
    let proposed_by = match &origin.run_id {
        Some(run) => format!("task {} の run {run} の receipt が提案", origin.task_id),
        None => format!("task {} の receipt が提案", origin.task_id),
    };
    let adopted_by = match by {
        Adopter::Job => "follow-up triage が adopt".to_owned(),
        Adopter::Person { ask_id } => format!("ask {ask_id} の answer で人が adopt"),
    };
    let context = format!(
        "follow-up draft task {}（{proposed_by}）を{adopted_by}",
        draft.id()
    );
    let created = insert_task(tx, proposal.new_task(goal, &context), now)?;
    let id = created.id();
    let dependents: Vec<TaskId> = tx
        .prepare("SELECT task_id FROM task_dependencies WHERE predecessor_id=?1 ORDER BY task_id")?
        .query_map([draft.id()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for dependent in &dependents {
        tx.execute(
            "UPDATE task_dependencies SET predecessor_id=?3 WHERE task_id=?1 AND predecessor_id=?2",
            params![dependent, draft.id(), id],
        )?;
        let task = read_task(tx, *dependent)?;
        draft_event(
            tx,
            &task,
            "dependency_removed",
            json!({"predecessor_id": draft.id(), "by": "follow_up_adopted"}),
        )?;
        draft_event(
            tx,
            &task,
            "dependency_added",
            json!({"predecessor_id": id, "by": "follow_up_adopted"}),
        )?;
    }
    transition_task(tx, id, TaskAction::BypassReview, now)?;
    tx.execute(
        "UPDATE tasks SET follow_up_depth=?2 WHERE id=?1",
        params![id, depth],
    )?;
    transition_task(tx, draft.id(), TaskAction::Cancel, now)?;
    let (by, ask_id) = match by {
        Adopter::Job => ("job", None),
        Adopter::Person { ask_id } => ("person", Some(ask_id)),
    };
    let payload = json!({
        "draft_task_id": draft.id(),
        "new_task_id": id,
        "source_task_id": origin.task_id,
        "source_run_id": origin.run_id,
        "by": by,
        "ask_id": ask_id,
        "depth": depth,
        "moved_dependents": dependents,
    });
    let adopted = read_task(tx, id)?;
    draft_event(
        tx,
        &read_task(tx, draft.id())?,
        "follow_up_adopted",
        payload.clone(),
    )?;
    draft_event(tx, &adopted, "follow_up_adopted", payload)?;
    Ok(adopted)
}

/// The question of the `follow_up` ask: the job's (or why the runtime
/// overrode its adopt), its reason, a summary of its proposal, the prompt
/// it read and what each option does.
fn ask_question(
    draft: &Task,
    verdict: &FollowUpVerdict,
    overridden: Option<&str>,
    proposal: Option<&FollowUpProposal>,
    job: &FollowUpJob,
) -> String {
    let asked = match overridden {
        Some(why) => format!("the follow-up triage answered adopt, but {why}"),
        None if !verdict.question.trim().is_empty() => verdict.question.trim().to_owned(),
        None => "should this follow-up be adopted?".to_owned(),
    };
    let mut question = format!(
        "The follow-up triage of draft task {} ({}) asks a person: {asked}\nReason: {}",
        draft.id(),
        draft.title(),
        verdict.reason
    );
    match proposal {
        Some(p) => question.push_str(&format!(
            "\nProposed task: {} / acceptance: {} / verification: {} / paths: {} / evidence: {} / depends on: {}",
            p.title,
            if p.acceptance.trim().is_empty() { "(none)" } else { p.acceptance.trim() },
            p.verification_commands.join("; "),
            p.paths.join(" "),
            p.evidence.join(" "),
            p.depends_on
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        )),
        None => question.push_str("\nNo valid task was proposed, so adopt is not offered."),
    }
    if let Some(path) = &job.prompt_path {
        question.push_str(&format!("\nFollow-up triage material: {path}"));
    }
    if proposal.is_some() {
        question.push_str("\nadopt: register the proposed task as ready and cancel the draft.");
    }
    question
        .push_str("\ncancel: cancel the draft. keep_draft: keep it as a draft for the planner.");
    question
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::TaskStore;
    use crate::domain::{ClaimOutcome, CommitSha, NewGoal, NewTask};
    use tempfile::TempDir;

    const TOKEN: &str = "sv";

    fn new_task(title: &str, goal_id: Option<GoalId>) -> NewTask {
        NewTask {
            title: title.into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["cargo test".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id,
            context: String::new(),
        }
    }

    struct Fixture {
        _dir: TempDir,
        queue: SqliteQueue,
        goal: GoalId,
        source: TaskId,
        run: RunId,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        let goal = queue
            .add_goal(NewGoal {
                title: "g".into(),
                description: "d".into(),
                acceptance: "a".into(),
                constraints: "c".into(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        let source = queue.add(new_task("source", Some(goal))).unwrap().id();
        queue.transition(source, TaskAction::BypassReview).unwrap();
        let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
        let ClaimOutcome::Claimed { run } = queue.claim(&base).unwrap() else {
            panic!("nothing claimed");
        };
        Fixture {
            _dir: dir,
            queue,
            goal,
            source,
            run: run.id().clone(),
        }
    }

    impl Fixture {
        /// A follow_up draft as `integrate` registers it, at `depth`.
        fn draft(&mut self, title: &str, goal: Option<GoalId>, depth: i64) -> TaskId {
            let mut new = new_task(title, goal);
            new.acceptance = String::new();
            new.verification_commands = Vec::new();
            let id = self.queue.add(new).unwrap().id();
            self.queue.set_follow_up_depth(id, depth).unwrap();
            self.queue
                .record_runtime_event(
                    &self.run,
                    "follow_up_registered",
                    json!({"task_id": id, "title": title, "index": 0}),
                )
                .unwrap();
            id
        }

        fn events(&self, task: TaskId, kind: &str) -> Vec<Value> {
            self.queue
                .conn
                .prepare("SELECT payload FROM run_events WHERE task_id=?1 AND kind=?2 ORDER BY id")
                .unwrap()
                .query_map(params![task, kind], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
                .collect()
        }

        fn started(&mut self, draft: TaskId) -> usize {
            match self.queue.begin_follow_up_triage(draft, TOKEN).unwrap() {
                FollowUpStart::Started { attempt, .. } => attempt,
                other => panic!("not started: {other:?}"),
            }
        }

        fn finish(&mut self, draft: TaskId, verdict: FollowUpVerdict) -> FollowUpApplied {
            self.started(draft);
            self.queue
                .finish_follow_up_triage(draft, TOKEN, &job(), &verdict)
                .unwrap()
                .expect("the lease is held")
        }

        fn targets(&self) -> Vec<TaskId> {
            self.queue
                .follow_up_drafts()
                .unwrap()
                .iter()
                .map(Task::id)
                .collect()
        }
    }

    fn job() -> FollowUpJob {
        FollowUpJob {
            attempt: 1,
            duration_secs: 4,
            prompt_path: Some("/q/follow-ups/1/follow-up-triage-prompt-1.txt".into()),
        }
    }

    fn proposal() -> FollowUpProposal {
        FollowUpProposal {
            title: "complete fix".into(),
            description: "what".into(),
            acceptance: "it works".into(),
            verification_commands: vec!["cargo fmt --all --check".into()],
            paths: vec!["docs/**".into()],
            evidence: vec!["e2e".into()],
            depends_on: Vec::new(),
            context: "why".into(),
        }
    }

    fn verdict(decision: FollowUpDecision, task: Option<FollowUpProposal>) -> FollowUpVerdict {
        FollowUpVerdict {
            verdict: decision,
            reason: "because".into(),
            task,
            question: String::new(),
        }
    }

    #[test]
    fn adopt_registers_a_ready_task_cancels_the_draft_and_records_it_once() {
        let mut f = fixture();
        let draft = f.draft("follow", Some(f.goal), 1);
        // A task waiting on the draft moves onto the adopted task.
        let dependent = f.queue.add(new_task("after", Some(f.goal))).unwrap().id();
        f.queue.add_dependency(dependent, draft).unwrap();
        let mut p = proposal();
        p.depends_on = vec![f.source.as_i64()];
        assert_eq!(f.targets(), [draft]);
        let applied = f.finish(draft, verdict(FollowUpDecision::Adopt, Some(p)));
        assert_eq!(applied.action, FollowUpAction::Adopted);
        assert_eq!(applied.overridden, None);
        assert!(applied.ask.is_none());
        assert_eq!(applied.draft.status(), TaskStatus::Canceled);
        let new = applied.new_task.unwrap();
        assert_eq!(new.status(), TaskStatus::Ready);
        assert_eq!(new.goal_id(), Some(f.goal));
        assert_eq!(new.acceptance(), "it works");
        assert_eq!(new.verification_commands(), ["cargo fmt --all --check"]);
        assert_eq!(new.paths(), ["docs/**"]);
        assert_eq!(new.required_evidence(), [crate::domain::EvidenceCheck::E2e]);
        assert!(
            new.context().starts_with(&format!(
                "follow-up draft task {draft}（task {} の run {} の receipt が提案）をfollow-up triage が adopt",
                f.source, f.run
            )),
            "{}",
            new.context()
        );
        assert!(new.context().ends_with("\n\nwhy"));
        assert_eq!(f.queue.follow_up_depth(new.id()).unwrap(), 1);
        let predecessors: Vec<TaskId> = f
            .queue
            .conn
            .prepare("SELECT predecessor_id FROM task_dependencies WHERE task_id=?1")
            .unwrap()
            .query_map([new.id()], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(predecessors, [f.source]);
        let moved: Vec<TaskId> = f
            .queue
            .conn
            .prepare("SELECT predecessor_id FROM task_dependencies WHERE task_id=?1")
            .unwrap()
            .query_map([dependent], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(moved, [new.id()]);
        for task in [draft, new.id()] {
            let adopted = f.events(task, "follow_up_adopted");
            assert_eq!(adopted.len(), 1);
            assert_eq!(adopted[0]["by"], "job");
            assert_eq!(adopted[0]["draft_task_id"], draft.as_i64());
            assert_eq!(adopted[0]["new_task_id"], new.id().as_i64());
            assert_eq!(adopted[0]["source_task_id"], f.source.as_i64());
            assert_eq!(adopted[0]["source_run_id"], f.run.as_str());
            assert_eq!(adopted[0]["depth"], 1);
        }
        let finished = f.events(draft, "follow_up_triage_finished");
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["action"], "adopted");
        assert_eq!(finished[0]["new_task_id"], new.id().as_i64());
        assert_eq!(finished[0]["status"], "canceled");
        assert_eq!(finished[0]["duration_secs"], 4);
        assert_eq!(f.events(draft, "lease_released").len(), 1);
        // Applied once: the lease is gone, the draft is no target.
        assert!(f.targets().is_empty());
        assert!(
            f.queue
                .finish_follow_up_triage(
                    draft,
                    TOKEN,
                    &job(),
                    &verdict(FollowUpDecision::Adopt, Some(proposal()))
                )
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            f.queue.begin_follow_up_triage(draft, TOKEN).unwrap(),
            FollowUpStart::Skipped
        ));
        let tasks: i64 = f
            .queue
            .conn
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tasks, 4);
    }

    #[test]
    fn drop_cancels_the_draft() {
        let mut f = fixture();
        let draft = f.draft("follow", Some(f.goal), 1);
        let applied = f.finish(draft, verdict(FollowUpDecision::Drop, Some(proposal())));
        assert_eq!(applied.action, FollowUpAction::Dropped);
        assert_eq!(applied.draft.status(), TaskStatus::Canceled);
        assert!(applied.new_task.is_none() && applied.ask.is_none());
        assert_eq!(
            f.events(draft, "follow_up_triage_finished")[0]["action"],
            "dropped"
        );
        assert!(f.targets().is_empty());
    }

    /// Adopt a proposal the runtime overrides; the ask and the reason.
    fn overridden(f: &mut Fixture, draft: TaskId, p: FollowUpProposal) -> (Ask, String) {
        let applied = f.finish(draft, verdict(FollowUpDecision::Adopt, Some(p)));
        assert_eq!(applied.action, FollowUpAction::Asked);
        assert_eq!(applied.draft.status(), TaskStatus::Draft);
        assert!(applied.new_task.is_none());
        let why = applied.overridden.unwrap();
        let finished = f.events(draft, "follow_up_triage_finished");
        assert_eq!(finished[0]["overridden"], why.as_str());
        assert_eq!(finished[0]["action"], "asked");
        let outcome = applied.ask.unwrap();
        assert!(outcome.created);
        assert_eq!(finished[0]["ask_id"], outcome.ask.id.as_i64());
        assert_eq!(outcome.ask.kind, AskKind::FollowUp);
        assert_eq!(outcome.ask.task_id, Some(draft));
        assert_eq!(outcome.ask.run_id, None);
        assert_eq!(outcome.ask.asked_by, "supervisor");
        assert!(
            outcome.ask.question.contains(&why),
            "{}",
            outcome.ask.question
        );
        assert!(
            outcome
                .ask
                .question
                .contains("follow-up-triage-prompt-1.txt")
        );
        (outcome.ask, why)
    }

    #[test]
    fn adopt_becomes_an_ask_for_a_closed_goal_depth_two_or_no_acceptance() {
        let mut f = fixture();
        let closed = f.draft("no goal", None, 1);
        let (ask, why) = overridden(&mut f, closed, proposal());
        assert!(why.contains("closed"), "{why}");
        assert_eq!(ask.options, FOLLOW_UP_OPTIONS);

        let other = f
            .queue
            .add_goal(NewGoal {
                title: "closing".into(),
                ..NewGoal::default()
            })
            .unwrap()
            .id();
        let in_closed = f.draft("closed goal", Some(other), 1);
        f.queue
            .conn
            .execute(
                "UPDATE goals SET closed_at='now', verdict='abandoned' WHERE id=?1",
                [other],
            )
            .unwrap();
        assert!(
            overridden(&mut f, in_closed, proposal())
                .1
                .contains("closed")
        );

        let deep = f.draft("deep", Some(f.goal), 2);
        assert!(overridden(&mut f, deep, proposal()).1.contains("2 steps"));

        let bare = f.draft("bare", Some(f.goal), 1);
        let mut p = proposal();
        p.acceptance = " ".into();
        assert!(overridden(&mut f, bare, p).1.contains("acceptance"));
    }

    #[test]
    fn an_invalid_proposal_asks_without_offering_adopt() {
        let mut f = fixture();
        let missing = f.draft("missing dependency", Some(f.goal), 1);
        let mut p = proposal();
        p.depends_on = vec![999];
        let (ask, why) = overridden(&mut f, missing, p);
        assert!(why.contains("does not exist"), "{why}");
        assert_eq!(ask.options, ["cancel", "keep_draft"]);

        // A predecessor that waits on the draft would close a cycle.
        let cyclic = f.draft("cycle", Some(f.goal), 1);
        let waiting = f.queue.add(new_task("waiting", Some(f.goal))).unwrap().id();
        f.queue.add_dependency(waiting, cyclic).unwrap();
        let mut p = proposal();
        p.depends_on = vec![waiting.as_i64()];
        assert!(overridden(&mut f, cyclic, p).1.contains("cycle"));

        let canceled = f.queue.add(new_task("gone", Some(f.goal))).unwrap().id();
        f.queue.transition(canceled, TaskAction::Cancel).unwrap();
        let on_canceled = f.draft("on canceled", Some(f.goal), 1);
        let mut p = proposal();
        p.depends_on = vec![canceled.as_i64()];
        assert!(overridden(&mut f, on_canceled, p).1.contains("canceled"));

        let itself = f.draft("itself", Some(f.goal), 1);
        let mut p = proposal();
        p.depends_on = vec![itself.as_i64()];
        assert!(overridden(&mut f, itself, p).1.contains("the draft"));

        let bad = f.draft("bad evidence", Some(f.goal), 1);
        let mut p = proposal();
        p.evidence = vec!["lint".into()];
        assert!(overridden(&mut f, bad, p).1.contains("unknown evidence"));

        let none = f.draft("nothing proposed", Some(f.goal), 1);
        assert!(
            f.finish(none, verdict(FollowUpDecision::Adopt, None))
                .overridden
                .unwrap()
                .contains("proposed no task")
        );

        // An ask verdict with an invalid proposal offers no adopt either;
        // its question is the job's.
        let asked = f.draft("asked", Some(f.goal), 1);
        let mut v = verdict(
            FollowUpDecision::Ask,
            Some(FollowUpProposal {
                title: " ".into(),
                ..proposal()
            }),
        );
        v.question = "is this still needed?".into();
        let applied = f.finish(asked, v);
        assert_eq!(applied.overridden, None);
        let ask = applied.ask.unwrap().ask;
        assert_eq!(ask.options, ["cancel", "keep_draft"]);
        assert!(ask.question.contains("is this still needed?"));
        assert!(ask.question.contains("adopt is not offered"));
    }

    /// A draft whose job asked with a valid proposal, and the ask answered.
    fn answered(f: &mut Fixture, title: &str, answer: &str) -> (TaskId, AskId) {
        let draft = f.draft(title, Some(f.goal), 1);
        let applied = f.finish(draft, verdict(FollowUpDecision::Ask, Some(proposal())));
        let ask = applied.ask.unwrap().ask;
        assert_eq!(ask.options, FOLLOW_UP_OPTIONS);
        f.queue.answer(ask.id, answer).unwrap();
        (draft, ask.id)
    }

    fn delivers(f: &Fixture, draft: TaskId) -> Value {
        f.events(draft, "ask_answered")[0]["runtime_delivers"].clone()
    }

    #[test]
    fn answers_adopt_cancel_and_keep_draft_are_applied_and_close_the_ask() {
        let mut f = fixture();
        let (adopted, adopt_ask) = answered(&mut f, "adopt me", "adopt");
        let (canceled, cancel_ask) = answered(&mut f, "cancel me", " cancel ");
        let (kept, keep_ask) = answered(&mut f, "keep me", "keep_draft");
        for draft in [adopted, canceled, kept] {
            assert_eq!(delivers(&f, draft), true);
        }
        let asks: Vec<AskId> = f
            .queue
            .follow_up_answers()
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(asks, [adopt_ask, cancel_ask, keep_ask]);
        let ask = f.queue.read_ask(adopt_ask).unwrap();
        assert!(f.queue.applies_follow_up_answer(&ask).unwrap());

        let applied = f.queue.decide_follow_up(adopt_ask).unwrap().unwrap();
        assert_eq!(applied.action, FollowUpAction::Adopted);
        assert_eq!(applied.draft.status(), TaskStatus::Canceled);
        let new = applied.new_task.unwrap();
        assert_eq!(new.status(), TaskStatus::Ready);
        assert_eq!(new.goal_id(), Some(f.goal));
        assert_eq!(f.queue.follow_up_depth(new.id()).unwrap(), 0);
        assert!(
            new.context()
                .contains(&format!("ask {adopt_ask} の answer で人が adopt"))
        );
        let by = f.events(new.id(), "follow_up_adopted");
        assert_eq!(by[0]["by"], "person");
        assert_eq!(by[0]["ask_id"], adopt_ask.as_i64());
        assert_eq!(by[0]["depth"], 0);

        let applied = f.queue.decide_follow_up(cancel_ask).unwrap().unwrap();
        assert_eq!(applied.action, FollowUpAction::Dropped);
        assert_eq!(applied.draft.status(), TaskStatus::Canceled);

        let applied = f.queue.decide_follow_up(keep_ask).unwrap().unwrap();
        assert_eq!(applied.action, FollowUpAction::KeptDraft);
        assert_eq!(applied.draft.status(), TaskStatus::Draft);

        for (draft, ask, answer, status) in [
            (adopted, adopt_ask, "adopt", "canceled"),
            (canceled, cancel_ask, "cancel", "canceled"),
            (kept, keep_ask, "keep_draft", "draft"),
        ] {
            assert!(f.queue.read_ask(ask).unwrap().closed_at.is_some());
            let decided = f.events(draft, "follow_up_decided");
            assert_eq!(decided.len(), 1);
            assert_eq!(decided[0]["answer"], answer);
            assert_eq!(decided[0]["status"], status);
            // Never applied twice.
            assert!(f.queue.decide_follow_up(ask).is_err());
        }
        assert!(f.queue.follow_up_answers().unwrap().is_empty());
        // A kept draft is the planner's: no job looks at it again.
        assert!(f.targets().is_empty());
        assert!(matches!(
            f.queue.begin_follow_up_triage(kept, TOKEN).unwrap(),
            FollowUpStart::Skipped
        ));
    }

    #[test]
    fn a_person_adopting_into_a_closed_goal_registers_no_goal() {
        let mut f = fixture();
        let (_, ask) = answered(&mut f, "late", "adopt");
        f.queue
            .conn
            .execute(
                "UPDATE goals SET closed_at='now', verdict='achieved' WHERE id=?1",
                [f.goal],
            )
            .unwrap();
        let new = f
            .queue
            .decide_follow_up(ask)
            .unwrap()
            .unwrap()
            .new_task
            .unwrap();
        assert_eq!(new.goal_id(), None);
        assert_eq!(new.status(), TaskStatus::Ready);
    }

    #[test]
    fn answers_the_runtime_does_not_apply_are_left_to_a_person() {
        let mut f = fixture();
        let (free, free_ask) = answered(&mut f, "free", "let the planner split it");
        assert_eq!(delivers(&f, free), false);
        assert!(f.queue.decide_follow_up(free_ask).unwrap().is_none());
        assert!(f.queue.read_ask(free_ask).unwrap().closed_at.is_none());

        // adopt is not offered without a valid proposal.
        let draft = f.draft("no proposal", Some(f.goal), 1);
        let ask = f
            .finish(draft, verdict(FollowUpDecision::Ask, None))
            .ask
            .unwrap()
            .ask;
        f.queue.answer(ask.id, "adopt").unwrap();
        assert_eq!(delivers(&f, draft), false);
        assert!(f.queue.decide_follow_up(ask.id).unwrap().is_none());

        // A proposal that stopped being valid is not adopted.
        let dep = f.queue.add(new_task("dep", Some(f.goal))).unwrap().id();
        let stale = f.draft("stale", Some(f.goal), 1);
        let mut p = proposal();
        p.depends_on = vec![dep.as_i64()];
        let ask = f
            .finish(stale, verdict(FollowUpDecision::Ask, Some(p)))
            .ask
            .unwrap()
            .ask;
        f.queue.transition(dep, TaskAction::Cancel).unwrap();
        f.queue.answer(ask.id, "adopt").unwrap();
        assert_eq!(delivers(&f, stale), false);
        assert!(f.queue.decide_follow_up(ask.id).unwrap().is_none());

        // A draft a person readied meanwhile: the ask is only closed.
        let (readied, ready_ask) = answered(&mut f, "readied", "cancel");
        f.queue
            .transition(readied, TaskAction::BypassReview)
            .unwrap();
        assert_eq!(f.queue.follow_up_depth(readied).unwrap(), 0);
        let applied = f.queue.decide_follow_up(ready_ask).unwrap().unwrap();
        assert_eq!(applied.action, FollowUpAction::Closed);
        assert_eq!(applied.draft.status(), TaskStatus::Ready);
        assert!(f.queue.read_ask(ready_ask).unwrap().closed_at.is_some());

        // A leased draft waits; a non-follow_up ask is refused.
        let (leased, leased_ask) = answered(&mut f, "leased", "cancel");
        f.queue
            .conn
            .execute(
                "INSERT INTO task_leases(task_id, supervisor_token, reason, heartbeat_at) VALUES (?1,'other',?2,?3)",
                params![leased, FOLLOW_UP_TRIAGE, f.queue.generators.clock.now()],
            )
            .unwrap();
        assert!(f.queue.decide_follow_up(leased_ask).unwrap().is_none());
        let other = f
            .queue
            .ask(NewAsk {
                kind: AskKind::Decide,
                task_id: Some(f.source),
                run_id: None,
                question: "q".into(),
                options: Vec::new(),
                asked_by: "x".into(),
            })
            .unwrap()
            .ask;
        assert!(f.queue.decide_follow_up(other.id).is_err());
        assert!(!f.queue.applies_follow_up_answer(&other).unwrap());
    }

    #[test]
    fn targets_are_follow_up_drafts_not_yet_triaged_to_an_end() {
        let mut f = fixture();
        let manual = f.queue.add(new_task("by hand", Some(f.goal))).unwrap().id();
        let failed = f.draft("failed", Some(f.goal), 1);
        let pending = f.draft("pending", Some(f.goal), 1);
        let readied = f.draft("readied", Some(f.goal), 1);
        f.queue
            .transition(readied, TaskAction::BypassReview)
            .unwrap();
        // A skipped entry names no task.
        f.queue
            .record_runtime_event(
                &f.run,
                "follow_up_registered",
                json!({"task_id": null, "index": 9, "skipped": "blank"}),
            )
            .unwrap();
        assert_eq!(f.targets(), [failed, pending]);
        assert!(!f.targets().contains(&manual));

        f.started(failed);
        assert!(
            f.queue
                .fail_follow_up_triage(failed, TOKEN, &job(), "timed out")
                .unwrap()
        );
        let events = f.events(failed, "follow_up_triage_failed");
        assert_eq!(events[0]["error"], "timed out");
        assert_eq!(events[0]["status"], "draft");
        assert_eq!(f.targets(), [pending]);
    }

    #[test]
    fn one_follow_up_triage_runs_at_a_time_and_stale_leases_are_taken_over() {
        let mut f = fixture();
        let first = f.draft("first", Some(f.goal), 1);
        let second = f.draft("second", Some(f.goal), 1);
        assert_eq!(f.started(first), 1);
        // The queue's one slot is taken, and the draft is leased.
        assert!(matches!(
            f.queue.begin_follow_up_triage(second, "other").unwrap(),
            FollowUpStart::Skipped
        ));
        assert!(matches!(
            f.queue.begin_follow_up_triage(first, "other").unwrap(),
            FollowUpStart::Skipped
        ));
        assert!(
            !f.queue
                .fail_follow_up_triage(first, "other", &job(), "x")
                .unwrap()
        );
        // The heartbeat keeps it fresh.
        assert_eq!(f.queue.heartbeat(TOKEN).unwrap(), 1);
        // A supervisor that died leaves a stale lease another takes over
        // with the next attempt; the first can no longer apply its verdict.
        f.queue
            .conn
            .execute("UPDATE task_leases SET heartbeat_at=0", [])
            .unwrap();
        let FollowUpStart::Started { attempt, draft } =
            f.queue.begin_follow_up_triage(first, "other").unwrap()
        else {
            panic!("not taken over");
        };
        assert_eq!((attempt, draft.id()), (2, first));
        let acquired = f.events(first, "lease_acquired");
        assert_eq!(acquired[1]["previous_token"], TOKEN);
        assert_eq!(acquired[1]["reason"], FOLLOW_UP_TRIAGE);
        assert!(
            f.queue
                .finish_follow_up_triage(
                    first,
                    TOKEN,
                    &job(),
                    &verdict(FollowUpDecision::Drop, None)
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.queue.follow_up_drafts().unwrap()[0].status(),
            TaskStatus::Draft
        );
    }

    #[test]
    fn a_draft_decided_by_a_person_during_the_job_is_neither_applied_nor_failed() {
        let mut f = fixture();
        let draft = f.draft("raced", Some(f.goal), 1);
        f.started(draft);
        f.queue.transition(draft, TaskAction::Cancel).unwrap();
        let applied = f
            .queue
            .finish_follow_up_triage(draft, TOKEN, &job(), &verdict(FollowUpDecision::Drop, None))
            .unwrap()
            .unwrap();
        assert_eq!(applied.action, FollowUpAction::Closed);
        assert!(f.events(draft, "follow_up_triage_finished").is_empty());
        assert_eq!(f.events(draft, "lease_released").len(), 1);
        assert!(
            !f.queue
                .fail_follow_up_triage(draft, TOKEN, &job(), "x")
                .unwrap()
        );
        assert!(f.events(draft, "follow_up_triage_failed").is_empty());

        // A verdict already applied is not recorded as failed afterwards.
        let done = f.draft("done", Some(f.goal), 1);
        f.finish(done, verdict(FollowUpDecision::Ask, None));
        assert!(
            !f.queue
                .fail_follow_up_triage(done, TOKEN, &job(), "notify")
                .unwrap()
        );
        assert!(f.events(done, "follow_up_triage_failed").is_empty());
        // A lease still held is given up even then.
        f.queue
            .conn
            .execute(
                "INSERT INTO task_leases(task_id, supervisor_token, reason, heartbeat_at) VALUES (?1,?2,?3,?4)",
                params![
                    done,
                    TOKEN,
                    FOLLOW_UP_TRIAGE,
                    f.queue.generators.clock.now()
                ],
            )
            .unwrap();
        assert!(
            !f.queue
                .fail_follow_up_triage(done, TOKEN, &job(), "notify")
                .unwrap()
        );
        assert_eq!(f.events(done, "lease_released").len(), 2);
    }

    #[test]
    fn a_free_answer_about_a_draft_that_moved_on_is_closed_by_the_runtime() {
        let mut f = fixture();
        let (draft, ask) = answered(&mut f, "moved", "later");
        assert_eq!(delivers(&f, draft), false);
        f.queue.transition(draft, TaskAction::Cancel).unwrap();
        assert!(
            f.queue
                .applies_follow_up_answer(&f.queue.read_ask(ask).unwrap())
                .unwrap()
        );
        let applied = f.queue.decide_follow_up(ask).unwrap().unwrap();
        assert_eq!(applied.action, FollowUpAction::Closed);
    }

    #[test]
    fn an_adopt_the_store_refuses_rolls_back_into_an_ask() {
        let mut f = fixture();
        // A task that waits for the draft's whole goal: the adopted task,
        // in that goal, cannot depend on it (ADR-0038).
        let mut waiting = new_task("waits for the goal", None);
        waiting.goal_dependencies = vec![f.goal];
        let waiting = f.queue.add(waiting).unwrap().id();
        let draft = f.draft("follow", Some(f.goal), 1);
        let mut p = proposal();
        p.depends_on = vec![waiting.as_i64()];
        let tasks = |f: &Fixture| -> i64 {
            f.queue
                .conn
                .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
                .unwrap()
        };
        let before = tasks(&f);
        let (ask, why) = overridden(&mut f, draft, p);
        assert!(why.contains("could not be registered"), "{why}");
        assert_eq!(ask.options, ["cancel", "keep_draft"]);
        assert_eq!(tasks(&f), before);
        assert!(f.events(draft, "follow_up_adopted").is_empty());
    }

    #[test]
    fn a_draft_started_three_times_without_a_verdict_fails() {
        let mut f = fixture();
        let draft = f.draft("stuck", Some(f.goal), 1);
        for attempt in 1..=MAX_FOLLOW_UP_TRIAGE_ATTEMPTS {
            assert_eq!(f.started(draft), attempt);
            f.queue
                .conn
                .execute("UPDATE task_leases SET heartbeat_at=0", [])
                .unwrap();
        }
        assert!(matches!(
            f.queue.begin_follow_up_triage(draft, TOKEN).unwrap(),
            FollowUpStart::Exhausted { attempts: 3 }
        ));
        let failed = f.events(draft, "follow_up_triage_failed");
        assert!(failed[0]["error"].as_str().unwrap().contains("3 times"));
        assert!(f.targets().is_empty());
        let leases: i64 = f
            .queue
            .conn
            .query_row("SELECT count(*) FROM task_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(leases, 0);
    }

    #[test]
    fn follow_up_drafts_from_before_the_migration_are_targets_at_depth_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v21.db");
        let raw = Connection::open(&path).unwrap();
        for migration in &super::super::schema::MIGRATIONS[..21] {
            raw.execute_batch(migration).unwrap();
        }
        raw.pragma_update(None, "application_id", 0x43545131)
            .unwrap();
        raw.pragma_update(None, "user_version", 21).unwrap();
        raw.execute_batch(
            "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
             VALUES ('source','','','[]','completed'),('old draft','','','[]','draft'),
                    ('old readied','','','[]','ready'),('by hand','','','[]','draft');
             INSERT INTO run_events(task_id,kind,payload) VALUES
               (1,'follow_up_registered','{\"task_id\":2,\"index\":0}'),
               (1,'follow_up_registered','{\"task_id\":3,\"index\":1}'),
               (1,'follow_up_registered','{\"task_id\":null,\"index\":2}');
             INSERT INTO asks(kind,task_id,question,asked_by) VALUES ('decide',1,'q','x');",
        )
        .unwrap();
        drop(raw);
        SqliteQueue::migrate(&path, None, 0).unwrap();
        let queue = SqliteQueue::open(&path).unwrap();
        let targets: Vec<i64> = queue
            .follow_up_drafts()
            .unwrap()
            .iter()
            .map(|t| t.id().as_i64())
            .collect();
        assert_eq!(targets, [2]);
        let depths: Vec<i64> = (1..=4)
            .map(|id| queue.follow_up_depth(TaskId::new(id)).unwrap())
            .collect();
        assert_eq!(depths, [0, 1, 0, 0]);
        // The asks survive the rebuild with their IDs.
        assert_eq!(queue.read_ask(AskId::new(1)).unwrap().kind, AskKind::Decide);
    }
}
