//! Durable supervisor ownership and one-shot wrapper registration.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;

use super::sqlite::{SqliteQueue, event, run_row};
use crate::domain::{RunProcess, SupervisorLease, TaskRun};

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
    pub fn acquire_supervisor(&mut self, token: &str, common_dir: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing == common_dir,
                "queue is bound to another Git repository: {existing}"
            );
        }
        let leased: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM supervisor_leases)", [], |r| {
                r.get(0)
            })?;
        ensure!(
            !leased,
            "supervisor lease already exists; inspect status (stale leases are never taken over automatically)"
        );
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_runs WHERE status IN ('claimed','starting','running','validating'))", [], |r| r.get(0)
        )?;
        ensure!(
            !active,
            "an unfinished run exists; inspect its state before starting another run"
        );
        tx.execute(
            "INSERT OR IGNORE INTO queue_repository VALUES (1,?1)",
            [common_dir],
        )?;
        tx.execute(
            "INSERT INTO supervisor_leases(singleton,token,pid) VALUES (1,?1,?2)",
            params![token, std::process::id()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn heartbeat_supervisor(&self, token: &str) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisor_leases SET heartbeat_at=unixepoch() WHERE token=?1",
                [token]
            )? == 1,
            "supervisor lease was lost"
        );
        Ok(())
    }

    pub fn release_supervisor(&self, token: &str) -> Result<()> {
        ensure!(
            self.conn
                .execute("DELETE FROM supervisor_leases WHERE token=?1", [token])?
                == 1,
            "supervisor lease was lost"
        );
        Ok(())
    }

    pub fn supervisor_lease(&self) -> Result<Option<SupervisorLease>> {
        Ok(self
            .conn
            .query_row("SELECT pid,heartbeat_at FROM supervisor_leases", [], |r| {
                Ok(SupervisorLease {
                    pid: r.get(0)?,
                    heartbeat_at: r.get(1)?,
                })
            })
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
        assert_lease(&tx, token)?;
        ensure!(tx.execute(
            "UPDATE task_runs SET status='starting',supervisor_token=?2,repo_path=?3,run_dir=?4,
             branch=?5,worktree_path=?6,receipt_path=?7,log_path=?8
             WHERE id=?1 AND status='claimed' AND supervisor_token IS NULL",
            params![id,token,plan.repo_path,plan.run_dir,plan.branch,plan.worktree_path,plan.receipt_path,plan.log_path]
        )? == 1, "run cannot be provisioned twice");
        run_event(&tx, id, "run_planned", serde_json::to_value(plan)?)?;
        tx.commit()?;
        Ok(())
    }

    pub fn workspace_created(&mut self, id: &str, token: &str, workspace: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, token)?;
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
        assert_lease(&tx, token)?;
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
        assert_lease(&tx, token)?;
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
        assert_lease(&tx, token)?;
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
}

impl SqliteQueue {
    /// Record a confirmed cmux close. Only an accepted run whose workspace is
    /// still recorded as open qualifies; the worktree and branch stay for integration.
    pub fn workspace_closed(&mut self, id: &str, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, token)?;
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
        assert_lease(&tx, token)?;
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

fn assert_lease(conn: &Connection, token: &str) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM supervisor_leases WHERE token=?1 AND heartbeat_at >= unixepoch()-?2)",
        params![token,HEARTBEAT_TIMEOUT_SECS], |r| r.get(0))?;
    ensure!(valid, "supervisor lease is missing or stale");
    Ok(())
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
