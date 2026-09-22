//! Durable per-run supervisor ownership and one-shot wrapper registration.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;

use super::sqlite::{SqliteQueue, claim_task, event, read_task, run_row};
use crate::domain::{ClaimOutcome, RunLease, RunProcess, Task, TaskRun, validate_base_commit};

pub const HEARTBEAT_TIMEOUT_SECS: i64 = 30;

/// Outcome of supervisor-side receipt validation. `result_commit` is kept on
/// rejection too when the commit itself was verified, so inspection can start there.
#[derive(Debug, Serialize)]
pub struct Validation {
    pub accepted: bool,
    pub result_commit: Option<String>,
    pub reason: Option<String>,
    pub receipt: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct RunPlan {
    pub repo_path: String,
    pub run_dir: String,
    pub branch: String,
    pub worktree_path: String,
    pub receipt_path: String,
    pub log_path: String,
}

impl SqliteQueue {
    /// Reserve the next dependency-ready task for this supervisor: the run,
    /// its `supervisor_token` and its lease row are created in one transaction,
    /// so a claimed run never exists without an owner. Concurrent supervisors
    /// on the same queue take different tasks.
    pub fn claim_for_supervisor(&mut self, base_commit: &str, token: &str) -> Result<ClaimOutcome> {
        validate_base_commit(base_commit)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = claim_task(&tx, base_commit)?;
        if let ClaimOutcome::Claimed { run } = &outcome {
            tx.execute(
                "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
                params![run.id, token],
            )?;
            tx.execute(
                "INSERT INTO run_leases(run_id,token,pid) VALUES (?1,?2,?3)",
                params![run.id, token, std::process::id()],
            )?;
            run_event(
                &tx,
                &run.id,
                "lease_acquired",
                json!({"pid": std::process::id()}),
            )?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Refresh every lease this supervisor holds. Zero rows is not an error:
    /// an idle supervisor owns nothing.
    pub fn heartbeat_leases(&self, token: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch() WHERE token=?1",
            [token],
        )?)
    }

