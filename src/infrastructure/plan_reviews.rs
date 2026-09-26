//! Plan review (ADR-0041 decisions 11-15, 17): the `plan_reviews` rows of
//! the headless job (one unfinished at a time, queue-wide), the plan-review
//! columns of `proposals` (a hold, and the revise on its way to a planner),
//! and the verdicts and `approve_plan` answers the supervisor applies, each
//! in one transaction. Events go to the first task of the proposal, since
//! an event needs a task or a goal.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use tracing::warn;

use super::{
    asks::{insert_ask, read_ask},
    proposals, sessions,
    sqlite::{SqliteQueue, enum_col, event, insert_dependency, read_task, transition_task},
};
use crate::{
    application::{
        PlanDecided, PlanReviewApplied, PlanReviewApply, PlanReviewHold, PlanReviewJob,
        PlanReviewStore, ReopenedTask, RevisingProposal,
    },
    domain::{
        Ask, AskId, AskKind, HEARTBEAT_TIMEOUT_SECS, PlanAnswer, PlanReviewAction,
        PlanReviewCandidate, PlanReviewDecision, PlannerId, Priority, ProposalId, ProposalStatus,
        TaskAction, TaskId, TaskStatus,
        plan_quality::{self, ProposalFeatures},
        proposal, task,
    },
};

/// The first task of a proposal, where its plan review events go.
fn anchor(conn: &Connection, proposal_id: ProposalId) -> Result<TaskId> {
    conn.query_row(
        "SELECT min(id) FROM tasks WHERE proposal_id=?1",
        [proposal_id],
        |r| r.get::<_, Option<TaskId>>(0),
    )?
    .with_context(|| format!("proposal {proposal_id} has no task"))
}

fn hold(conn: &Connection, proposal_id: ProposalId) -> Result<Option<String>> {
    Ok(conn.query_row(
        "SELECT review_hold FROM proposals WHERE id=?1",
        [proposal_id],
        |r| r.get(0),
    )?)
}

fn set_hold(conn: &Connection, proposal_id: ProposalId, hold: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE proposals SET review_hold=?2 WHERE id=?1",
        params![proposal_id, hold],
    )?;
    Ok(())
}

/// The revise `reasons` wait for the supervisor to deliver them, from
/// `now` on.
fn await_delivery(
    conn: &Connection,
    proposal_id: ProposalId,
    reasons: &[String],
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE proposals SET revise_reasons=?2, revised_at=?3, revise_sent_at=NULL,
             revise_planner_id=NULL, unresponsive_at=NULL, review_hold=NULL WHERE id=?1",
        params![proposal_id, serde_json::to_string(reasons)?, now],
    )?;
    Ok(())
}

/// The job's row, while it is unfinished and still `token`'s.
fn running(conn: &Connection, job: &PlanReviewJob, token: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM plan_reviews WHERE id=?1 AND supervisor_token=?2
         AND finished_at IS NULL)",
        params![job.id, token],
        |r| r.get(0),
    )?)
}

fn finish_row(
    conn: &Connection,
    id: i64,
    now: i64,
    outcome: &str,
    verdict: Option<&Value>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE plan_reviews SET finished_at=?2, outcome=?3, verdict=?4, error=?5
         WHERE id=?1 AND finished_at IS NULL",
        params![id, now, outcome, verdict.map(Value::to_string), error],
    )?;
    Ok(())
}

/// Whether the proposal is still the submitted, unheld one the job took.
fn reviewable(conn: &Connection, proposal_id: ProposalId) -> Result<bool> {
    Ok(
        proposals::read(conn, proposal_id)?.status() == ProposalStatus::Submitted
            && hold(conn, proposal_id)?.is_none(),
    )
}

