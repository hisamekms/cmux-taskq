//! Findings (ADR-0044 decision 18) as `findings` rows. Recording one, and
//! every change to it, also writes a run event (`finding_recorded`,
//! `finding_updated`, `finding_status_changed`) on the finding's target, or
//! on nothing for a finding on the queue.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, enum_col, event_row, json_col, read_goal, read_task};
use crate::domain::{
    AskId, EventId, Finding, FindingId, FindingOutcome, FindingQuery, FindingStatus, FindingTarget,
    FindingView, GoalId, NewFinding, RunId, TaskId, finding,
};

impl SqliteQueue {
    /// Record `new`: the unsettled finding of the same kind, target and
    /// subject (or else the latest settled one) takes it as an occurrence
    /// ([`finding::merge`]); without one a new finding is created. A
    /// record that brings nothing new writes nothing (`changed` is empty).
    pub fn record_finding(&mut self, new: NewFinding) -> Result<FindingOutcome> {
        new.validate()?;
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (task_id, run_id, goal_id) = resolve_target(&tx, &new.target)?;
        let evidence = new.distinct_evidence();
        for id in &evidence {
            ensure!(
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM run_events WHERE id=?1)",
                    [id],
                    |r| r.get::<_, bool>(0)
                )?,
                "event {id} does not exist"
            );
        }
        let existing = tx
            .query_row(
                "SELECT * FROM findings WHERE kind=?1 AND target=?2 AND ifnull(task_id,0)=ifnull(?3,0)
                 AND ifnull(run_id,'')=ifnull(?4,'') AND ifnull(goal_id,0)=ifnull(?5,0) AND subject=?6
                 ORDER BY status IN ('open','proposed') DESC, id DESC LIMIT 1",
                params![
                    new.kind,
                    new.target.name(),
                    task_id,
                    run_id,
                    goal_id,
                    new.subject
                ],
                finding_row,
            )
            .optional()?;
        let outcome = match existing {
            None => {
                tx.execute(
                    "INSERT INTO findings(kind,target,task_id,run_id,goal_id,subject,summary,detail,
                       impact,first_seen_at,last_seen_at,occurrences,evidence,status,
                       propose_reason,propose_requested_at,recorded_by,updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,1,?11,'open',?12,?13,?14,?10)",
                    params![
                        new.kind,
                        new.target.name(),
                        task_id,
                        run_id,
                        goal_id,
                        new.subject,
                        new.summary,
                        new.detail.as_deref().unwrap_or_default(),
                        new.impact.unwrap_or(crate::domain::Impact::Normal).as_str(),
                        now,
                        serde_json::to_string(&evidence)?,
                        new.propose,
                        new.propose.as_ref().map(|_| now),
                        new.by,
                    ],
                )?;
                let created = read_finding(&tx, FindingId::new(tx.last_insert_rowid()))?;
                finding_event(
                    &tx,
                    &created,
                    "finding_recorded",
                    json!({
                        "finding_id": created.id,
                        "kind": created.kind,
                        "target": created.target,
                        "subject": created.subject,
                        "impact": created.impact,
                        "evidence": created.evidence,
                        "propose": created.propose_reason.is_some(),
                        "by": new.by,
                    }),
                )?;
                FindingOutcome {
                    finding: created,
                    created: true,
                    changed: Vec::new(),
                }
            }
            Some(existing) => match finding::merge(&existing, &new, now) {
                None => FindingOutcome {
                    finding: existing,
                    created: false,
                    changed: Vec::new(),
                },
                Some(update) => {
                    let f = &update.finding;
                    tx.execute(
                        "UPDATE findings SET summary=?2, detail=?3, impact=?4, last_seen_at=?5,
                           occurrences=?6, evidence=?7, status=?8, status_reason=?9,
                           propose_reason=?10, propose_requested_at=?11, updated_at=?12
                         WHERE id=?1",
                        params![
                            f.id,
                            f.summary,
                            f.detail,
                            f.impact.as_str(),
                            f.last_seen_at,
                            f.occurrences,
                            serde_json::to_string(&f.evidence)?,
                            f.status.as_str(),
                            f.status_reason,
                            f.propose_reason,
                            f.propose_requested_at,
                            f.updated_at,
                        ],
                    )?;
                    if update.reopened {
                        finding_event(
                            &tx,
                            f,
                            "finding_status_changed",
                            json!({
                                "finding_id": f.id,
                                "from": existing.status,
                                "to": f.status,
                                "reason": "occurred again",
                                "by": new.by,
                            }),
                        )?;
                    }
                    finding_event(
                        &tx,
                        f,
                        "finding_updated",
                        json!({
                            "finding_id": f.id,
                            "changed": update.changed,
                            "added_evidence": update.added_evidence,
                            "occurrences": f.occurrences,
                            "by": new.by,
                        }),
                    )?;
                    FindingOutcome {
                        finding: read_finding(&tx, f.id)?,
                        created: false,
                        changed: update.changed,
                    }
                }
            },
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Resolve or dismiss a finding with the reason
    /// ([`finding::check_transition`]), recording `finding_status_changed`.
    pub fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
    ) -> Result<Finding> {
        ensure!(!reason.trim().is_empty(), "reason must not be blank");
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_finding(&tx, id)?;
        finding::check_transition(&current, to)?;
        tx.execute(
            "UPDATE findings SET status=?2, status_reason=?3, updated_at=?4 WHERE id=?1",
            params![id, to.as_str(), reason, now],
        )?;
        let changed = read_finding(&tx, id)?;
        finding_event(
            &tx,
            &changed,
            "finding_status_changed",
            json!({
                "finding_id": id,
                "from": current.status,
                "to": to,
                "reason": reason,
                "by": by,
            }),
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// The findings `query` lists, larger impact first
    /// ([`finding::by_impact`]). A pure read.
    pub fn findings(&self, query: &FindingQuery) -> Result<Vec<FindingView>> {
        let target = query
            .target
            .as_ref()
            .map(|target| resolve_target(&self.conn, target).map(|ids| (target.name(), ids)))
            .transpose()?;
        if let Some(id) = query.id {
            read_finding(&self.conn, id)?;
        }
        let mut findings: Vec<Finding> = self
            .conn
            .prepare("SELECT * FROM findings ORDER BY id")?
            .query_map([], finding_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|f| query.admits(f))
            .filter(|f| {
                target.as_ref().is_none_or(|(name, (task, run, goal))| {
                    f.target == *name
                        && f.task_id == *task
                        && f.run_id == *run
                        && f.goal_id == *goal
                })
            })
            .collect();
        findings.sort_by(finding::by_impact);
        findings
            .into_iter()
            .map(|finding| {
                let proposal_status = finding
                    .proposal_id
                    .map(|id| {
                        self.conn
                            .query_row("SELECT status FROM proposals WHERE id=?1", [id], |r| {
                                enum_col(r, "status")
                            })
                    })
                    .transpose()?;
                let open_asks = self
                    .conn
                    .prepare(
                        "SELECT id FROM asks WHERE finding_id=?1
                         AND answered_at IS NULL AND closed_at IS NULL ORDER BY id",
                    )?
                    .query_map([finding.id], |r| r.get::<_, AskId>(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let evidence_events = query
                    .full
                    .then(|| evidence_events(&self.conn, &finding.evidence))
                    .transpose()?;
                Ok(FindingView {
                    finding,
                    proposal_status,
                    open_asks,
                    evidence_events,
                })
            })
            .collect()
    }
}

/// The (task, run, goal) columns of `target`, checking that it exists; a
/// run brings its task.
fn resolve_target(
    conn: &Connection,
    target: &FindingTarget,
) -> Result<(Option<TaskId>, Option<RunId>, Option<GoalId>)> {
    Ok(match target {
        FindingTarget::Queue => (None, None, None),
        FindingTarget::Goal(goal_id) => {
            read_goal(conn, *goal_id)?;
            (None, None, Some(*goal_id))
        }
        FindingTarget::Task(task_id) => {
            read_task(conn, *task_id)?;
            (Some(*task_id), None, None)
        }
        FindingTarget::Run(run_id) => {
            let task_id: TaskId = conn
                .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("run {run_id} does not exist"))?;
            (Some(task_id), Some(run_id.clone()), None)
        }
    })
}

/// A finding's event, on its target (a run's on the run and its task), or
/// on nothing for the queue.
fn finding_event(
    conn: &Connection,
    finding: &Finding,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    crate::domain::check_event_target(kind, finding.task_id, finding.goal_id)?;
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,goal_id,kind,payload) VALUES (?1,?2,?3,?4,?5)",
        params![
            finding.task_id,
            finding.run_id,
            finding.goal_id,
            kind,
            serde_json::to_string(&payload)?
        ],
    )?;
    Ok(())
}