    /// Give up ownership of a run that came to rest (`awaiting_integration`
    /// or `failed`). The run's `supervisor_token` stays as a record.
    pub fn release_lease(&mut self, id: &str, token: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
                params![id, token]
            )? == 1,
            "run lease was lost"
        );
        run_event(&tx, id, "lease_released", json!({"reason": "finished"}))?;
        tx.commit()?;
        Ok(())
    }

    /// Record a runtime error and disown the run without changing its status
    /// or touching its processes and resources. Dropping the lease lets
    /// `recover` judge the run by its registered processes alone while this
    /// supervisor keeps serving other runs; a wrapper that has not registered
    /// yet can no longer do so.
    pub fn abandon_run(&mut self, id: &str, token: &str, message: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET last_error=?2 WHERE id=?1",
                params![id, message]
            )? == 1,
            "run does not exist"
        );
        let released = tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        run_event(
            &tx,
            id,
            "runtime_error",
            json!({"message": message, "lease_released": released == 1}),
        )?;
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        tx.commit()?;
        Ok(result)
    }

    /// Bind the queue to a repository before any run exists, as `init` does for a
    /// queue resolved from the working directory. A queue already bound to
    /// another repository is refused; rebinding is never implicit.
    pub fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO queue_repository VALUES (1,?1)",
            [common_dir],
        )?;
        let bound: String = tx.query_row(
            "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            bound == common_dir,
            "queue is bound to another Git repository: {bound}"
        );
        tx.commit()?;
        Ok(())
    }

    /// Refuse a queue that belongs to another repository. An unbound queue
    /// (created with `--db` and never supervised) passes.
    pub fn assert_repository(&self, common_dir: &str) -> Result<()> {
        if let Some(bound) = self.repository_binding()? {
            ensure!(
                bound == common_dir,
                "queue is bound to another Git repository: {bound} (this repository is {common_dir})"
            );
        }
        Ok(())
    }

    /// Git common directory the queue is bound to, recorded by `init` for a
    /// repository queue or by the first `supervise` otherwise.
    pub fn repository_binding(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every lease in the queue, oldest run first.
    pub fn run_leases(&self) -> Result<Vec<RunLease>> {
        Ok(self
            .conn
            .prepare("SELECT l.run_id,l.pid,l.heartbeat_at FROM run_leases l JOIN task_runs r ON r.id=l.run_id ORDER BY r.rowid")?
            .query_map([], lease_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn run_lease(&self, id: &str) -> Result<Option<RunLease>> {
        Ok(self
            .conn
            .query_row(
                "SELECT run_id,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?)
    }

    pub fn run(&self, id: &str) -> Result<TaskRun> {
        self.conn
            .query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)
            .optional()?
            .with_context(|| format!("run {id} does not exist"))
    }

    pub fn plan_run(&mut self, id: &str, token: &str, plan: &RunPlan) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET status='starting',repo_path=?3,run_dir=?4,
             branch=?5,worktree_path=?6,receipt_path=?7,log_path=?8
             WHERE id=?1 AND status='claimed' AND supervisor_token=?2",
                params![
                    id,
                    token,
                    plan.repo_path,
                    plan.run_dir,
                    plan.branch,
                    plan.worktree_path,
                    plan.receipt_path,
                    plan.log_path
                ]
            )? == 1,
            "run cannot be provisioned twice"
        );
        run_event(&tx, id, "run_planned", serde_json::to_value(plan)?)?;
        tx.commit()?;
        Ok(())
    }

    pub fn workspace_created(&mut self, id: &str, token: &str, workspace: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET workspace_id=?3 WHERE id=?1 AND supervisor_token=?2
             AND status='starting' AND workspace_id IS NULL",
                params![id, token, workspace]
            )? == 1,
            "workspace cannot be attached to this run"
        );
        run_event(
            &tx,
            id,
            "workspace_created",
            json!({"workspace_id": workspace}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_runtime_event(
        &self,
        id: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        run_event(&self.conn, id, kind, payload)
    }

    pub fn record_runtime_error(&mut self, id: &str, message: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET last_error=?2 WHERE id=?1",
                params![id, message]
            )? == 1,
            "run does not exist"
        );
        run_event(&tx, id, "runtime_error", json!({"message": message}))?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_wrapper(&mut self, id: &str, token: &str, pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        let allowed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_runs WHERE id=?1 AND supervisor_token=?2
             AND status='starting' AND workspace_id IS NOT NULL)",
            params![id, token],
            |r| r.get(0),
        )?;
        ensure!(allowed, "run is not ready for its wrapper");
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'wrapper',?2)",
            params![id, pid],
        )
        .context("wrapper is already registered; a run may only launch once")?;
        run_event(&tx, id, "wrapper_started", json!({"pid": pid}))?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_agent(&mut self, id: &str, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, wrapper_pid)?;
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'agent',?2)",
            params![id, agent_pid],
        )?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET status='running' WHERE id=?1 AND status='starting'",
                [id]
            )? == 1,
            "run is not starting"
        );
        run_event(
            &tx,
            id,
            "agent_started",
            json!({"pid": agent_pid, "session_id": id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn heartbeat_wrapper(&self, id: &str, pid: u32) -> Result<()> {
        // Registration is immutable and a run ID is never reused.
        assert_wrapper(&self.conn, id, pid)?;
        self.conn.execute("UPDATE run_processes SET heartbeat_at=unixepoch() WHERE run_id=?1 AND exited_at IS NULL", [id])?;
        Ok(())
    }

    pub fn wrapper_exited(&mut self, id: &str, pid: u32, exit_code: i32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, pid)?;
        tx.execute("UPDATE run_processes SET exited_at=unixepoch(),exit_code=?2,heartbeat_at=unixepoch() WHERE run_id=?1 AND exited_at IS NULL",
            params![id,exit_code])?;
        run_event(&tx, id, "session_exited", json!({"exit_code": exit_code}))?;
        tx.commit()?;
        Ok(())
    }

    /// Runs that hold the execution slot, oldest first.
    pub fn active_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs WHERE status IN ('claimed','starting','running','validating') ORDER BY rowid")?
            .query_map([], run_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Operator recovery of an orphaned run. The caller has checked that the
    /// registered processes are dead; `checked_processes` guards against a
    /// registration that happened in between, and a fresh lease is refused here
    /// again. Only this run's lease is deleted; other runs, their leases,
    /// resources and the task's `in_progress` status are left untouched.
    pub fn recover_run(
        &mut self,
        id: &str,
        checked_processes: usize,
        mut report: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fresh: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND heartbeat_at >= unixepoch()-?2)",
            params![id, HEARTBEAT_TIMEOUT_SECS],
            |r| r.get(0),
        )?;
        ensure!(!fresh, "run lease heartbeat is fresh");
        let registered: i64 = tx.query_row(
            "SELECT count(*) FROM run_processes WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(registered).ok() == Some(checked_processes),
            "run processes changed during recovery; inspect doctor again"
        );
        let previous: String = tx
            .query_row("SELECT status FROM task_runs WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .with_context(|| format!("run {id} does not exist"))?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET status='interrupted' WHERE id=?1
                 AND status IN ('claimed','starting','running','validating')",
                [id]
            )? == 1,
            "run {id} is {previous}; only unfinished runs can be recovered"
        );
        let leases_deleted = tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        report["previous_status"] = json!(previous);
        report["lease_deleted"] = json!(leases_deleted == 1);
        run_event(&tx, id, "run_recovered", report)?;
        // The task stays in_progress; a retry is an explicit `ready` and a new run.
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn processes(&self, id: &str) -> Result<Vec<RunProcess>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_processes WHERE run_id=?1 ORDER BY role")?
            .query_map([id], process_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn finish_supervision(&mut self, id: &str, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        let code: i32 = tx.query_row(
            "SELECT exit_code FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL",
            [id], |r| r.get(0)
        ).context("wrapper has not reported session exit")?;
        let status = if code == 0 { "validating" } else { "failed" };
        ensure!(tx.execute("UPDATE task_runs SET status=?3 WHERE id=?1 AND supervisor_token=?2 AND status IN ('starting','running')",
            params![id,token,status])? == 1, "run is not owned by this supervisor");
        run_event(
            &tx,
            id,
            "supervision_finished",
            json!({"status": status, "exit_code": code}),
        )?;
        // Completion and dependency release belong to the next validation stage.
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn finish_validation(
        &mut self,
        id: &str,
        token: &str,
        validation: &Validation,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        let status = if validation.accepted {
            "awaiting_integration"
        } else {
            "failed"
        };
        ensure!(
            tx.execute(
                "UPDATE task_runs SET status=?3,result_commit=?4,last_error=COALESCE(?5,last_error)
                 WHERE id=?1 AND supervisor_token=?2 AND status='validating'",
                params![
                    id,
                    token,
                    status,
                    validation.result_commit,
                    validation.reason
                ]
            )? == 1,
            "run is not validating under this supervisor"
        );
        let mut payload = serde_json::to_value(validation)?;
        payload["status"] = json!(status);
        run_event(&tx, id, "validation_finished", payload)?;
        // Task completion still waits for integration into main.
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        tx.commit()?;
        Ok(result)
    }

    /// Complete the task once its awaiting run was confirmed in `main`. No lease
    /// is involved: the run stopped executing when validation finished. The status
    /// predicates make a repeated or concurrent confirmation fail without effect.
    pub fn finish_integration(
        &mut self,
        id: &str,
        main: &str,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET status='integrated' WHERE id=?1 AND status='awaiting_integration'",
                [id]
            )? == 1,
            "run {id} is no longer awaiting integration"
        );
        let run = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        ensure!(
            tx.execute(
                "UPDATE tasks SET status='completed', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id=?1 AND status='in_progress'",
                [run.task_id]
            )? == 1,
            "task {} is not in progress",
            run.task_id
        );
        run_event(
            &tx,
            id,
            "run_integrated",
            json!({"result_commit": run.result_commit, "main": main, "git_common_dir": common_dir}),
        )?;
        event(
            &tx,
            run.task_id,
            Some(id),
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
        )?;
        let task = read_task(&tx, run.task_id)?;
        tx.commit()?;
        Ok((task, run))
    }
}