/// The tasks of the proposal edited since the job started (ADR-0041
/// decision 9): its verdict is of their old contents. Event ids order the
/// edits against the job's `plan_review_started`.
fn edited_during(conn: &Connection, job: &PlanReviewJob) -> Result<Vec<TaskId>> {
    Ok(conn
        .prepare(
            "SELECT DISTINCT e.task_id FROM run_events e JOIN tasks t ON t.id = e.task_id
             WHERE e.kind='task_edited' AND t.proposal_id=?1 AND e.id > (
                 SELECT id FROM run_events WHERE task_id=?2 AND kind='plan_review_started'
                 AND json_extract(payload,'$.plan_review_id')=?3)
             ORDER BY e.task_id",
        )?
        .query_map(params![job.proposal_id, job.anchor, job.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Check an action against the proposal before anything is applied: it
/// changes a submitted task of the proposal, a dependency names another
/// task, a priority only goes down, and a duplicate is of another task
/// that is not canceled.
fn check_action(conn: &Connection, members: &[TaskId], action: &PlanReviewAction) -> Result<()> {
    let task_id = action.task_id();
    ensure!(
        members.contains(&task_id),
        "plan review may change only the tasks of the proposal, not task {task_id}"
    );
    let target = read_task(conn, task_id)?;
    ensure!(
        target.status() == TaskStatus::Submitted,
        "task {task_id} is {}, not submitted",
        target.status().as_str()
    );
    match action {
        PlanReviewAction::AddDependency { depends_on, .. } => {
            task::check_not_self(task_id, *depends_on)?;
            read_task(conn, *depends_on)?;
        }
        PlanReviewAction::LowerPriority { priority, .. } => ensure!(
            *priority < target.priority(),
            "plan review may only lower the priority of task {task_id} ({}), not set it to {}",
            target.priority().as_str(),
            priority.as_str()
        ),
        PlanReviewAction::CancelDuplicate { duplicate_of, .. } => {
            ensure!(
                duplicate_of != &task_id,
                "task {task_id} cannot duplicate itself"
            );
            ensure!(
                read_task(conn, *duplicate_of)?.status() != TaskStatus::Canceled,
                "task {duplicate_of} is canceled; task {task_id} duplicates nothing live"
            );
        }
    }
    Ok(())
}

fn apply_action(conn: &Connection, action: &PlanReviewAction, now: &str) -> Result<()> {
    match action {
        PlanReviewAction::AddDependency {
            task_id,
            depends_on,
        } => insert_dependency(conn, *task_id, *depends_on, now),
        PlanReviewAction::LowerPriority { task_id, priority } => {
            let from = read_task(conn, *task_id)?.priority();
            set_priority(conn, *task_id, from, *priority, now)
        }
        PlanReviewAction::CancelDuplicate {
            task_id,
            duplicate_of,
        } => {
            transition_task(conn, *task_id, TaskAction::Cancel, now)?;
            event(
                conn,
                *task_id,
                None,
                "task_canceled_as_duplicate",
                json!({"duplicate_of": duplicate_of, "by": "plan_review"}),
            )
        }
    }
}

fn set_priority(
    conn: &Connection,
    task_id: TaskId,
    from: Priority,
    to: Priority,
    now: &str,
) -> Result<()> {
    let changed = task::set_priority(read_task(conn, task_id)?, to)?;
    conn.execute(
        "UPDATE tasks SET priority=?1, updated_at=?2 WHERE id=?3",
        params![changed.priority().as_i64(), now, task_id],
    )?;
    event(
        conn,
        task_id,
        None,
        "task_priority_changed",
        json!({"from": from, "to": to, "by": "plan_review"}),
    )
}

/// Take the ready task out of the claim into a proposal of its own that
/// waits for a planner with `reason` (ADR-0041 decision 14). A task that is
/// not ready, or belongs to an active proposal, is skipped with why.
fn reopen(
    conn: &Connection,
    task_id: TaskId,
    reason: &str,
    reviewed: ProposalId,
    now: &str,
    at: i64,
) -> Result<std::result::Result<ReopenedTask, String>> {
    let Some((status, current)) = conn
        .query_row(
            "SELECT t.status, p.id, p.status FROM tasks t
             LEFT JOIN proposals p ON p.id = t.proposal_id WHERE t.id=?1",
            [task_id],
            |r| {
                let proposal: Option<ProposalId> = r.get(1)?;
                let active = match r.get::<_, Option<String>>(2)? {
                    Some(status) => status
                        .parse::<ProposalStatus>()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?
                        .is_active(),
                    None => false,
                };
                Ok((
                    enum_col::<TaskStatus>(r, "status")?,
                    proposal.filter(|_| active),
                ))
            },
        )
        .optional()?
    else {
        return Ok(Err(format!("task {task_id} does not exist")));
    };
    if status != TaskStatus::Ready {
        return Ok(Err(format!(
            "task {task_id} is {}, not ready: it is not changed now",
            status.as_str()
        )));
    }
    if let Some(current) = current {
        return Ok(Err(format!(
            "task {task_id} is in proposal {current}, still under plan review or revise: it is not moved"
        )));
    }
    let id = ProposalId::new(super::sqlite::next_id(conn, "proposals")?);
    let reopened = proposal::reopen(id, vec![task_id], now.into())?;
    proposals::save(conn, &reopened)?;
    transition_task(conn, task_id, TaskAction::Reopen, now)?;
    conn.execute(
        "UPDATE tasks SET proposal_id=?1 WHERE id=?2",
        params![id, task_id],
    )?;
    await_delivery(
        conn,
        id,
        &[format!(
            "plan review of proposal {reviewed} found that ready task {task_id} has to change: {reason}"
        )],
        at,
    )?;
    event(
        conn,
        task_id,
        None,
        "task_reopened",
        json!({"proposal_id": id, "reviewed_proposal_id": reviewed, "reason": reason}),
    )?;
    Ok(Ok(ReopenedTask {
        task_id,
        proposal_id: id,
    }))
}

/// Whether the answer `text` of `ask` is one the supervisor applies: one
/// of the plan options, while the proposal of the ask's task is still
/// submitted and held for this concern.
pub(super) fn plan_answer_applies(conn: &Connection, ask: &Ask, text: &str) -> Result<bool> {
    if ask.kind != AskKind::ApprovePlan || PlanAnswer::parse(text).is_none() {
        return Ok(false);
    }
    let Some(proposal_id) = ask_proposal(conn, ask)? else {
        return Ok(false);
    };
    Ok(
        proposals::read(conn, proposal_id)?.status() == ProposalStatus::Submitted
            && hold(conn, proposal_id)?.as_deref() == Some("concern"),
    )
}

fn ask_proposal(conn: &Connection, ask: &Ask) -> Result<Option<ProposalId>> {
    let Some(task_id) = ask.task_id else {
        return Ok(None);
    };
    Ok(conn.query_row(
        "SELECT proposal_id FROM tasks WHERE id=?1",
        [task_id],
        |r| r.get(0),
    )?)
}

/// How many follow-ups deep a draft is: 0 for none, 1 for a follow-up of a
/// task that is none, and so on; bounded so a cycle ends.
fn follow_up_depth(conn: &Connection, task_id: TaskId) -> Result<i64> {
    let mut depth = 0;
    let mut task = task_id.as_i64();
    while depth < 32 {
        let source: Option<Option<i64>> = conn
            .query_row(
                "SELECT json_extract(material, '$.source_task_id') FROM draft_origins
                 WHERE task_id=?1 AND origin='follow_up'",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        let Some(source) = source else {
            break;
        };
        depth += 1;
        let Some(source) = source else {
            break;
        };
        task = source;
    }
    Ok(depth)
}

impl SqliteQueue {
    /// What `proposal_id` is like now (ADR-0079 decision 7): where it came
    /// from, how deep its follow-ups go, how close its closest existing
    /// task is (`dagq related`), and how often plan review sent it back.
    /// `None` when it does not exist.
    fn proposal_features(&self, proposal_id: ProposalId) -> Result<Option<ProposalFeatures>> {
        let exists: bool = self.conn.query_row(
            "SELECT count(*) > 0 FROM proposals WHERE id=?1",
            [proposal_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let proposal = proposals::read(&self.conn, proposal_id)?;
        let members = proposal.task_ids();
        let has = |sql: &str| -> Result<bool> {
            Ok(self.conn.query_row(sql, [proposal_id], |r| r.get(0))?)
        };
        let origin = plan_quality::origin(
            has(
                "SELECT count(*) > 0 FROM draft_origins d JOIN tasks t ON t.id = d.task_id
                 WHERE t.proposal_id=?1 AND d.origin='follow_up'",
            )?,
            has(
                "SELECT count(*) > 0 FROM draft_origins d JOIN tasks t ON t.id = d.task_id
                 WHERE t.proposal_id=?1 AND d.origin='goal_gap'",
            )?,
            has("SELECT count(*) > 0 FROM findings WHERE proposal_id=?1")?,
            proposal.owner().origin,
        );
        let mut follow_up = 0;
        let mut related: Option<f64> = None;
        for &task in members {
            follow_up = follow_up.max(follow_up_depth(&self.conn, task)?);
            let page = self.related(task.as_i64(), &[], members.len() + 1)?;
            let best = page
                .related
                .iter()
                .find(|other| !members.contains(&TaskId::new(other.id)))
                .map(|other| other.score);
            if let Some(best) = best {
                related = Some(related.map_or(best, |kept| kept.max(best)));
            }
        }
        Ok(Some(ProposalFeatures {
            origin,
            follow_up_depth: follow_up,
            related_score: related,
            revise_count: i64::from(proposal.revise_count()),
        }))
    }
}

impl PlanReviewStore for SqliteQueue {
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>> {
        candidates(&self.conn)
    }

    fn begin_plan_review(
        &mut self,
        proposal_id: ProposalId,
        token: &str,
        plan_reviews_dir: &Path,
        cwd: &Path,
    ) -> Result<Option<PlanReviewJob>> {
        let now = self.generators.clock.now();
        // What the proposal is like, read before the write lock (task
        // 579); a failure leaves it unrecorded, not the review undone.
        let features = match self.proposal_features(proposal_id) {
            Ok(features) => features.map(|features| features.payload()),
            Err(error) => {
                warn!("proposal {proposal_id}: features not recorded: {error:#}");
                None
            }
        };
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(&self.conn, sessions::Closing::PlanReviews(None))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unfinished: Vec<(i64, String, Option<i64>)> = tx
            .prepare(
                "SELECT r.id, r.supervisor_token, s.heartbeat_at FROM plan_reviews r
                 LEFT JOIN supervisors s ON s.token = r.supervisor_token
                 WHERE r.finished_at IS NULL",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, owner, heartbeat) in unfinished {
            let live =
                owner != token && heartbeat.is_some_and(|at| now - at <= HEARTBEAT_TIMEOUT_SECS);
            if live {
                return Ok(None);
            }
            finish_row(
                &tx,
                id,
                now,
                "interrupted",
                None,
                Some("its supervisor is gone"),
            )?;
            sessions::close_plan_review(&tx, id, true)?;
        }
        let candidate = candidates(&tx)?;
        if !candidate.iter().any(|c| c.proposal_id == proposal_id) {
            // Keep the rows finished above.
            tx.commit()?;
            return Ok(None);
        }
        let attempt: usize = tx.query_row(
            "SELECT count(*) + 1 FROM plan_reviews WHERE proposal_id=?1 AND outcome IS NOT 'interrupted'",
            [proposal_id],
            |r| r.get::<_, i64>(0),
        )? as usize;
        tx.execute(
            "INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![proposal_id, attempt as i64, token, now],
        )?;
        let id = tx.last_insert_rowid();
        let dir = plan_reviews_dir.join(id.to_string());
        let dir_text = dir.to_str().context("plan review directory is not UTF-8")?;
        tx.execute(
            "UPDATE plan_reviews SET dir=?2 WHERE id=?1",
            params![id, dir_text],
        )?;
        let anchor = anchor(&tx, proposal_id)?;
        // The job's Claude session id, given to it by the runtime (ADR-0048
        // decision 4).
        let session_id = self.generators.ids.uuid();
        let cwd = cwd.to_str().context("repository checkout is not UTF-8")?;
        event(
            &tx,
            anchor,
            None,
            "plan_review_started",
            json!({"proposal_id": proposal_id, "plan_review_id": id, "attempt": attempt, "dir": dir_text, "session_id": session_id, "cwd": cwd, "features": features}),
        )?;
        tx.commit()?;
        Ok(Some(PlanReviewJob {
            id,
            proposal_id,
            attempt,
            anchor,
            dir,
            session_id,
        }))
    }

    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &str,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        // The spans it closes read their transcripts first (task 543).
        let _read =
            sessions::read_before(&self.conn, sessions::Closing::PlanReviews(Some(job.id)))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(PlanReviewApplied {
                stale: true,
                ..PlanReviewApplied::default()
            });
        }
        let verdict_json = serde_json::to_value(&apply.verdict)?;
        if !reviewable(&tx, job.proposal_id)? {
            finish_row(
                &tx,
                job.id,
                now,
                "interrupted",
                Some(&verdict_json),
                Some("the proposal moved on during its review"),
            )?;
            sessions::close_plan_review(&tx, job.id, false)?;
            tx.commit()?;
            return Ok(PlanReviewApplied {
                stale: true,
                ..PlanReviewApplied::default()
            });
        }
        let edited = edited_during(&tx, job)?;
        if !edited.is_empty() {
            // The proposal stays submitted and unheld: the next pass
            // reviews the edited tasks.
            let error = format!(
                "task {} of the proposal was edited during its review",
                edited
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            finish_row(
                &tx,
                job.id,
                now,
                "interrupted",
                Some(&verdict_json),
                Some(&error),
            )?;
            sessions::close_plan_review(&tx, job.id, false)?;
            event(
                &tx,
                job.anchor,
                None,
                "plan_review_discarded",
                json!({
                    "proposal_id": job.proposal_id,
                    "plan_review_id": job.id,
                    "attempt": job.attempt,
                    "verdict": apply.verdict.verdict,
                    "edited": edited,
                    "error": error,
                }),
            )?;
            tx.commit()?;
            return Ok(PlanReviewApplied {
                stale: true,
                ..PlanReviewApplied::default()
            });
        }
        let members = proposals::read(&tx, job.proposal_id)?.task_ids().to_vec();
        if apply.decision == PlanReviewDecision::Pass {
            for action in &apply.verdict.actions {
                check_action(&tx, &members, action)?;
            }
            // A task canceled as a duplicate is no original of another.
            let canceled: Vec<TaskId> = apply
                .verdict
                .actions
                .iter()
                .filter(|a| matches!(a, PlanReviewAction::CancelDuplicate { .. }))
                .map(PlanReviewAction::task_id)
                .collect();
            for action in &apply.verdict.actions {
                if let PlanReviewAction::CancelDuplicate { duplicate_of, .. } = action {
                    ensure!(
                        !canceled.contains(duplicate_of),
                        "task {duplicate_of} is canceled as a duplicate itself; it is no original"
                    );
                }
            }
        }
        for reopen in &apply.verdict.reopen {
            ensure!(
                !members.contains(&reopen.task_id),
                "task {} is in the proposal under review, not a ready task to reopen",
                reopen.task_id
            );
        }
        let mut applied = PlanReviewApplied::default();
        let mut skipped = Vec::new();
        match apply.decision {
            PlanReviewDecision::Pass => {
                for action in &apply.verdict.actions {
                    apply_action(&tx, action, &stamp)?;
                }
                proposals::approve(&tx, job.proposal_id, &stamp)?;
            }
            PlanReviewDecision::Revise => {
                proposals::send_back(&tx, job.proposal_id, &stamp)?;
                await_delivery(&tx, job.proposal_id, &apply.revise_reasons, now)?;
            }
            PlanReviewDecision::Concern => {
                set_hold(&tx, job.proposal_id, Some("concern"))?;
                let ask = apply
                    .ask
                    .as_ref()
                    .context("a concern opens an approve_plan ask")?;
                ask.validate()?;
                applied.ask = Some(insert_ask(&tx, ask)?);
            }
        }
        for reopen_task in &apply.verdict.reopen {
            match reopen(
                &tx,
                reopen_task.task_id,
                &reopen_task.reason,
                job.proposal_id,
                &stamp,
                now,
            )? {
                Ok(reopened) => applied.reopened.push(reopened),
                Err(why) => skipped.push(why),
            }
        }
        finish_row(
            &tx,
            job.id,
            now,
            apply.decision.as_str(),
            Some(&verdict_json),
            None,
        )?;
        event(
            &tx,
            job.anchor,
            None,
            "plan_review_finished",
            json!({
                "proposal_id": job.proposal_id,
                "plan_review_id": job.id,
                "attempt": job.attempt,
                "verdict": apply.verdict.verdict,
                "decision": apply.decision,
                "overridden": apply.overridden,
                "reasons": apply.verdict.reasons,
                "summary": apply.verdict.summary,
                "actions": apply.verdict.actions,
                "reopened": applied.reopened,
                "reopen_skipped": skipped,
                "precedents": apply.verdict.precedents,
                "ask_id": applied.ask.as_ref().map(|outcome| outcome.ask.id),
                "duration_secs": apply.duration_secs,
            }),
        )?;
        tx.commit()?;
        Ok(applied)
    }

    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &str,
        error: &str,
        duration_secs: u64,
    ) -> Result<()> {
        let now = self.generators.clock.now();
        // The spans it closes read their transcripts first (task 543).
        let _read =
            sessions::read_before(&self.conn, sessions::Closing::PlanReviews(Some(job.id)))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(());
        }
        if !reviewable(&tx, job.proposal_id)? {
            finish_row(&tx, job.id, now, "interrupted", None, Some(error))?;
            sessions::close_plan_review(&tx, job.id, false)?;
            tx.commit()?;
            return Ok(());
        }
        finish_row(&tx, job.id, now, "failed", None, Some(error))?;
        set_hold(&tx, job.proposal_id, Some("failed"))?;
        event(
            &tx,
            job.anchor,
            None,
            "plan_review_failed",
            json!({
                "code": crate::domain::ReasonCode::JobFailed,
                "proposal_id": job.proposal_id,
                "plan_review_id": job.id,
                "attempt": job.attempt,
                "error": error,
                "duration_secs": duration_secs,
                "status": TaskStatus::Submitted.as_str(),
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>> {
        let tx = self.conn.unchecked_transaction()?;
        let ids: Vec<ProposalId> = tx
            .prepare("SELECT id FROM proposals WHERE status='revising' ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                let (reasons, revised_at, sent_at, planner_id, unresponsive_at) = tx.query_row(
                    "SELECT revise_reasons, revised_at, revise_sent_at, revise_planner_id,
                            unresponsive_at FROM proposals WHERE id=?1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                        ))
                    },
                )?;
                Ok(RevisingProposal {
                    proposal: proposals::read(&tx, id)?,
                    reasons: reasons
                        .map(|text| serde_json::from_str(&text))
                        .transpose()?
                        .unwrap_or_default(),
                    revised_at,
                    sent_at,
                    planner_id,
                    unresponsive_at,
                })
            })
            .collect()
    }

    fn claim_revise(&mut self, proposal_id: ProposalId) -> Result<bool> {
        let now = self.generators.clock.now();
        Ok(self.conn.execute(
            "UPDATE proposals SET revise_sent_at=?2, revise_planner_id=NULL
             WHERE id=?1 AND status='revising' AND revise_sent_at IS NULL",
            params![proposal_id, now],
        )? == 1)
    }

    fn revise_sent(
        &mut self,
        proposal_id: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: bool,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE proposals SET revise_planner_id=?2, unresponsive_at=NULL
             WHERE id=?1 AND status='revising'",
            params![proposal_id, planner],
        )?;
        event(
            &tx,
            anchor(&tx, proposal_id)?,
            None,
            "plan_revise_sent",
            json!({"proposal_id": proposal_id, "planner_id": planner, "workspace_id": workspace, "opened": opened}),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn revise_lost(
        &mut self,
        proposal_id: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE proposals SET revise_sent_at=NULL, revise_planner_id=NULL, unresponsive_at=NULL
             WHERE id=?1 AND status='revising' AND revise_sent_at IS NOT NULL",
            [proposal_id],
        )?;
        if changed == 1 && planner.is_some() {
            event(
                &tx,
                anchor(&tx, proposal_id)?,
                None,
                "plan_revise_lost",
                json!({"proposal_id": proposal_id, "planner_id": planner, "reason": why}),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn planner_unresponsive(
        &mut self,
        proposal_id: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
    ) -> Result<()> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE proposals SET unresponsive_at=?2
             WHERE id=?1 AND status='revising' AND unresponsive_at IS NULL",
            params![proposal_id, now],
        )?;
        if changed == 1 {
            let reason = match planner {
                Some(planner) => format!(
                    "planner {planner} did not submit proposal {proposal_id} again within {waited_secs} seconds of its revise"
                ),
                None => format!(
                    "the revise of proposal {proposal_id} waited {waited_secs} seconds for a planner to take it"
                ),
            };
            event(
                &tx,
                anchor(&tx, proposal_id)?,
                None,
                "planner_unresponsive",
                json!({
                    "proposal_id": proposal_id,
                    "planner_id": planner,
                    "waited_secs": waited_secs,
                    "reason": reason,
                }),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, ProposalStatus)>> {
        let stamp = self.generators.clock.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let emptied: Vec<ProposalId> = tx
            .prepare(
                "SELECT p.id FROM proposals p WHERE p.status='submitted'
                 AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.proposal_id = p.id
                                 AND t.status='submitted')
                 AND NOT EXISTS (SELECT 1 FROM plan_reviews r WHERE r.proposal_id = p.id
                                 AND r.finished_at IS NULL)
                 ORDER BY p.id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut settled = Vec::new();
        for id in emptied {
            let live: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE proposal_id=?1 AND status!='canceled')",
                [id],
                |r| r.get(0),
            )?;
            let current = proposals::read(&tx, id)?;
            let ended = if live {
                proposal::accept(current, stamp.clone())?
            } else {
                proposal::cancel(current, stamp.clone())?
            };
            proposals::save(&tx, &ended)?;
            set_hold(&tx, id, None)?;
            if let Ok(anchor) = anchor(&tx, id) {
                event(
                    &tx,
                    anchor,
                    None,
                    "proposal_settled",
                    json!({"proposal_id": id, "status": ended.status(), "reason": "none of its tasks waits for plan review any more"}),
                )?;
            }
            settled.push((id, ended.status()));
        }
        tx.commit()?;
        Ok(settled)
    }

    fn plan_answers(&self) -> Result<Vec<Ask>> {
        let ids: Vec<AskId> = self
            .conn
            .prepare(
                "SELECT id FROM asks WHERE kind='approve_plan' AND answered_at IS NOT NULL
                 AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| read_ask(&self.conn, id)).collect()
    }

    fn decide_plan(&mut self, ask_id: AskId) -> Result<Option<PlanDecided>> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, ask_id)?;
        ensure!(
            ask.kind == AskKind::ApprovePlan,
            "ask {ask_id} is not an approve_plan ask"
        );
        let text = ask.answer.clone().unwrap_or_default();
        let Some(answer) = PlanAnswer::parse(&text) else {
            return Ok(None);
        };
        if ask.closed_at.is_some() {
            return Ok(None);
        }
        let close = |tx: &Connection| -> Result<()> {
            tx.execute(
                "UPDATE asks SET closed_at=?2 WHERE id=?1",
                params![ask_id, now],
            )?;
            Ok(())
        };
        if !plan_answer_applies(&tx, &ask, &text)? {
            close(&tx)?;
            tx.commit()?;
            return Ok(None);
        }
        let proposal_id = ask_proposal(&tx, &ask)?.context("the ask's task has no proposal")?;
        set_hold(&tx, proposal_id, None)?;
        let decided = match &answer {
            PlanAnswer::Ready => proposals::approve(&tx, proposal_id, &stamp)?,
            PlanAnswer::SendBack(reason) => {
                let sent = proposals::send_back(&tx, proposal_id, &stamp)?;
                let mut reasons = vec![match reason {
                    Some(reason) => {
                        format!("a person sent the proposal back in ask {ask_id}: {reason}")
                    }
                    None => format!(
                        "a person sent the proposal back in ask {ask_id} for plan review's findings"
                    ),
                }];
                reasons.extend(latest_reasons(&tx, proposal_id)?);
                await_delivery(&tx, proposal_id, &reasons, now)?;
                sent
            }
            PlanAnswer::Cancel => {
                let canceled = proposal::cancel(proposals::read(&tx, proposal_id)?, stamp.clone())?;
                proposals::save(&tx, &canceled)?;
                for &task_id in canceled.task_ids() {
                    if read_task(&tx, task_id)?.status() == TaskStatus::Submitted {
                        transition_task(&tx, task_id, TaskAction::Cancel, &stamp)?;
                    }
                }
                proposals::read(&tx, proposal_id)?
            }
        };
        close(&tx)?;
        event(
            &tx,
            anchor(&tx, proposal_id)?,
            None,
            "plan_decided",
            json!({"proposal_id": proposal_id, "ask_id": ask_id, "answer": text.trim(), "status": decided.status()}),
        )?;
        tx.commit()?;
        Ok(Some(PlanDecided {
            proposal: decided,
            answer: text.trim().to_owned(),
        }))
    }

    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool> {
        match &ask.answer {
            Some(text) if ask.closed_at.is_none() => plan_answer_applies(&self.conn, ask, text),
            _ => Ok(false),
        }
    }

    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>> {
        let rows: Vec<(ProposalId, bool, Option<String>)> = self
            .conn
            .prepare(
                "SELECT p.id, p.status='submitted',
                        (SELECT error FROM plan_reviews r WHERE r.proposal_id = p.id
                         ORDER BY r.id DESC LIMIT 1)
                 FROM proposals p
                 WHERE (p.status='submitted' AND p.review_hold='failed')
                    OR (p.status='submitted' AND p.review_hold='concern' AND NOT EXISTS (
                        SELECT 1 FROM asks a JOIN tasks t ON t.id = a.task_id
                        WHERE t.proposal_id = p.id AND a.kind='approve_plan'
                          AND a.closed_at IS NULL))
                    OR (p.status='revising' AND p.unresponsive_at IS NOT NULL)
                 ORDER BY p.id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(proposal_id, failed, error)| {
                // A proposal with no task left has nothing to show on.
                let anchor = anchor(&self.conn, proposal_id).ok()?;
                Some(PlanReviewHold {
                    proposal_id,
                    anchor,
                    kind: if failed {
                        "plan_review_failed"
                    } else {
                        "planner_unresponsive"
                    },
                    error: if failed { error } else { None },
                })
            })
            .collect())
    }

    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>> {
        let ids: Vec<AskId> = self
            .conn
            .prepare(
                "SELECT id FROM asks WHERE answered_at IS NOT NULL ORDER BY answered_at DESC, id DESC
                 LIMIT ?1",
            )?
            .query_map([i64::try_from(limit)?], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| read_ask(&self.conn, id)).collect()
    }
}

