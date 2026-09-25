//! Proposals (ADR-0041 decisions 7, 8): the goals and tasks a planner
//! submits for plan review, kept as `proposals` rows with each member's
//! `proposal_id`. Submitting moves the member drafts to `submitted`; the
//! plan-review path ([`approve`]) is what makes them `ready`, and a send
//! back returns them to `draft` for the planner.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use super::sqlite::{enum_col, event, goal_event, read_goal, transition_task};
use crate::domain::{
    DomainError, GoalStatus, PlannerOwner, Proposal, ProposalId, ProposalRecord, ProposalStatus,
    Submission, TaskAction, TaskId, TaskStatus, goal, proposal,
};

/// Submit `submission` inside the caller's write transaction: its tasks
/// and the draft tasks of its goals (and, for a resubmission, the drafts
/// the proposal already holds) move from `draft` to `submitted` and join
/// the proposal, recording `task_status_changed` and `task_submitted`
/// (`goal_submitted` for a goal).
pub(super) fn submit(conn: &Connection, submission: Submission, now: &str) -> Result<Proposal> {
    submission.validate()?;
    let target = submission.proposal;
    let existing = target.map(|id| read(conn, id)).transpose()?;
    // Only a proposal plan review sent back is submitted again, or one it
    // holds for a person (a failed job, a concern), which goes again as it
    // is.
    if let Some(existing) = &existing
        && existing.status() != ProposalStatus::Revising
    {
        if existing.status() == ProposalStatus::Submitted && held(conn, existing.id())? {
            ensure!(
                submission.tasks.is_empty() && submission.goals.is_empty(),
                "proposal {} waits for plan review as it is; it takes no new task or goal",
                existing.id()
            );
            let retried = proposal::retry(existing.clone(), now.into())?;
            save(conn, &retried)?;
            conn.execute(
                "UPDATE proposals SET review_hold=NULL WHERE id=?1",
                [retried.id()],
            )?;
            for &task_id in retried.task_ids().iter().take(1) {
                event(
                    conn,
                    task_id,
                    None,
                    "proposal_resubmitted",
                    json!({"proposal_id": retried.id()}),
                )?;
            }
            return read(conn, retried.id());
        }
        return Err(DomainError::ProposalNotInStatus {
            proposal_id: existing.id(),
            status: existing.status(),
            expected: ProposalStatus::Revising,
        }
        .into());
    }
    let mut goals = submission.goals;
    let mut tasks = submission.tasks;
    if let Some(existing) = &existing {
        goals.extend_from_slice(existing.goal_ids());
        // A ready task plan review reopened into this proposal waits in
        // `submitted` (ADR-0041 decision 14) and goes again as it is.
        tasks.extend(ids::<TaskId>(
            conn,
            "SELECT id FROM tasks WHERE proposal_id=?1 AND status IN ('draft','submitted')
             ORDER BY id",
            existing.id().as_i64(),
        )?);
    }
    goals.sort();
    goals.dedup();
    for &goal_id in &goals {
        goal::check_accepts_tasks(&read_goal(conn, goal_id)?)?;
        proposal::check_goal_joins(
            goal_id,
            membership(conn, "goals", goal_id.as_i64())?,
            target,
        )?;
        tasks.extend(ids::<TaskId>(
            conn,
            "SELECT id FROM tasks WHERE goal_id=?1 AND status='draft' ORDER BY id",
            goal_id.as_i64(),
        )?);
    }
    tasks.sort();
    tasks.dedup();
    if tasks.is_empty() {
        return Err(DomainError::EmptyProposal.into());
    }
    for &task_id in &tasks {
        proposal::check_task_joins(
            task_id,
            membership(conn, "tasks", task_id.as_i64())?,
            target,
        )?;
    }
    let submitted = match existing {
        Some(existing) => {
            let mut members = existing.task_ids().to_vec();
            members.extend_from_slice(&tasks);
            proposal::resubmit(existing, submission.owner, members, goals, now.into())?
        }
        None => Proposal::submit(
            ProposalId::new(super::sqlite::next_id(conn, "proposals")?),
            submission.owner,
            tasks.clone(),
            goals,
            now.into(),
        )?,
    };
    save(conn, &submitted)?;
    let id = submitted.id();
    // A submission starts plan review afresh: no hold, no revise pending.
    conn.execute(
        "UPDATE proposals SET review_hold=NULL, revise_reasons=NULL, revise_sent_at=NULL,
             revise_planner_id=NULL, unresponsive_at=NULL WHERE id=?1",
        [id],
    )?;
    for &task_id in &tasks {
        if status(conn, task_id)? == TaskStatus::Submitted {
            continue;
        }
        transition_task(conn, task_id, TaskAction::Submit, now)?;
        conn.execute(
            "UPDATE tasks SET proposal_id=?1 WHERE id=?2",
            params![id, task_id],
        )?;
        event(
            conn,
            task_id,
            None,
            "task_submitted",
            json!({"proposal_id": id}),
        )?;
    }
    for &goal_id in submitted.goal_ids() {
        let joined = conn.execute(
            "UPDATE goals SET proposal_id=?1 WHERE id=?2 AND proposal_id IS NOT ?1",
            params![id, goal_id],
        )?;
        if joined != 0 {
            goal_event(conn, goal_id, "goal_submitted", json!({"proposal_id": id}))?;
        }
    }
    read(conn, id)
}