impl SqliteQueue {
    /// Record a confirmed cmux close. Only an accepted run whose workspace is
    /// still recorded as open qualifies; the worktree and branch stay for integration.
    pub fn workspace_closed(&mut self, id: &str, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET workspace_closed_at=unixepoch() WHERE id=?1 AND supervisor_token=?2
                 AND status='awaiting_integration' AND workspace_id IS NOT NULL AND workspace_closed_at IS NULL",
                params![id, token]
            )? == 1,
            "run is not awaiting integration with an open workspace under this supervisor"
        );
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        run_event(
            &tx,
            id,
            "workspace_closed",
            json!({"workspace_id": result.workspace_id, "closed_at": result.workspace_closed_at}),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// A failed close leaves `workspace_closed_at` null so the workspace is never
    /// treated as cleaned; the run status does not change.
    pub fn cleanup_failed(&mut self, id: &str, token: &str, message: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, token)?;
        ensure!(
            tx.execute(
                "UPDATE task_runs SET last_error=?3 WHERE id=?1 AND supervisor_token=?2
                 AND status='awaiting_integration' AND workspace_closed_at IS NULL",
                params![id, token, message]
            )? == 1,
            "run is not awaiting integration with an open workspace under this supervisor"
        );
        let result = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [id], run_row)?;
        run_event(
            &tx,
            id,
            "cleanup_failed",
            json!({"workspace_id": result.workspace_id, "message": message}),
        )?;
        tx.commit()?;
        Ok(result)
    }
}