/// Submitted proposals with no hold and a submitted task, oldest
/// submission first, each with whether a submitted task is `interrupt`.
fn candidates(conn: &Connection) -> Result<Vec<PlanReviewCandidate>> {
    Ok(conn
        .prepare(
            "SELECT p.id, p.submitted_at, max(t.status='submitted' AND t.priority=?1)
             FROM proposals p JOIN tasks t ON t.proposal_id = p.id
             WHERE p.status='submitted' AND p.review_hold IS NULL
             GROUP BY p.id HAVING max(t.status='submitted')
             ORDER BY p.submitted_at, p.id",
        )?
        .query_map([Priority::Interrupt.as_i64()], |r| {
            Ok(PlanReviewCandidate {
                proposal_id: r.get(0)?,
                submitted_at: r.get(1)?,
                interrupt: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The reasons of the proposal's latest finished review with a verdict.
fn latest_reasons(conn: &Connection, proposal_id: ProposalId) -> Result<Vec<String>> {
    let verdict: Option<String> = conn
        .query_row(
            "SELECT verdict FROM plan_reviews WHERE proposal_id=?1 AND verdict IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [proposal_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(verdict) = verdict else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_str(&verdict)?;
    match value.get("reasons") {
        Some(Value::Array(items)) => Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()),
        Some(_) => bail!("plan review verdict of proposal {proposal_id} has malformed reasons"),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::TaskStore,
        domain::{NewTask, PlannerOrigin, PlannerOwner, Submission},
    };

    /// The job's checkout is on its start event, and so on the span of its
    /// Claude session (ADR-0048).
    #[test]
    fn a_plan_review_records_the_checkout_it_runs_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = queue
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
            .id();
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![task_id],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        let job = queue
            .begin_plan_review(
                proposal_id,
                "token",
                &dir.path().join("plan-reviews"),
                Path::new("/repo"),
            )
            .unwrap()
            .unwrap();
        let payload = |kind: &str| -> Value {
            let text: String = queue
                .conn
                .query_row(
                    "SELECT payload FROM run_events WHERE kind=?1",
                    [kind],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let started = payload("plan_review_started");
        assert_eq!(started["cwd"], "/repo");
        assert_eq!(started["session_id"], job.session_id.as_str());
        assert_eq!(started["plan_review_id"], job.id);
        let opened = payload("session_opened");
        assert_eq!(opened["kind"], "plan_review");
        assert_eq!(opened["cwd"], "/repo");
        // A person's proposal of one task close to nothing (task 579).
        assert_eq!(
            started["features"],
            json!({"origin": "person", "follow_up_depth": 0, "related_score": null,
                   "related": "low", "revise_count": 0})
        );
    }

    fn new_task(title: &str) -> NewTask {
        NewTask {
            title: title.into(),
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
        }
    }

    /// A proposal of a follow-up of a follow-up is `follow_up` two deep,
    /// and its closest task outside it scores; its own tasks do not.
    #[test]
    fn a_plan_review_records_what_its_proposal_is_like() {
        use crate::domain::DraftOrigin;
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let source = queue
            .add(new_task("stats: count the widget gadgets per window"))
            .unwrap()
            .id();
        let first = queue.add(new_task("widget gadgets")).unwrap().id();
        let second = queue.add(new_task("widget gadgets again")).unwrap().id();
        for (task, of) in [(first, source), (second, first)] {
            queue
                .record_draft_origin(
                    task,
                    DraftOrigin::FollowUp,
                    &json!({"source_task_id": of.as_i64()}),
                )
                .unwrap();
        }
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![first, second],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        queue
            .begin_plan_review(
                proposal_id,
                "token",
                &dir.path().join("plan-reviews"),
                Path::new("/repo"),
            )
            .unwrap()
            .unwrap();
        let text: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind='plan_review_started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let features = &serde_json::from_str::<Value>(&text).unwrap()["features"];
        assert_eq!(features["origin"], "follow_up");
        assert_eq!(features["follow_up_depth"], 2);
        assert_eq!(features["revise_count"], 0);
        let best = queue.related(first.as_i64(), &[], 5).unwrap();
        let outside = best
            .related
            .iter()
            .find(|task| task.id == source.as_i64())
            .unwrap()
            .score;
        let score = features["related_score"].as_f64().unwrap();
        assert!(score >= outside, "{score} < {outside}");
        assert_eq!(
            features["related"],
            crate::domain::plan_quality::related_tier(Some(score))
        );
        // A proposal that is gone has no features.
        assert!(
            queue
                .proposal_features(ProposalId::new(99))
                .unwrap()
                .is_none()
        );
    }
}