/// The plan-review path to `ready` (ADR-0041 decisions 8, 11): the
/// proposal is accepted, its submitted tasks become ready and its draft
/// goals open, in the caller's transaction.
pub(super) fn approve(conn: &Connection, id: ProposalId, now: &str) -> Result<Proposal> {
    let accepted = proposal::accept(read(conn, id)?, now.into())?;
    save(conn, &accepted)?;
    for &task_id in accepted.task_ids() {
        if status(conn, task_id)? == TaskStatus::Submitted {
            transition_task(conn, task_id, TaskAction::Approve, now)?;
        }
    }
    for &goal_id in accepted.goal_ids() {
        let draft = read_goal(conn, goal_id)?;
        if draft.status() == GoalStatus::Draft && !draft.is_closed() {
            let opened = goal::ready(draft)?;
            conn.execute(
                "UPDATE goals SET status=?1, updated_at=?2 WHERE id=?3",
                params![opened.status().as_str(), now, goal_id],
            )?;
            goal_event(
                conn,
                goal_id,
                "goal_status_changed",
                json!({"from": GoalStatus::Draft, "to": opened.status()}),
            )?;
        }
    }
    read(conn, id)
}

/// Plan review sent the proposal back to its planner (ADR-0041 decision
/// 11): its submitted tasks return to `draft` until it is submitted again.
pub(super) fn send_back(conn: &Connection, id: ProposalId, now: &str) -> Result<Proposal> {
    let revising = proposal::send_back(read(conn, id)?, now.into())?;
    save(conn, &revising)?;
    for &task_id in revising.task_ids() {
        if status(conn, task_id)? == TaskStatus::Submitted {
            transition_task(conn, task_id, TaskAction::Draft, now)?;
        }
    }
    read(conn, id)
}

/// The active proposals (submitted or revising) in the order plan review
/// takes them, oldest submission first; with `all`, every proposal.
pub(super) fn list(conn: &Connection, all: bool) -> Result<Vec<Proposal>> {
    let ids: Vec<ProposalId> = conn
        .prepare(
            "SELECT id FROM proposals WHERE ?1 OR status IN ('submitted','revising')
             ORDER BY submitted_at, id",
        )?
        .query_map([all], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter().map(|id| read(conn, id)).collect()
}

pub(super) fn read(conn: &Connection, id: ProposalId) -> Result<Proposal> {
    let record = conn
        .query_row("SELECT * FROM proposals WHERE id=?1", [id], record_row)
        .optional()?
        .with_context(|| format!("proposal {id} does not exist"))?;
    let task_ids = ids(
        conn,
        "SELECT id FROM tasks WHERE proposal_id=?1 ORDER BY id",
        id.as_i64(),
    )?;
    let goal_ids = ids(
        conn,
        "SELECT id FROM goals WHERE proposal_id=?1 ORDER BY id",
        id.as_i64(),
    )?;
    Ok(Proposal::restore(ProposalRecord {
        task_ids,
        goal_ids,
        ..record
    })?)
}

fn record_row(row: &Row<'_>) -> rusqlite::Result<ProposalRecord> {
    Ok(ProposalRecord {
        id: row.get("id")?,
        status: enum_col(row, "status")?,
        owner: PlannerOwner {
            origin: enum_col(row, "owner_origin")?,
            workspace_id: row.get("owner_workspace_id")?,
        },
        submitted_at: row.get("submitted_at")?,
        revise_count: row.get("revise_count")?,
        task_ids: Vec::new(),
        goal_ids: Vec::new(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Insert or update the proposal row; members are the tasks' and goals'
/// `proposal_id`, written by the caller.
pub(super) fn save(conn: &Connection, proposal: &Proposal) -> Result<()> {
    conn.execute(
        "INSERT INTO proposals(id, status, owner_origin, owner_workspace_id, submitted_at,
                               revise_count, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(id) DO UPDATE SET status=excluded.status,
             owner_origin=excluded.owner_origin, owner_workspace_id=excluded.owner_workspace_id,
             submitted_at=excluded.submitted_at, revise_count=excluded.revise_count,
             updated_at=excluded.updated_at",
        params![
            proposal.id(),
            proposal.status().as_str(),
            proposal.owner().origin.as_str(),
            proposal.owner().workspace_id,
            proposal.submitted_at(),
            proposal.revise_count(),
            proposal.created_at(),
            proposal.updated_at()
        ],
    )?;
    Ok(())
}

/// The proposal a task or goal (`table`) belongs to now, with its status.
fn membership(
    conn: &Connection,
    table: &str,
    id: i64,
) -> Result<Option<(ProposalId, ProposalStatus)>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT p.id, p.status FROM {table} m JOIN proposals p ON p.id = m.proposal_id
                 WHERE m.id=?1"
            ),
            [id],
            |row| Ok((row.get(0)?, enum_col(row, "status")?)),
        )
        .optional()?)
}

/// Whether plan review holds the proposal for a person.
fn held(conn: &Connection, id: ProposalId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT review_hold IS NOT NULL FROM proposals WHERE id=?1",
        [id],
        |r| r.get(0),
    )?)
}

fn status(conn: &Connection, task_id: TaskId) -> Result<TaskStatus> {
    Ok(
        conn.query_row("SELECT status FROM tasks WHERE id=?1", [task_id], |row| {
            enum_col(row, "status")
        })?,
    )
}

fn ids<T: rusqlite::types::FromSql>(conn: &Connection, query: &str, key: i64) -> Result<Vec<T>> {
    Ok(conn
        .prepare(query)?
        .query_map([key], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