fn assert_lease(conn: &Connection, id: &str, token: &str) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND token=?2 AND heartbeat_at >= unixepoch()-?3)",
        params![id,token,HEARTBEAT_TIMEOUT_SECS], |r| r.get(0))?;
    ensure!(valid, "run lease is missing or stale");
    Ok(())
}

fn lease_row(r: &Row<'_>) -> rusqlite::Result<RunLease> {
    Ok(RunLease {
        run_id: r.get(0)?,
        pid: r.get(1)?,
        heartbeat_at: r.get(2)?,
    })
}

fn assert_wrapper(conn: &Connection, id: &str, pid: u32) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND pid=?2 AND exited_at IS NULL)",
        params![id,pid], |r| r.get(0))?;
    ensure!(valid, "wrapper is not the live owner of this run");
    Ok(())
}

fn run_event(conn: &Connection, id: &str, kind: &str, payload: serde_json::Value) -> Result<()> {
    let task_id: i64 = conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [id], |r| {
        r.get(0)
    })?;
    event(conn, task_id, Some(id), kind, payload)
}

fn process_row(r: &Row<'_>) -> rusqlite::Result<RunProcess> {
    Ok(RunProcess {
        run_id: r.get("run_id")?,
        role: r.get("role")?,
        pid: r.get("pid")?,
        heartbeat_at: r.get("heartbeat_at")?,
        exited_at: r.get("exited_at")?,
        exit_code: r.get("exit_code")?,
    })
}

pub(super) fn processes_for_task(conn: &Connection, task_id: i64) -> Result<Vec<RunProcess>> {
    Ok(conn.prepare("SELECT p.* FROM run_processes p JOIN task_runs r ON r.id=p.run_id WHERE r.task_id=?1 ORDER BY r.rowid,p.role")?
        .query_map([task_id], process_row)?.collect::<rusqlite::Result<_>>()?)
}