/// The evidence events that still exist, in the finding's order.
fn evidence_events(
    conn: &Connection,
    evidence: &[EventId],
) -> Result<Vec<crate::domain::RunEvent>> {
    let mut events = Vec::new();
    for id in evidence {
        if let Some(event) = conn
            .query_row("SELECT * FROM run_events WHERE id=?1", [id], event_row)
            .optional()?
        {
            events.push(event);
        }
    }
    Ok(events)
}

pub(super) fn read_finding(conn: &Connection, id: FindingId) -> Result<Finding> {
    conn.query_row("SELECT * FROM findings WHERE id=?1", [id], finding_row)
        .optional()?
        .with_context(|| format!("finding {id} does not exist"))
}

fn finding_row(row: &Row<'_>) -> rusqlite::Result<Finding> {
    Ok(Finding {
        id: row.get("id")?,
        kind: row.get("kind")?,
        target: row.get("target")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        goal_id: row.get("goal_id")?,
        subject: row.get("subject")?,
        summary: row.get("summary")?,
        detail: row.get("detail")?,
        impact: enum_col(row, "impact")?,
        first_seen_at: row.get("first_seen_at")?,
        last_seen_at: row.get("last_seen_at")?,
        occurrences: row.get("occurrences")?,
        evidence: json_col(row, "evidence")?,
        status: enum_col(row, "status")?,
        status_reason: row.get("status_reason")?,
        proposal_id: row.get("proposal_id")?,
        propose_reason: row.get("propose_reason")?,
        propose_requested_at: row.get("propose_requested_at")?,
        recorded_by: row.get("recorded_by")?,
        updated_at: row.get("updated_at")?,
    })
}
