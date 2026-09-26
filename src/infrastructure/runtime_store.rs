//! Durable per-run supervisor ownership and one-shot wrapper registration.
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::{
    adapters::process_alive,
    sqlite::{
        SqliteQueue, claim_task, enum_col, event, event_row, json_col, read_task, run_row,
        stored_run_row,
    },
};
use crate::application::{AskStore, Generators, RunStore, timestamp, unix_seconds};
use crate::domain::{
    AskId, ClaimOutcome, CommitSha, DomainError, EventFilter, EventId, GoalId, PlannerId,
    PlannerOrigin, PlannerSession, ProposalId, Reason, ReasonCode, RunEvent, RunId, RunLease,
    RunPaths, RunProcess, RunStatus, SessionRole, SupervisorMode, SupervisorRegistration, Task,
    TaskAction, TaskId, TaskRun,
    resume::{self, ResumeCount},
    run,
};

pub use crate::application::{
    EndedRunWorkspace, EndedRunWorktree, Exhaustion, Landing, LeasedRun, ResumeCandidate,
    TRIAGE_ASKER, TriageAction, Validation,
};
pub use crate::domain::{HEARTBEAT_TIMEOUT_SECS, RunPlan};

/// Whether a lease no longer has a working process behind it: its pid is
/// dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`. The rule
/// `status` / `doctor` report as `stale` and the one adoption re-checks.
pub fn lease_is_stale(lease: &RunLease, now: i64) -> bool {
    !process_alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}

impl SqliteQueue {
    /// Reserve the next dependency-ready task for this supervisor: the run,
    /// its `supervisor_token` and its lease row are created in one transaction,
    /// so a claimed run never exists without an owner. Concurrent supervisors
    /// on the same queue take different tasks.
    pub fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
    ) -> Result<ClaimOutcome> {
        self.claim_for_supervisor_in_order(base_commit, token, &[])
    }

    /// [`Self::claim_for_supervisor`], taking the first task of `order` that
    /// is still claimable (the lowest-ID candidate when none is).
    pub fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
        order: &[TaskId],
    ) -> Result<ClaimOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The claim, its run and its first heartbeat share one time.
        let at = self.generators.clock.system_time();
        let outcome = claim_task(
            &tx,
            &self.runs_dir,
            self.generators.ids.as_ref(),
            at,
            base_commit,
            order,
        )?;
        if let ClaimOutcome::Claimed { run } = &outcome {
            tx.execute(
                "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
                params![run.id(), token],
            )?;
            tx.execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
                params![run.id(), token, std::process::id(), unix_seconds(at)],
            )?;
            run_event(
                &tx,
                run.id(),
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
            "UPDATE run_leases SET heartbeat_at=?2 WHERE token=?1",
            params![token, self.generators.clock.now()],
        )?)
    }

    /// One heartbeat of a process identified by `token`: its registration
    /// (if it is a resident supervisor) and every run lease it holds, in one
    /// transaction so `status` never sees them disagree. Returns the number
    /// of leases refreshed.
    pub fn heartbeat(&mut self, token: &str) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        tx.execute(
            "UPDATE supervisors SET heartbeat_at=?2 WHERE token=?1",
            params![token, now],
        )?;
        let leases = tx.execute(
            "UPDATE run_leases SET heartbeat_at=?2 WHERE token=?1",
            params![token, now],
        )?;
        tx.commit()?;
        Ok(leases)
    }

    /// Register a resident `supervise` process before it claims anything,
    /// so `status` and `doctor` can list it while it holds no run.
    /// `binary_version` is the running binary's own build identifier
    /// (`crate::VERSION`), which only this process knows; `up` reads it back
    /// to decide whether a live supervisor is of its own build (ADR-0045).
    pub fn register_supervisor(
        &mut self,
        token: &str,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        ensure!(parallel >= 1, "parallel must be at least 1");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        tx.execute(
            "INSERT INTO supervisors(token,pid,parallel,binary_version,started_at,heartbeat_at)
             VALUES (?1,?2,?3,?4,?5,?5)",
            params![token, pid, parallel, binary_version, now],
        )
        .context("supervisor token is already registered")?;
        let result = tx.query_row(
            "SELECT * FROM supervisors WHERE token=?1",
            [token],
            supervisor_row,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Record how `up` started this supervisor, once its process has
    /// registered itself. Only `up` writes it, and only for a supervisor it
    /// started; the row's own process never does, so a supervisor started
    /// by hand keeps `mode` unset. The workspace belongs to `in_cmux` mode.
    pub fn set_supervisor_mode(
        &self,
        token: &str,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET mode=?2, workspace_id=?3 WHERE token=?1",
                params![token, mode.as_str(), workspace_id],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Remove the registration on a graceful exit. Leases are untouched; a
    /// missing row (already removed, or never written) is not an error, so a
    /// crashed supervisor's row is only ever removed by `up`, `down --force`
    /// or a person.
    pub fn deregister_supervisor(&self, token: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM supervisors WHERE token=?1", [token])?
            == 1)
    }

    /// Mark the registration of `token` as one that takes a handoff
    /// (ADR-0045 decision 10): its process execs another binary when asked.
    pub fn accept_handoff(&self, token: &str) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET handoff_accepted=1 WHERE token=?1",
                [token],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Ask the supervisor `token` to exec `binary` at its next pause between
    /// short steps. `false` when it is not registered or does not take a
    /// handoff.
    pub fn request_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=?2, handoff_requested_at=?3
             WHERE token=?1 AND handoff_accepted=1",
            params![token, binary, self.generators.clock.now()],
        )? == 1)
    }

    /// Withdraw the request that the supervisor `token` exec `binary`, if
    /// it has not taken it yet; `false` when there was none.
    pub fn cancel_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=NULL, handoff_requested_at=NULL
             WHERE token=?1 AND handoff_binary=?2",
            params![token, binary],
        )? == 1)
    }

    /// The binary the supervisor `token` was asked to exec, if any.
    pub fn handoff_request(&self, token: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT handoff_binary FROM supervisors WHERE token=?1",
                [token],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Take the registration of `token` back after exec'ing another binary
    /// (or after an exec that failed): the same process (`pid`) now runs
    /// `binary_version`. Clears the request and refreshes the heartbeat, and
    /// leaves `mode`, `workspace_id`, `started_at` and every lease alone.
    pub fn resume_registration(
        &mut self,
        token: &str,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE supervisors SET binary_version=?3, heartbeat_at=?4, handoff_accepted=1,
                        handoff_binary=NULL, handoff_requested_at=NULL
                 WHERE token=?1 AND pid=?2",
                params![token, pid, binary_version, self.generators.clock.now()],
            )? == 1,
            "supervisor {token} (pid {pid}) is no longer registered; nothing to hand off to"
        );
        let result = tx.query_row(
            "SELECT * FROM supervisors WHERE token=?1",
            [token],
            supervisor_row,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// The runs whose lease carries `token`, oldest first: what a supervisor
    /// that exec'd another binary under the same token picks up again.
    pub fn runs_leased_by(&self, token: &str) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r JOIN run_leases l ON l.run_id=r.id
                 WHERE l.token=?1 ORDER BY r.rowid",
            )?
            .query_map([token], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Turn the automatic update of the supervisor `token` on or off
    /// (ADR-0045 decision 17).
    pub fn set_auto_update(&self, token: &str, enabled: bool) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET auto_update=?2 WHERE token=?1",
                params![token, i64::from(enabled)],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Append one step of the automatic update to `binary_updates`.
    pub fn record_binary_update(
        &self,
        kind: &str,
        commit: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO binary_updates(kind,commit_sha,payload,created_at) VALUES (?1,?2,?3,?4)",
            params![
                kind,
                commit,
                serde_json::to_string(&payload)?,
                self.generators.clock.now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The latest `limit` steps of the automatic update, newest first.
    pub fn binary_updates(&self, limit: usize) -> Result<Vec<crate::domain::BinaryUpdate>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM binary_updates ORDER BY id DESC LIMIT ?1")?
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                Ok(crate::domain::BinaryUpdate {
                    id: r.get("id")?,
                    kind: r.get("kind")?,
                    commit: r.get("commit_sha")?,
                    payload: json_col(r, "payload")?,
                    created_at: r.get("created_at")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every registered supervisor, oldest registration first, whether its
    /// process is alive or not.
    pub fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM supervisors ORDER BY started_at, rowid")?
            .query_map([], supervisor_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Record the cmux workspace `up` opened for `role`, replacing any
    /// earlier one: the UUID is how `up` finds it again, never its title
    /// (ADR-0026).
    pub fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_workspaces(role,workspace_id,created_at) VALUES (?1,?2,?3)
             ON CONFLICT(role) DO UPDATE SET workspace_id=excluded.workspace_id,
                                              created_at=excluded.created_at",
            params![role.as_str(), workspace_id, self.generators.clock.now()],
        )?;
        Ok(())
    }

    /// The workspace last recorded for `role`, whether or not cmux still has it.
    pub fn session_workspace(&self, role: SessionRole) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT workspace_id FROM session_workspaces WHERE role=?1",
                [role.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Forget the workspace of `role`; `false` when none was recorded.
    pub fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM session_workspaces WHERE role=?1",
            [role.as_str()],
        )? == 1)
    }

    /// Forget the workspaces recorded for a role `up` no longer opens (the
    /// resident sessions ADR-0024 and ADR-0041 decision 6 retired: the
    /// maintainer and the resident planner): only the in-cmux supervisor's
    /// and the inbox's are kept. Returns how many were forgotten; the
    /// workspaces themselves are a person's to close.
    pub fn forget_retired_session_workspaces(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM session_workspaces WHERE role NOT IN (?1,?2)",
            params![
                SessionRole::Supervisor.as_str(),
                SessionRole::Inbox.as_str()
            ],
        )?)
    }

    /// Give up ownership of a run that came to rest (`awaiting_integration`
    /// or `failed`). The run's `supervisor_token` stays as a record.
    pub fn release_lease(&mut self, id: &RunId, token: &str) -> Result<()> {
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

    /// `running`, `validating` and `awaiting_integration` runs whose lease
    /// carries a token other than `token`, oldest run first, each with its
    /// wrapper registration. An `awaiting_integration` run is leased only
    /// while its supervisor reviews it (ADR-0027). Runs in other statuses
    /// and runs without a lease are not adoptable, so they are not listed.
    pub fn runs_leased_by_others(&self, token: &str) -> Result<Vec<LeasedRun>> {
        let mut statement = self.conn.prepare(
            "SELECT r.*, l.token, l.pid, l.heartbeat_at FROM task_runs r
             JOIN run_leases l ON l.run_id=r.id
             WHERE r.status IN ('running','validating','awaiting_integration') AND l.token<>?1
             ORDER BY r.rowid",
        )?;
        let rows = statement
            .query_map([token], |r| {
                let run = run_row(&self.runs_dir)(r)?;
                let lease = RunLease {
                    run_id: run.id().clone(),
                    token: r.get("token")?,
                    pid: r.get("pid")?,
                    heartbeat_at: r.get("heartbeat_at")?,
                };
                Ok((run, lease))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(run, lease)| {
                let wrapper = self
                    .conn
                    .query_row(
                        "SELECT * FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                        [&run.id()],
                        process_row,
                    )
                    .optional()?;
                Ok(LeasedRun {
                    run,
                    lease,
                    wrapper,
                })
            })
            .collect()
    }

    /// Take over a `running` or `validating` run whose supervisor is gone
    /// (ADR-0012): the lease row keeps its run but gets this process's
    /// `token`, `pid` and a fresh heartbeat, `task_runs.supervisor_token`
    /// follows so the lease-guarded transitions accept the adopter, and a
    /// `run_adopted` event keeps the claimer's token and pid, the age of the
    /// heartbeat it left behind, and `wrapper` as the caller observed it.
    /// The staleness of the lease under `previous_token` is re-checked
    /// inside the transaction. `Ok(None)` means nothing was taken: the lease
    /// is fresh again, released, or already carries another token (a second
    /// adopter won the race). The wrapper registration and every other row
    /// are untouched.
    pub fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &str,
        token: &str,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let lease = tx
            .query_row(
                "SELECT l.run_id,l.token,l.pid,l.heartbeat_at FROM run_leases l
                 JOIN task_runs r ON r.id=l.run_id
                 WHERE l.run_id=?1 AND l.token=?2
                 AND r.status IN ('running','validating','awaiting_integration')",
                params![id, previous_token],
                lease_row,
            )
            .optional()?;
        let Some(lease) = lease else {
            return Ok(None);
        };
        if !lease_is_stale(&lease, now) {
            return Ok(None);
        }
        let updated = tx.execute(
            "UPDATE run_leases SET token=?3,pid=?4,heartbeat_at=?5
             WHERE run_id=?1 AND token=?2",
            params![id, previous_token, token, pid, now],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        ensure!(
            tx.execute(
                "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
                params![id, token]
            )? == 1,
            "run does not exist"
        );
        run_event(
            &tx,
            id,
            "run_adopted",
            json!({
                "previous_token": lease.token,
                "previous_pid": lease.pid,
                "previous_heartbeat_age_secs": now - lease.heartbeat_at,
                "wrapper": wrapper,
                "token": token,
                "pid": pid,
            }),
        )?;
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        tx.commit()?;
        Ok(Some(result))
    }

    /// Whether this process still holds the run's lease. An adopted-away or
    /// recovered run answers `false`, and its former owner must not touch it.
    pub fn holds_lease(&self, id: &RunId, token: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND token=?2)",
            params![id, token],
            |r| r.get(0),
        )?)
    }

    /// Whether the run has recorded at least one event of `kind`; an
    /// adopter rebuilds what the previous supervisor already did from these.
    pub fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND kind=?2)",
            params![id, kind],
            |r| r.get(0),
        )?)
    }

    /// Record a runtime error and disown the run without changing its status
    /// or touching its processes and resources. Dropping the lease lets
    /// `recover` judge the run by its registered processes alone while this
    /// supervisor keeps serving other runs; a wrapper that has not registered
    /// yet can no longer do so.
    pub fn abandon_run(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::abandon(run, message.to_owned()),
        )?;
        let released = tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        run_event(
            &tx,
            id,
            "runtime_error",
            reason.on(json!({"message": message, "lease_released": released == 1})),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Bind the queue to a repository before any run exists, as `init` does for a
    /// queue resolved from the working directory. A queue already bound to
    /// another repository is refused; rebinding is never implicit.
    pub fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO queue_repository(singleton, git_common_dir) VALUES (1,?1)",
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

    /// Point the queue at `common_dir` whatever it was bound to, and return
    /// the previous binding. Only the explicit `rebind` command calls this
    /// (ADR-0020); `init` and `supervise` go through `bind_repository`.
    pub fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO queue_repository(singleton, git_common_dir) VALUES (1,?1)
             ON CONFLICT(singleton) DO UPDATE SET git_common_dir=excluded.git_common_dir",
            [common_dir],
        )?;
        tx.commit()?;
        Ok(previous)
    }

    /// Every run of the queue, oldest first.
    pub fn all_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs ORDER BY rowid")?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
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
            .prepare("SELECT l.run_id,l.token,l.pid,l.heartbeat_at FROM run_leases l JOIN task_runs r ON r.id=l.run_id ORDER BY r.rowid")?
            .query_map([], lease_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        Ok(self
            .conn
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?)
    }

    pub fn run(&self, id: &RunId) -> Result<TaskRun> {
        self.conn
            .query_row(
                "SELECT * FROM task_runs WHERE id=?1",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?
            .with_context(|| format!("run {id} does not exist"))
    }

    pub fn plan_run(&mut self, id: &RunId, token: &str, plan: &RunPlan) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run cannot be provisioned twice".to_owned(),
            |run| run::start_provisioning(run, plan),
        )?;
        run_event(&tx, id, "run_planned", serde_json::to_value(plan)?)?;
        tx.commit()?;
        Ok(())
    }

    pub fn workspace_created(&mut self, id: &RunId, token: &str, workspace: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "workspace cannot be attached to this run".to_owned(),
            |run| run::attach_workspace(run, workspace.to_owned()),
        )?;
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
        id: &RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        run_event(&self.conn, id, kind, payload)
    }

    /// Record `backend_call_failed`: on `run` (its id) when the call was for
    /// one, otherwise with neither a task nor a run.
    pub fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        match run {
            Some(id) => run_event(&self.conn, id, "backend_call_failed", payload),
            None => {
                self.conn.execute(
                    "INSERT INTO run_events(kind,payload) VALUES ('backend_call_failed',?1)",
                    [serde_json::to_string(&payload)?],
                )?;
                Ok(())
            }
        }
    }

    /// Record an event of the queue itself, on no task, goal or run (the
    /// observer's `observe_started` / `observe_finished`); returns its id.
    pub fn record_queue_event(&self, kind: &str, payload: serde_json::Value) -> Result<EventId> {
        self.conn.execute(
            "INSERT INTO run_events(kind,payload) VALUES (?1,?2)",
            params![kind, serde_json::to_string(&payload)?],
        )?;
        Ok(EventId::new(self.conn.last_insert_rowid()))
    }

    /// The newest event of the queue itself (on no task, goal or run) of one
    /// of `kinds`.
    pub fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM run_events
                 WHERE run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
                   AND kind IN (SELECT value FROM json_each(?1))
                 ORDER BY id DESC LIMIT 1",
                [serde_json::to_string(kinds)?],
                event_row,
            )
            .optional()?)
    }

    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    pub fn event_id_before(&self, unix: i64) -> Result<EventId> {
        Ok(self.conn.query_row(
            "SELECT ifnull(max(id),0) FROM run_events
             WHERE CAST(strftime('%s',created_at) AS INTEGER) < ?1",
            [unix],
            |r| r.get(0),
        )?)
    }

    /// When the observer of `mode` last started or finished (unix seconds).
    pub fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT max(CAST(strftime('%s',created_at) AS INTEGER)) FROM run_events
             WHERE kind IN ('observe_started','observe_finished')
               AND json_extract(payload,'$.mode')=?1",
            [mode],
            |r| r.get(0),
        )?)
    }

    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    pub fn ask_high_water(&self) -> Result<AskId> {
        Ok(self
            .conn
            .query_row("SELECT ifnull(max(id),0) FROM asks", [], |r| r.get(0))?)
    }

    /// What `role` wrote after the marks: findings it recorded and updated
    /// (`finding_recorded`, `finding_updated` after `event_id`) and asks
    /// after `ask_id`.
    pub fn written_by(
        &self,
        role: &str,
        event_id: EventId,
        ask_id: AskId,
    ) -> Result<(i64, i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT
               (SELECT count(*) FROM run_events WHERE id>?2 AND kind='finding_recorded'
                  AND json_extract(payload,'$.by')=?1),
               (SELECT count(*) FROM run_events WHERE id>?2 AND kind='finding_updated'
                  AND json_extract(payload,'$.by')=?1),
               (SELECT count(*) FROM asks WHERE id>?3 AND asked_by=?1)",
            params![role, event_id, ask_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// The latest run whose session opened in `workspace_id`, if any.
    pub fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM task_runs WHERE workspace_id=?1 ORDER BY rowid DESC LIMIT 1",
                [workspace_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The slots held and the slots offered when a backend call failed:
    /// for the supervisor `token`, its leased runs and its `--parallel`;
    /// without one (`up`, `down`), every lease and the sum over every
    /// registered supervisor. `parallel` is `None` when no supervisor is
    /// registered (under `token`).
    pub fn backend_slots(&self, token: Option<&str>) -> Result<(i64, Option<i64>)> {
        let slots = self.conn.query_row(
            "SELECT count(*) FROM run_leases WHERE ?1 IS NULL OR token=?1",
            [token],
            |r| r.get(0),
        )?;
        let parallel = self.conn.query_row(
            "SELECT SUM(parallel) FROM supervisors WHERE ?1 IS NULL OR token=?1",
            [token],
            |r| r.get(0),
        )?;
        Ok((slots, parallel))
    }

    pub fn record_runtime_error(
        &mut self,
        id: &RunId,
        message: &str,
        reason: &Reason,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::abandon(run, message.to_owned()),
        )?;
        run_event(
            &tx,
            id,
            "runtime_error",
            reason.on(json!({"message": message})),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Register the wrapper of a resumed session (ADR-0019): the run is
    /// `needs_session` and leased to `token`, and `begin_resume` cleared the
    /// previous session's process rows.
    pub fn register_resume_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let allowed =
            supervised_run(&tx, id, token)?.is_some_and(|run| run::check_resumable(&run).is_ok());
        ensure!(allowed, "run is not being resumed by this supervisor");
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'wrapper',?2)",
            params![id, pid],
        )
        .context("wrapper is already registered; a resume may only launch once")?;
        run_event(&tx, id, "wrapper_started", json!({"pid": pid}))?;
        tx.commit()?;
        Ok(())
    }

    /// Register the agent of a resumed session; the run stays `needs_session`.
    pub fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, wrapper_pid)?;
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'agent',?2)",
            params![id, agent_pid],
        )?;
        run_event(
            &tx,
            id,
            "agent_started",
            json!({"pid": agent_pid, "session_id": id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let allowed = supervised_run(&tx, id, token)?
            .is_some_and(|run| run::check_ready_for_wrapper(&run).is_ok());
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

    pub fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, wrapper_pid)?;
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'agent',?2)",
            params![id, agent_pid],
        )?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run is not starting".to_owned(),
            run::mark_running,
        )?;
        run_event(
            &tx,
            id,
            "agent_started",
            json!({"pid": agent_pid, "session_id": id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        // Registration is immutable and a run ID is never reused.
        assert_wrapper(&self.conn, id, pid)?;
        self.conn.execute(
            "UPDATE run_processes SET heartbeat_at=?2 WHERE run_id=?1 AND exited_at IS NULL",
            params![id, self.generators.clock.now()],
        )?;
        Ok(())
    }

    pub fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, pid)?;
        tx.execute(
            "UPDATE run_processes SET exited_at=?3,exit_code=?2,heartbeat_at=?3
             WHERE run_id=?1 AND exited_at IS NULL",
            params![id, exit_code, self.generators.clock.now()],
        )?;
        run_event(&tx, id, "session_exited", json!({"exit_code": exit_code}))?;
        tx.commit()?;
        Ok(())
    }

    /// Runs a process is responsible for right now (executing under a
    /// supervisor, or being landed by `integrate`), oldest first.
    pub fn active_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs WHERE status IN ('claimed','starting','running','validating','integrating') ORDER BY rowid")?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Recovery of an orphaned run (the supervisor's, or `recover` by hand). The caller has checked that the
    /// registered processes are dead; `checked_processes` guards against a
    /// registration that happened in between, and a fresh lease is refused here
    /// again. Only this run's lease is deleted; other runs, their leases,
    /// resources and the task's `in_progress` status are left untouched. An
    /// executing run becomes `interrupted`; a run abandoned mid-integration
    /// goes back to `awaiting_integration`, since its validated result is intact.
    pub fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        mut report: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fresh: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND heartbeat_at >= ?2-?3)",
            params![id, self.generators.clock.now(), HEARTBEAT_TIMEOUT_SECS],
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
        let previous = stored_run(&tx, id)?
            .with_context(|| format!("run {id} does not exist"))?
            .status();
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || {
                format!(
                    "run {id} is {}; only unfinished runs can be recovered",
                    previous.as_str()
                )
            },
            run::interrupt,
        )?;
        let leases_deleted = tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        report["previous_status"] = json!(previous);
        report["status"] = json!(run.status());
        report["lease_deleted"] = json!(leases_deleted == 1);
        Reason::new(ReasonCode::Orphaned).apply_to(&mut report);
        run_event(&tx, id, "run_recovered", report)?;
        // The task stays in_progress; a retry is an explicit `ready` and a new run.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    pub fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_processes WHERE run_id=?1 ORDER BY role")?
            .query_map([id], process_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn finish_supervision(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let code: i32 = tx.query_row(
            "SELECT exit_code FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL",
            [id], |r| r.get(0)
        ).context("wrapper has not reported session exit")?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not owned by this supervisor".to_owned(),
            |run| run::finish_session(run, Some(code)),
        )?;
        let mut payload = json!({"status": run.status(), "exit_code": code});
        if code != 0 {
            Reason::of_exit_code(code).apply_to(&mut payload);
        }
        run_event(&tx, id, "supervision_finished", payload)?;
        // Completion and dependency release belong to the next validation stage.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Hand a run whose session went idle after its receipt to validation
    /// with the session still alive (ADR-0027 decision 1): `running` becomes
    /// `validating` under the same lease, and `supervision_finished` records
    /// `session_live: true` with no exit code.
    pub fn finish_supervision_live(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not owned by this supervisor".to_owned(),
            |run| run::finish_session(run, None),
        )?;
        run_event(
            &tx,
            id,
            "supervision_finished",
            json!({"status": run.status(), "exit_code": null, "session_live": true}),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Validate an `awaiting_integration` run again under the lease that
    /// reviews it, after its live session rewrote the receipt for a
    /// `revise` verdict (ADR-0027 decision 2); `revise_finished` is the
    /// record of why.
    pub fn restart_validation(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not awaiting integration under this supervisor".to_owned(),
            run::restart_validation,
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Apply the `send_back` or `cancel` answer of an `approve_landing` ask
    /// to a run awaiting integration that nobody leases: it becomes
    /// `status` (`needs_session` or `failed`) with `reason` as `last_error`,
    /// recorded as `landing_decided` with `payload`.
    pub fn decide_landing(
        &mut self,
        id: &RunId,
        status: crate::domain::RunStatus,
        reason: &str,
        mut payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        ensure!(!leased, "run {id} is leased");
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not awaiting integration"),
            |run| run::decide_landing(run, status, reason.to_owned()),
        )?;
        payload["status"] = json!(run.status());
        payload["reason"] = json!(reason);
        run_event(&tx, id, "landing_decided", payload)?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    pub fn finish_validation(
        &mut self,
        id: &RunId,
        token: &str,
        validation: &Validation,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let refusal = || "run is not validating under this supervisor".to_owned();
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            refusal,
            |run| match (validation.accepted, validation.result_commit.clone()) {
                (true, Some(commit)) => run::accept(run, commit),
                (true, None) => Err(DomainError::InvalidCommit {
                    field: "accepted result commit",
                }),
                (false, commit) => run::reject(
                    run,
                    commit,
                    validation.reason.clone(),
                    !validation.evidence_missing.is_empty()
                        || !validation.scope_violation.is_empty(),
                ),
            },
        )?;
        let status = run.status();
        let mut payload = serde_json::to_value(validation)?;
        payload["status"] = json!(status);
        run_event(&tx, id, "validation_finished", payload)?;
        // No `status` in the payloads: `validation_finished` already
        // reports the park, and `stats` counts it once.
        if status == RunStatus::NeedsSession && !validation.scope_violation.is_empty() {
            run_event(
                &tx,
                id,
                "scope_violation",
                json!({
                    "code": ReasonCode::ScopeViolation,
                    "paths": validation.scope_violation,
                    "allowed": validation.allowed_paths,
                    "reason": validation.reason,
                }),
            )?;
        } else if status == RunStatus::NeedsSession {
            run_event(
                &tx,
                id,
                "evidence_missing",
                json!({
                    "code": ReasonCode::EvidenceMissing,
                    "checks": validation.evidence_missing,
                    "reason": validation.reason,
                }),
            )?;
        }
        // Task completion still waits for integration into main.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// The newest `run_events` id, 0 for an empty queue: the cursor that
    /// `status` hands out and `watch` starts from.
    pub fn latest_event_id(&self) -> Result<EventId> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM run_events", [], |r| {
                r.get(0)
            })?)
    }

    /// Every run event, oldest first, for `stats`. A pure read.
    pub fn all_events(&self) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_events ORDER BY id")?
            .query_map([], event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The goal of every task, for `stats`. A pure read.
    pub fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        Ok(self
            .conn
            .prepare("SELECT id, goal_id FROM tasks")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Events with `after < id <= upto` that `filter` keeps, oldest first,
    /// at most `limit`. A pure read.
    pub fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let kinds = filter
            .kinds
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM run_events WHERE id>?1 AND id<=?2
                 AND (?3 IS NULL OR kind IN (SELECT value FROM json_each(?3)))
                 AND (?4 IS NULL OR run_id=?4) AND (?5 IS NULL OR task_id=?5)
                 AND (?6 IS NULL OR goal_id=?6
                      OR task_id IN (SELECT id FROM tasks WHERE goal_id=?6))
                 AND (?7 IS NULL OR julianday(created_at)>=julianday(?7))
                 AND (?8 IS NULL OR julianday(created_at)<julianday(?8))
                 ORDER BY id LIMIT ?9",
            )?
            .query_map(
                params![
                    after,
                    upto,
                    kinds,
                    filter.run,
                    filter.task,
                    filter.goal,
                    filter.since,
                    filter.until,
                    i64::try_from(limit)?
                ],
                event_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every event of one run, oldest first.
    pub fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
            .query_map([id], event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The latest run of every `in_progress` task, oldest first: the runs
    /// `status` judges for attention. An older run of a retried task is
    /// history, and a completed or canceled task needs nobody.
    pub fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)
                 ORDER BY r.rowid",
            )?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push (`push_finished`), oldest first. A later successful
    /// push carries every earlier landing, so it clears them all.
    pub fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r WHERE r.status='integrated'
                 AND r.id IN (SELECT run_id FROM run_events WHERE kind='push_failed'
                   AND id>(SELECT COALESCE(MAX(id),0) FROM run_events WHERE kind='push_finished'))
                 ORDER BY r.rowid",
            )?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every run in one status, oldest first; `up` reports the runs that
    /// wait for a person or the supervisor (`awaiting_integration`, `needs_session`).
    pub fn runs_with_status(&self, status: crate::domain::RunStatus) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs WHERE status=?1 ORDER BY rowid")?
            .query_map([status.as_str()], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The oldest run awaiting integration by validation time: the FIFO
    /// order of the merge queue. Runs waiting for a session are skipped;
    /// they are resumed explicitly by `integrate ID`.
    pub fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        Ok(self
            .conn
            .query_row(
                "SELECT r.* FROM task_runs r WHERE r.status='awaiting_integration'
                 ORDER BY (SELECT MIN(e.id) FROM run_events e
                           WHERE e.run_id=r.id AND e.kind='validation_finished') NULLS LAST,
                          r.rowid
                 LIMIT 1",
                [],
                run_row(&self.runs_dir),
            )
            .optional()?)
    }

    /// Take the single integration slot for an awaiting run or one coming
    /// back from a session: the run becomes `integrating` and this process
    /// owns it through a lease row for the duration, so `doctor` can see who
    /// is landing what. A lease this `token` already holds (a supervisor
    /// landing the run it resumed) is kept; one under another token refuses. `one_integrating_run_per_queue` backs the explicit check.
    pub fn begin_integration(
        &mut self,
        id: &RunId,
        token: &str,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let busy: Option<RunId> = tx
            .query_row(
                "SELECT id FROM task_runs WHERE status='integrating'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = busy {
            ensure!(
                other == *id,
                "run {other} is integrating; one run lands at a time (see doctor if it is stuck)"
            );
            bail!("run {id} is already integrating (see doctor if it is stuck)");
        }
        let previous = stored_run(&tx, id)?
            .with_context(|| format!("run {id} does not exist"))?
            .status();
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || {
                format!(
                    "run {id} is {}; only a run awaiting integration or a session can be integrated",
                    previous.as_str()
                )
            },
            run::begin_integration,
        )?;
        // A supervisor landing a run it resumed or reviewed already holds
        // its lease under the same token; any other lease means someone
        // owns the run.
        ensure!(
            tx.execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(run_id) DO UPDATE SET pid=excluded.pid,heartbeat_at=excluded.heartbeat_at
                 WHERE run_leases.token=excluded.token",
                params![id, token, std::process::id(), self.generators.clock.now()],
            )? == 1,
            "run is still leased"
        );
        run_event(
            &tx,
            id,
            "integration_started",
            json!({"main": main, "previous_status": previous, "pid": std::process::id()}),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// `needs_session` runs, oldest first, with their lease (if any), their
    /// wrapper registration (the latest session's) and how many resumes
    /// were started: what the supervisor judges for a resume (ADR-0019).
    pub fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>> {
        let runs = self.runs_with_status(crate::domain::RunStatus::NeedsSession)?;
        runs.into_iter()
            .map(|run| {
                let lease = self.run_lease(run.id())?;
                let wrapper = self
                    .conn
                    .query_row(
                        "SELECT * FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                        [&run.id()],
                        process_row,
                    )
                    .optional()?;
                let resumes = resume_count(&self.conn, run.id())?;
                Ok(ResumeCandidate {
                    run,
                    lease,
                    wrapper,
                    resumes,
                })
            })
            .collect()
    }

    /// Take a `needs_session` run for a resume: in one transaction, check
    /// that it is still `needs_session` with resumes left (ADR-0047
    /// decision 24: [`ResumeCount::exhausted`]), no lease but a stale one (which is replaced) and no
    /// session still heartbeating, lease it to `token`, clear the previous
    /// session's process rows, make `token` its supervisor and record
    /// `resume_started` (`attempt`, the number of every resume so far and
    /// this one, `counted`, `reason`, `main`). `Ok(None)` means another
    /// process took it or it changed meanwhile.
    pub fn begin_resume(
        &mut self,
        id: &RunId,
        token: &str,
        main: &CommitSha,
        reason: Option<&str>,
    ) -> Result<Option<(TaskRun, usize)>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let Some(run) = stored_run(&tx, id)?.filter(|run| run::check_resumable(run).is_ok()) else {
            return Ok(None);
        };
        let events = run_events_of(&tx, id)?;
        let resumes = ResumeCount::of(&events);
        if resumes.exhausted() {
            return Ok(None);
        }
        // A resume of a run parked only by a conflict after its review
        // passed is not one of the counted attempts.
        let counted = !resume::parked_for_conflict_only(&events);
        let Some(previous) = lease_parked_run(&tx, id, token, now, true)? else {
            return Ok(None);
        };
        let attempt = resumes.total() + 1;
        run_event(
            &tx,
            id,
            "lease_acquired",
            json!({"pid": std::process::id(), "reason": "resume", "previous_token": previous}),
        )?;
        run_event(
            &tx,
            id,
            "resume_started",
            json!({"attempt": attempt, "counted": counted, "reason": reason.or(run.last_error()), "main": main}),
        )?;
        tx.commit()?;
        Ok(Some((run.relocated(&self.runs_dir), attempt)))
    }

    /// Move a `needs_session` run on without a session because an earlier
    /// attempt already resolved it (its receipt names the clean worktree
    /// `head`, which sits on `main`): under the same checks as
    /// [`Self::begin_resume`] but whatever the attempts so far, lease it to
    /// `token` (`lease_acquired` with `reason: resume_skipped`), make
    /// `token` its supervisor and record `resume_skipped` (`head`, `main`,
    /// `approved`, `status`). An approved run stays `needs_session` for the
    /// caller to land under the lease; an unapproved one becomes
    /// `validating`. No `resume_started` is recorded, so no attempt is used.
    /// `Ok(None)` means another process took it or it changed meanwhile.
    pub fn skip_resume(
        &mut self,
        id: &RunId,
        token: &str,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        // No session starts: the earlier session's rows stay as they are.
        let Some(previous) = lease_parked_run(&tx, id, token, now, false)? else {
            return Ok(None);
        };
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not needs_session"),
            |run| run::skip_resume(run, approved),
        )?;
        let status = run.status();
        run_event(
            &tx,
            id,
            "lease_acquired",
            json!({"pid": std::process::id(), "reason": "resume_skipped", "previous_token": previous}),
        )?;
        run_event(
            &tx,
            id,
            "resume_skipped",
            json!({"head": head, "main": main, "approved": approved, "status": status.as_str()}),
        )?;
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// End a resume: record `resume_finished` (with `status`, the run's
    /// status after it) and, unless the caller goes on to land the run
    /// under the same lease (`keep_lease`), release the lease. `status`
    /// `awaiting_integration` or `failed` moves the run there from
    /// `needs_session` (`reason` then becomes `last_error`); `None` keeps it
    /// `needs_session`.
    pub fn finish_resume(
        &mut self,
        id: &RunId,
        token: &str,
        status: Option<crate::domain::RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        mut payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        if let Some(status) = status {
            apply(
                &tx,
                refusals(&self.runs_dir, &self.generators),
                id,
                None,
                || format!("run {id} is not needs_session"),
                |run| run::finish_resume(run, status, reason.map(str::to_owned)),
            )?;
        }
        let status = status.unwrap_or(RunStatus::NeedsSession);
        payload["status"] = json!(status.as_str());
        run_event(&tx, id, "resume_finished", payload)?;
        if !keep_lease {
            tx.execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
                params![id, token],
            )?;
            run_event(
                &tx,
                id,
                "lease_released",
                json!({"reason": "resume_finished"}),
            )?;
        }
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Park an integrating run for a session: `needs_session` with the reason
    /// in `last_error`; the slot and lease are released. The worktree is left
    /// as the landing attempt left it.
    pub fn defer_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.leave_integration(
            id,
            token,
            |run| run::defer_integration(run, reason.to_owned()),
            reason,
            "integration_deferred",
            detail,
        )
    }

    /// End an integrating run whose rewritten receipt reports `failed`. The
    /// receipt's JSON goes into the `integration_failed` event, since the DB
    /// otherwise holds only the receipt seen at validation time.
    pub fn fail_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        self.leave_integration(
            id,
            token,
            |run| run::fail_integration(run, reason.to_owned()),
            reason,
            "integration_failed",
            json!({"code": ReasonCode::WorkerFailed, "receipt": receipt}),
        )
    }

    /// Give the slot back after an error before `main` moved: the run returns
    /// to the status it had when the landing started.
    pub fn abort_integration(
        &mut self,
        id: &RunId,
        token: &str,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let revert_to: RunStatus = revert_to.parse()?;
        self.leave_integration(
            id,
            token,
            |run| run::abort_integration(run, revert_to, message.to_owned()),
            message,
            "integration_error",
            reason.on(json!({})),
        )
    }

    fn leave_integration(
        &mut self,
        id: &RunId,
        token: &str,
        command: impl FnOnce(TaskRun) -> Result<TaskRun, DomainError>,
        reason: &str,
        kind: &str,
        mut detail: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not integrating"),
            command,
        )?;
        tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        detail["status"] = json!(run.status());
        detail["reason"] = json!(reason);
        run_event(&tx, id, kind, detail)?;
        run_event(&tx, id, "lease_released", json!({"reason": kind}))?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Complete the task once its run landed on `main`: the run becomes
    /// `integrated` with the landed commit as `result_commit`, the task
    /// `completed`, and the slot and lease are released. The status
    /// predicates make a repeated or concurrent completion fail without effect.
    pub fn finish_integration(
        &mut self,
        id: &RunId,
        token: &str,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = self.generators.clock.system_time();
        renew_lease(&tx, id, token, unix_seconds(at))?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not integrating"),
            |run| run::finish_integration(run, landing.commit.clone()),
        )?
        .relocated(&self.runs_dir);
        tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        ensure!(
            tx.execute(
                "UPDATE tasks SET status='completed', updated_at=?2
                 WHERE id=?1 AND status='in_progress'",
                params![run.task_id(), timestamp(at)]
            )? == 1,
            "task {} is not in progress",
            run.task_id()
        );
        let mut payload = serde_json::to_value(landing)?;
        payload["result_commit"] = json!(landing.commit);
        payload["git_common_dir"] = json!(common_dir);
        run_event(&tx, id, "run_integrated", payload)?;
        run_event(&tx, id, "lease_released", json!({"reason": "integrated"}))?;
        event(
            &tx,
            run.task_id(),
            Some(id),
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
        )?;
        let task = read_task(&tx, run.task_id())?;
        tx.commit()?;
        Ok((task, run))
    }

    /// Note a post-landing cleanup failure (worktree or branch removal) on a
    /// run whose status no longer changes.
    pub fn record_cleanup_failure(
        &mut self,
        id: &RunId,
        message: &str,
        reason: &Reason,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::record_cleanup_failure(run, message.to_owned()),
        )?;
        run_event(
            &tx,
            id,
            "cleanup_failed",
            reason.on(json!({"message": message})),
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl SqliteQueue {
    /// Record a confirmed cmux close. Only an accepted run whose workspace is
    /// still recorded as open qualifies; the worktree and branch stay for integration.
    pub fn workspace_closed(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        renew_lease(&tx, id, token, now)?;
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            not_at_rest,
            |run| run::workspace_closed(run, now),
        )?
        .relocated(&self.runs_dir);
        run_event(
            &tx,
            id,
            "workspace_closed",
            json!({"workspace_id": result.workspace_id(), "closed_at": result.workspace_closed_at()}),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// A failed close leaves `workspace_closed_at` null so the workspace is never
    /// treated as cleaned; the run status does not change.
    pub fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            not_at_rest,
            |run| run::record_close_failure(run, message.to_owned()),
        )?
        .relocated(&self.runs_dir);
        run_event(
            &tx,
            id,
            "cleanup_failed",
            reason.on(json!({"workspace_id": result.workspace_id(), "message": message})),
        )?;
        tx.commit()?;
        Ok(result)
    }
}

impl SqliteQueue {
    /// The latest run of every `in_progress` task that is `failed` or
    /// `interrupted`, oldest first: the runs the triage looks at. An older
    /// run of a retried task is history.
    pub fn runs_to_triage(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .latest_runs_in_progress()?
            .into_iter()
            .filter(|run| run::check_triageable(run).is_ok())
            .collect())
    }

    /// Take a `failed` or `interrupted` run for its triage: in one
    /// transaction, check that its task is `in_progress`, that it has no
    /// lease but a stale one (which is replaced) and that it is not triaged
    /// since its last resume ([`crate::domain::triage_state`]), and that it
    /// is still the task's latest run, lease it to
    /// `token` and record `lease_acquired` and `triage_started` (`attempt`,
    /// `status`). `Ok(None)` means another process took it or it changed.
    pub fn begin_triage(&mut self, id: &RunId, token: &str) -> Result<Option<(TaskRun, usize)>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let run = tx
            .query_row(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE r.id=?1 AND r.status IN ('failed','interrupted') AND t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(None);
        };
        let events: Vec<RunEvent> = tx
            .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
            .query_map([id], event_row)?
            .collect::<rusqlite::Result<_>>()?;
        if crate::domain::triage_state(&events) != crate::domain::TriageState::Pending {
            return Ok(None);
        }
        let lease = tx
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?;
        if let Some(lease) = &lease {
            if !lease_is_stale(lease, now) {
                return Ok(None);
            }
            tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        }
        tx.execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
            params![id, token, std::process::id(), now],
        )?;
        let attempt = events.iter().filter(|e| e.kind == "triage_started").count() + 1;
        run_event(
            &tx,
            id,
            "lease_acquired",
            json!({"pid": std::process::id(), "reason": "triage", "previous_token": lease.map(|l| l.token)}),
        )?;
        run_event(
            &tx,
            id,
            "triage_started",
            json!({"attempt": attempt, "status": run.status().as_str()}),
        )?;
        tx.commit()?;
        Ok(Some((run, attempt)))
    }

    /// Act on the triage's verdict under the triage's lease and record
    /// `triage_finished` with `payload`, the `action` and the run's status
    /// after it, in one transaction: `Retry` makes the task `ready`,
    /// `Resume` the run `needs_session`, `Ask` changes nothing. The lease
    /// stays for the workspace's close.
    pub fn finish_triage(
        &mut self,
        id: &RunId,
        token: &str,
        action: &TriageAction,
        mut payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        ensure!(
            run::check_triageable(&run).is_ok(),
            "run {id} is {}; only failed or interrupted runs are triaged",
            run.status().as_str()
        );
        match action {
            TriageAction::Retry => {
                super::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
            }
            TriageAction::Resume { instruction } => {
                apply(
                    &tx,
                    refusals(&self.runs_dir, &self.generators),
                    id,
                    None,
                    || format!("run {id} changed"),
                    |run| run::resume_after_triage(run, instruction.clone()),
                )?;
                payload["instruction"] = json!(instruction);
                payload["code"] = json!(ReasonCode::TriageResume);
            }
            TriageAction::Ask { ask_id } => payload["ask_id"] = json!(ask_id),
        }
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        payload["action"] = json!(action.as_str());
        payload["status"] = json!(result.status().as_str());
        run_event(&tx, id, "triage_finished", payload)?;
        tx.commit()?;
        Ok(result)
    }

    /// End a `needs_session` run whose resumes are used up (ADR-0024's
    /// Consequences, ADR-0047 decision 24): in one transaction, check that
    /// it is still `needs_session` with its resumes used up
    /// ([`ResumeCount::exhausted`]), the latest run of an `in_progress`
    /// task, and not leased but stale; make it `failed` with `reason` as
    /// `last_error`, and record `triage_finished` (`by: runtime`), so the
    /// triage takes it as decided. [`Exhaustion::Ask`] names the ask a
    /// person answers like a triage's (action `ask`);
    /// [`Exhaustion::Inherit`] makes the task `ready` again for a run that
    /// carries this run's branch over (action `retry_inherit`, with
    /// `auto_repaired` `repair: inherit_retry`). `Ok(None)` means it
    /// changed.
    pub fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let run = tx
            .query_row(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE r.id=?1 AND r.status='needs_session' AND t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(None);
        };
        let events = run_events_of(&tx, id)?;
        let resumes = ResumeCount::of(&events);
        let leased = tx
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?
            .is_some_and(|lease| !lease_is_stale(&lease, now));
        if !resumes.exhausted() || leased {
            return Ok(None);
        }
        // Once per task, and only for a run still parked by a conflict
        // alone: checked again here, where no other supervisor can retry it.
        if matches!(exhaustion, Exhaustion::Inherit { .. }) {
            let task_events: Vec<RunEvent> = tx
                .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id")?
                .query_map([run.task_id()], event_row)?
                .collect::<rusqlite::Result<_>>()?;
            if !resume::inherits_on_exhaustion(&events, &task_events) {
                return Ok(None);
            }
        }
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} changed"),
            |run| run::exhaust_resumes(run, reason.to_owned()),
        )?;
        let mut payload = json!({
            "code": ReasonCode::ResumeExhausted,
            "by": "runtime",
            "reason": reason,
            "resumes": resumes.total(),
            "counted_resumes": resumes.counted,
            "conflict_only_resumes": resumes.conflict_only,
            "previous_status": run.status().as_str(),
            "status": result.status().as_str(),
        });
        match exhaustion {
            Exhaustion::Ask(ask_id) => {
                payload["verdict"] = json!("ask");
                payload["action"] = json!("ask");
                payload["ask_id"] = json!(ask_id);
            }
            Exhaustion::Inherit { branch, head } => {
                super::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
                payload["verdict"] = json!(resume::RETRY_INHERIT);
                payload["action"] = json!(resume::RETRY_INHERIT);
                payload["inherit"] = json!({"branch": branch, "head": head});
                run_event(
                    &tx,
                    id,
                    "auto_repaired",
                    json!({
                        "layer": "runtime",
                        "repair": "inherit_retry",
                        "conditions": {
                            "review": "pass",
                            "parked": ReasonCode::RebaseConflict,
                            "counted_resumes": resumes.counted,
                            "conflict_only_resumes": resumes.conflict_only,
                            "branch": branch,
                            "head": head,
                        },
                        "detail": "the resumes were used up on conflicts with main; the task is ready again for a run that carries this run's branch over",
                    }),
                )?;
            }
        }
        run_event(&tx, id, "triage_finished", payload)?;
        tx.commit()?;
        Ok(Some(result.relocated(&self.runs_dir)))
    }

    /// Record that the triage or the supervisor's sweep closed
    /// `workspace_id` of the run as `workspace_closed` (`payload` with the
    /// `workspace_id`): the worker's own (`workspace_closed_at` is set) or a
    /// resume's.
    pub fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        mut payload: serde_json::Value,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if let Some(run) = stored_run(&tx, id)? {
            let from = run.status();
            let run = run::record_closed_workspace(run, workspace_id, now)?;
            save_run(&tx, &run, from, None)?;
        }
        payload["workspace_id"] = json!(workspace_id);
        run_event(&tx, id, "workspace_closed", payload)?;
        tx.commit()?;
        Ok(())
    }

    /// The workspaces of the runs that ended (`integrated`, `succeeded`,
    /// `failed`, `interrupted`) and that nobody leases, except the runs the
    /// triage takes (the latest `failed` / `interrupted` run of an
    /// `in_progress` task): the worker's workspace and every workspace a
    /// `workspace_created` or `resume_finished` of the run names, whether
    /// or not its close is recorded (cmux's list decides), ordered by run.
    pub fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        let mut statement = self.conn.prepare(
            "WITH ended AS (
               SELECT r.id, r.status, r.workspace_id, r.rowid AS run_row FROM task_runs r
               JOIN tasks t ON t.id=r.task_id
               WHERE r.status IN ('integrated','succeeded','failed','interrupted')
               AND NOT (r.status IN ('failed','interrupted') AND t.status='in_progress'
                        AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id))
               AND NOT EXISTS (SELECT 1 FROM run_leases l WHERE l.run_id=r.id)
             )
             SELECT id, status, workspace_id, run_row, 0 AS event_id FROM ended
             WHERE workspace_id IS NOT NULL
             UNION ALL
             SELECT ended.id, ended.status, json_extract(e.payload,'$.workspace_id'), ended.run_row, e.id
             FROM run_events e JOIN ended ON ended.id=e.run_id
             WHERE e.kind IN ('workspace_created','resume_finished')
             AND json_extract(e.payload,'$.workspace_id') IS NOT NULL
             ORDER BY 4, 5",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(EndedRunWorkspace {
                run_id: row.get(0)?,
                status: enum_col(row, "status")?,
                workspace_id: row.get(2)?,
            })
        })?;
        let mut seen = std::collections::HashSet::new();
        let mut workspaces: Vec<EndedRunWorkspace> = Vec::new();
        for row in rows {
            let row = row?;
            if seen.insert((row.run_id.clone(), row.workspace_id.clone())) {
                workspaces.push(row);
            }
        }
        Ok(workspaces)
    }

    /// The worktrees of the runs nobody leases that ended (`integrated`,
    /// `succeeded`, `failed`, `interrupted`), or of any status once their
    /// task is `completed` or `canceled`, by run; each is where the run's
    /// worktree lives under the run directory, whether or not it is still
    /// there.
    pub fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        let mut statement = self.conn.prepare(
            "SELECT r.id, r.task_id, r.status AS run_status, t.status AS task_status, r.branch
             FROM task_runs r
             JOIN tasks t ON t.id=r.task_id
             WHERE r.worktree_path IS NOT NULL
             AND (r.status IN ('integrated','succeeded','failed','interrupted')
                  OR t.status IN ('completed','canceled'))
             AND NOT EXISTS (SELECT 1 FROM run_leases l WHERE l.run_id=r.id)
             ORDER BY r.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            let run_id: RunId = row.get(0)?;
            Ok(EndedRunWorktree {
                worktree: RunPaths::new(&self.runs_dir, &run_id)
                    .worktree
                    .to_string_lossy()
                    .into_owned(),
                run_id,
                task_id: row.get(1)?,
                status: enum_col(row, "run_status")?,
                task_status: enum_col(row, "task_status")?,
                branch: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Apply the answer of a triage's `decide` ask to its `failed` or
    /// `interrupted` run that nobody leases and that is still its task's
    /// latest run, and close the ask, in one transaction (an ask closed
    /// meanwhile, by another supervisor or a person, is refused): `retry` makes the task `ready`, `resume` the run
    /// `needs_session` with `reason` as `last_error`, `cancel` cancels the
    /// task. Recorded as `triage_decided` (`ask_id`, `answer`, `reason`,
    /// `status`).
    pub fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let lease = tx
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?;
        ensure!(
            lease.is_none_or(|lease| lease_is_stale(&lease, now)),
            "run {id} is leased"
        );
        // The stale lease goes, so its holder cannot renew it and write
        // after this decision (ADR-0039 decision 7).
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        let run = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        ensure!(
            run::check_triageable(&run).is_ok(),
            "run {id} is {}, not failed or interrupted",
            run.status().as_str()
        );
        let latest: RunId = tx.query_row(
            "SELECT id FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [run.task_id()],
            |r| r.get(0),
        )?;
        ensure!(
            latest == *id,
            "run {id} is no longer the latest run of task {}",
            run.task_id()
        );
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE id=?1 AND run_id=?2
             AND answered_at IS NOT NULL AND closed_at IS NULL)",
            params![ask_id, id],
            |r| r.get(0),
        )?;
        ensure!(
            pending,
            "ask {ask_id} is not an answered, unclosed ask of run {id}"
        );
        match answer {
            "retry" => {
                super::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
            }
            "resume" => {
                apply(
                    &tx,
                    refusals(&self.runs_dir, &self.generators),
                    id,
                    None,
                    || format!("run {id} changed"),
                    |run| run::resume_after_triage(run, reason.to_owned()),
                )?;
            }
            "cancel" => {
                super::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Cancel,
                    &self.generators.clock.timestamp(),
                )?;
            }
            other => bail!("{other:?} is not an answer the triage applies"),
        }
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            params![ask_id, now],
        )?;
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        let mut payload = json!({"ask_id": ask_id, "answer": answer, "reason": reason, "status": result.status().as_str()});
        if answer == "resume" {
            payload["code"] = json!(ReasonCode::TriageResume);
        }
        run_event(&tx, id, "triage_decided", payload)?;
        tx.commit()?;
        Ok(result)
    }

    /// The answered `decide` asks the supervisor opened about a `failed` or
    /// `interrupted` run that nobody closed, oldest first: the triage's
    /// asks whose answers the supervisor applies.
    pub fn triage_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        Ok(self
            .asks(super::asks::AskQuery::default())?
            .into_iter()
            .filter(|ask| {
                ask.kind == crate::domain::AskKind::Decide
                    && ask.asked_by == TRIAGE_ASKER
                    && ask.run_id.is_some()
                    && ask.answered_at.is_some()
            })
            .collect())
    }
}

/// The run as stored (its paths not relocated), for a command to start from.
fn stored_run(conn: &Connection, id: &RunId) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row("SELECT * FROM task_runs WHERE id=?1", [id], stored_run_row)
        .optional()?)
}

/// The run as stored if `token` is its supervisor.
fn supervised_run(conn: &Connection, id: &RunId, token: &str) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_runs WHERE id=?1 AND supervisor_token=?2",
            params![id, token],
            stored_run_row,
        )
        .optional()?)
}

/// Save what a domain command made of a run read with [`stored_run`].
/// `status=from` (the status the command started from) and, when given,
/// `supervisor_token=token` only detect a concurrent change: the domain
/// decided the move. Whether a row was updated.
fn save_run(
    conn: &Connection,
    run: &TaskRun,
    from: RunStatus,
    token: Option<&str>,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE task_runs SET status=?3,repo_path=?4,run_dir=?5,branch=?6,worktree_path=?7,
         receipt_path=?8,log_path=?9,workspace_id=?10,result_commit=?11,last_error=?12,
         workspace_closed_at=?13
         WHERE id=?1 AND status=?2 AND (?14 IS NULL OR supervisor_token=?14)",
        params![
            run.id(),
            from.as_str(),
            run.status().as_str(),
            run.repo_path(),
            run.run_dir(),
            run.branch(),
            run.worktree_path(),
            run.receipt_path(),
            run.log_path(),
            run.workspace_id(),
            run.result_commit(),
            run.last_error(),
            run.workspace_closed_at(),
            token,
        ],
    )? == 1)
}

/// Read the run, apply the domain `command` and save the result, inside
/// the caller's transaction; the stored (not relocated) run comes back.
/// A missing run, a command the domain refuses and a row that changed
/// meanwhile all fail with `refusal`, the message the store has always
/// given for the operation. The reason of a domain refusal is not part of
/// that message; it is kept in `refusals` instead.
fn apply(
    conn: &Connection,
    refusals: Refusals<'_>,
    id: &RunId,
    token: Option<&str>,
    refusal: impl Fn() -> String,
    command: impl FnOnce(TaskRun) -> Result<TaskRun, DomainError>,
) -> Result<TaskRun> {
    let run = stored_run(conn, id)?.ok_or_else(|| anyhow!(refusal()))?;
    let from = run.status();
    let run = command(run).map_err(|reason| {
        let message = refusal();
        refusals.record(id, &message, &reason);
        anyhow!(message)
    })?;
    ensure!(save_run(conn, &run, from, token)?, refusal());
    Ok(run)
}

/// The file in a run's directory that keeps why the domain refused a
/// transition of the run: one `[<unix seconds>] <message>: <DomainError>`
/// line per refusal, `<message>` being the error the operation returned.
pub const REFUSALS_LOG: &str = "refusals.log";

/// Where [`apply`] leaves the reason of a refused transition. The
/// operation's error keeps the message the store has always given, so the
/// CLI's `{"error"}` and `last_error` do not change; the domain's reason
/// (the status the run was in and the command) goes to [`REFUSALS_LOG`].
/// The caller's transaction rolls back after a refusal, which is why the
/// reason is not a run event.
#[derive(Clone, Copy)]
struct Refusals<'a> {
    runs_dir: &'a Path,
    generators: &'a Generators,
}

fn refusals<'a>(runs_dir: &'a Path, generators: &'a Generators) -> Refusals<'a> {
    Refusals {
        runs_dir,
        generators,
    }
}

impl Refusals<'_> {
    /// Append the line. A run directory that is gone or does not accept the
    /// write loses the line and not the operation's error.
    fn record(&self, id: &RunId, message: &str, reason: &DomainError) {
        let run_dir = RunPaths::new(self.runs_dir, id).run_dir;
        if !run_dir.is_dir() {
            return;
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join(REFUSALS_LOG))
        {
            let _ = writeln!(
                file,
                "[{}] {message}: {reason}",
                self.generators.clock.now()
            );
        }
    }
}

/// Why a workspace close of a run is not recorded.
fn not_at_rest() -> String {
    "run is not awaiting integration or a session with an open workspace under this supervisor"
        .to_owned()
}

/// Renew `token`'s lease on the run at `now`, inside the caller's
/// `BEGIN IMMEDIATE` transaction, or refuse when the lease row is gone or
/// carries another token (ADR-0039 decision 7). A heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS` is no reason to refuse: after a host sleep the
/// row is still this supervisor's until an adopter swaps the token or
/// `recover` removes it, and SQLite serializes that write with this one.
fn renew_lease(conn: &Connection, id: &RunId, token: &str, now: i64) -> Result<()> {
    let renewed = conn.execute(
        "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
        params![id, token, now],
    )?;
    ensure!(
        renewed == 1,
        "run lease is missing or held by another supervisor"
    );
    Ok(())
}

fn lease_row(r: &Row<'_>) -> rusqlite::Result<RunLease> {
    Ok(RunLease {
        run_id: r.get(0)?,
        token: r.get(1)?,
        pid: r.get(2)?,
        heartbeat_at: r.get(3)?,
    })
}

fn supervisor_row(r: &Row<'_>) -> rusqlite::Result<SupervisorRegistration> {
    Ok(SupervisorRegistration {
        token: r.get("token")?,
        pid: r.get("pid")?,
        parallel: r.get("parallel")?,
        started_at: r.get("started_at")?,
        heartbeat_at: r.get("heartbeat_at")?,
        mode: r
            .get::<_, Option<String>>("mode")?
            .map(|_| enum_col(r, "mode"))
            .transpose()?,
        workspace_id: r.get("workspace_id")?,
        binary_version: r.get("binary_version")?,
        handoff_accepted: r.get::<_, Option<i64>>("handoff_accepted")? == Some(1),
        handoff_binary: r.get("handoff_binary")?,
        auto_update: r.get::<_, Option<i64>>("auto_update")? == Some(1),
    })
}

fn assert_wrapper(conn: &Connection, id: &RunId, pid: u32) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND pid=?2 AND exited_at IS NULL)",
        params![id,pid], |r| r.get(0))?;
    ensure!(valid, "wrapper is not the live owner of this run");
    Ok(())
}

fn run_event(conn: &Connection, id: &RunId, kind: &str, payload: serde_json::Value) -> Result<()> {
    let task_id: TaskId =
        conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    event(conn, task_id, Some(id), kind, payload)
}

/// Lease a `needs_session` run to `token` for a resume or its skip, inside
/// the caller's transaction: no lease but a stale one (which is replaced)
/// and no session still heartbeating; with `clear_processes` (a resumed
/// session registers in their place) the previous session's process rows
/// are cleared (their history stays in the run's events), and `token`
/// becomes its supervisor. `Ok(Some(previous lease token))` once leased,
/// `Ok(None)` when the run is not free to take.
fn lease_parked_run(
    tx: &Connection,
    id: &RunId,
    token: &str,
    now: i64,
    clear_processes: bool,
) -> Result<Option<Option<String>>> {
    let parked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE id=?1 AND status='needs_session')",
        [id],
        |r| r.get(0),
    )?;
    if !parked {
        return Ok(None);
    }
    let lease = tx
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            [id],
            lease_row,
        )
        .optional()?;
    if let Some(lease) = &lease {
        if !lease_is_stale(lease, now) {
            return Ok(None);
        }
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
    }
    // A session still heartbeating is left alone.
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1
         AND exited_at IS NULL AND heartbeat_at >= ?2-?3)",
        params![id, now, HEARTBEAT_TIMEOUT_SECS],
        |r| r.get(0),
    )?;
    if live {
        return Ok(None);
    }
    if clear_processes {
        tx.execute("DELETE FROM run_processes WHERE run_id=?1", [id])?;
    }
    tx.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
        params![id, token, std::process::id(), now],
    )?;
    tx.execute(
        "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
        params![id, token],
    )?;
    Ok(Some(lease.map(|l| l.token)))
}

fn run_events_of(conn: &Connection, id: &RunId) -> Result<Vec<RunEvent>> {
    Ok(conn
        .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
        .query_map([id], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The run's resumes, counted as ADR-0047 decision 24 says.
fn resume_count(conn: &Connection, id: &RunId) -> Result<ResumeCount> {
    Ok(ResumeCount::of(&run_events_of(conn, id)?))
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

pub(super) fn processes_for_task(conn: &Connection, task_id: TaskId) -> Result<Vec<RunProcess>> {
    Ok(conn.prepare("SELECT p.* FROM run_processes p JOIN task_runs r ON r.id=p.run_id WHERE r.task_id=?1 ORDER BY r.rowid,p.role")?
        .query_map([task_id], process_row)?.collect::<rusqlite::Result<_>>()?)
}

/// The run store port over the inherent methods above, which callers that
/// hold a `SqliteQueue` keep using directly.
impl RunStore for SqliteQueue {
    fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
    ) -> Result<ClaimOutcome> {
        SqliteQueue::claim_for_supervisor(self, base_commit, token)
    }
    fn heartbeat_leases(&self, token: &str) -> Result<usize> {
        SqliteQueue::heartbeat_leases(self, token)
    }
    fn register_supervisor(
        &mut self,
        token: &str,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::register_supervisor(self, token, pid, parallel, binary_version)
    }
    fn deregister_supervisor(&self, token: &str) -> Result<bool> {
        SqliteQueue::deregister_supervisor(self, token)
    }
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        SqliteQueue::supervisors(self)
    }
    fn release_lease(&mut self, id: &RunId, token: &str) -> Result<()> {
        SqliteQueue::release_lease(self, id, token)
    }
    fn runs_leased_by_others(&self, token: &str) -> Result<Vec<LeasedRun>> {
        SqliteQueue::runs_leased_by_others(self, token)
    }
    fn accept_handoff(&self, token: &str) -> Result<()> {
        SqliteQueue::accept_handoff(self, token)
    }
    fn request_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        SqliteQueue::request_handoff(self, token, binary)
    }
    fn handoff_request(&self, token: &str) -> Result<Option<String>> {
        SqliteQueue::handoff_request(self, token)
    }
    fn cancel_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        SqliteQueue::cancel_handoff(self, token, binary)
    }
    fn resume_registration(
        &mut self,
        token: &str,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::resume_registration(self, token, pid, binary_version)
    }
    fn runs_leased_by(&self, token: &str) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_leased_by(self, token)
    }
    fn set_auto_update(&self, token: &str, enabled: bool) -> Result<()> {
        SqliteQueue::set_auto_update(self, token, enabled)
    }
    fn record_binary_update(
        &self,
        kind: &str,
        commit: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<i64> {
        SqliteQueue::record_binary_update(self, kind, commit, payload)
    }
    fn binary_updates(&self, limit: usize) -> Result<Vec<crate::domain::BinaryUpdate>> {
        SqliteQueue::binary_updates(self, limit)
    }
    fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &str,
        token: &str,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::adopt_run(self, id, previous_token, token, pid, wrapper)
    }
    fn holds_lease(&self, id: &RunId, token: &str) -> Result<bool> {
        SqliteQueue::holds_lease(self, id, token)
    }
    fn abandon_run(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        SqliteQueue::abandon_run(self, id, token, message, reason)
    }
    fn run_leases(&self) -> Result<Vec<RunLease>> {
        SqliteQueue::run_leases(self)
    }
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        SqliteQueue::run_lease(self, id)
    }
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::active_runs(self)
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::all_runs(self)
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::all_events(self)
    }
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        SqliteQueue::task_goals(self)
    }
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        SqliteQueue::rebind_repository(self, common_dir)
    }
    fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        report: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::recover_run(self, id, checked_processes, report)
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        SqliteQueue::run(self, id)
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_with_status(self, status)
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        SqliteQueue::next_awaiting_integration(self)
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        SqliteQueue::run_events(self, id)
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        SqliteQueue::has_run_event(self, id, kind)
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_runtime_event(self, id, kind, payload)
    }
    fn begin_integration(&mut self, id: &RunId, token: &str, main: &CommitSha) -> Result<TaskRun> {
        SqliteQueue::begin_integration(self, id, token, main)
    }
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::defer_integration(self, id, token, reason, detail)
    }
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &str,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::fail_integration(self, id, token, reason, receipt)
    }
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &str,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        SqliteQueue::abort_integration(self, id, token, revert_to, message, reason)
    }
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &str,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        SqliteQueue::finish_integration(self, id, token, landing, common_dir)
    }
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        SqliteQueue::record_cleanup_failure(self, id, message, reason)
    }
    fn workspace_closed(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        SqliteQueue::workspace_closed(self, id, token)
    }
    fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        SqliteQueue::cleanup_failed(self, id, token, message, reason)
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        SqliteQueue::repository_binding(self)
    }
    fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        SqliteQueue::bind_repository(self, common_dir)
    }
    fn assert_repository(&self, common_dir: &str) -> Result<()> {
        SqliteQueue::assert_repository(self, common_dir)
    }
    fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &str,
        order: &[TaskId],
    ) -> Result<ClaimOutcome> {
        SqliteQueue::claim_for_supervisor_in_order(self, base_commit, token, order)
    }
    fn heartbeat(&mut self, token: &str) -> Result<usize> {
        SqliteQueue::heartbeat(self, token)
    }
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        SqliteQueue::processes(self, id)
    }
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        SqliteQueue::record_runtime_error(self, id, message, reason)
    }
    fn plan_run(&mut self, id: &RunId, token: &str, plan: &crate::domain::RunPlan) -> Result<()> {
        SqliteQueue::plan_run(self, id, token, plan)
    }
    fn workspace_created(&mut self, id: &RunId, token: &str, workspace: &str) -> Result<()> {
        SqliteQueue::workspace_created(self, id, token, workspace)
    }
    fn finish_supervision(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        SqliteQueue::finish_supervision(self, id, token)
    }
    fn finish_supervision_live(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        SqliteQueue::finish_supervision_live(self, id, token)
    }
    fn finish_validation(
        &mut self,
        id: &RunId,
        token: &str,
        validation: &Validation,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_validation(self, id, token, validation)
    }
    fn restart_validation(&mut self, id: &RunId, token: &str) -> Result<TaskRun> {
        SqliteQueue::restart_validation(self, id, token)
    }
    fn decide_landing(
        &mut self,
        id: &RunId,
        status: RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::decide_landing(self, id, status, reason, payload)
    }
    fn runs_to_triage(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_to_triage(self)
    }
    fn begin_triage(&mut self, id: &RunId, token: &str) -> Result<Option<(TaskRun, usize)>> {
        SqliteQueue::begin_triage(self, id, token)
    }
    fn finish_triage(
        &mut self,
        id: &RunId,
        token: &str,
        action: &TriageAction,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_triage(self, id, token, action, payload)
    }
    fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_workspace_closed(self, id, workspace_id, payload)
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        SqliteQueue::ended_run_workspaces(self)
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        SqliteQueue::ended_run_worktrees(self)
    }
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        SqliteQueue::decide_triage(self, id, ask_id, answer, reason)
    }
    fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>> {
        SqliteQueue::runs_needing_session(self)
    }
    fn begin_resume(
        &mut self,
        id: &RunId,
        token: &str,
        main: &CommitSha,
        reason: Option<&str>,
    ) -> Result<Option<(TaskRun, usize)>> {
        SqliteQueue::begin_resume(self, id, token, main, reason)
    }
    fn finish_resume(
        &mut self,
        id: &RunId,
        token: &str,
        status: Option<RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_resume(self, id, token, status, reason, keep_lease, payload)
    }
    fn skip_resume(
        &mut self,
        id: &RunId,
        token: &str,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::skip_resume(self, id, token, head, main, approved)
    }
    fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::exhaust_resumes(self, id, exhaustion, reason)
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        SqliteQueue::last_observe(self, mode)
    }
    fn session_workspace(&self, role: SessionRole) -> Result<Option<String>> {
        SqliteQueue::session_workspace(self, role)
    }
    fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()> {
        SqliteQueue::register_session_workspace(self, role, workspace_id)
    }
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        SqliteQueue::remove_session_workspace(self, role)
    }
    fn forget_retired_session_workspaces(&self) -> Result<usize> {
        SqliteQueue::forget_retired_session_workspaces(self)
    }
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        SqliteQueue::open_planner(self, origin, proposal)
    }
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        SqliteQueue::planner_workspace_created(self, id, workspace_id)
    }
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        SqliteQueue::close_planner(self, id, error)
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        SqliteQueue::planner(self, id)
    }
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        SqliteQueue::planners(self, all)
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        SqliteQueue::register_planner_wrapper(self, id, pid)
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        SqliteQueue::register_planner_agent(self, id, wrapper_pid, agent)
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        SqliteQueue::heartbeat_planner(self, id, wrapper_pid)
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        SqliteQueue::planner_exited(self, id, wrapper_pid, exit_code)
    }
    fn set_supervisor_mode(
        &self,
        token: &str,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        SqliteQueue::set_supervisor_mode(self, token, mode, workspace_id)
    }
    fn latest_event_id(&self) -> Result<EventId> {
        SqliteQueue::latest_event_id(self)
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::latest_runs_in_progress(self)
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_with_pending_push(self)
    }
    fn register_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
        SqliteQueue::register_wrapper(self, id, token, pid)
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
        SqliteQueue::register_resume_wrapper(self, id, token, pid)
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        SqliteQueue::register_agent(self, id, wrapper_pid, agent_pid)
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        SqliteQueue::register_resume_agent(self, id, wrapper_pid, agent_pid)
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        SqliteQueue::heartbeat_wrapper(self, id, pid)
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        SqliteQueue::wrapper_exited(self, id, pid, exit_code)
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        SqliteQueue::run_in_workspace(self, workspace_id)
    }
    fn backend_slots(&self, token: Option<&str>) -> Result<(i64, Option<i64>)> {
        SqliteQueue::backend_slots(self, token)
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_backend_failure(self, run, payload)
    }
    fn record_queue_event(&self, kind: &str, payload: serde_json::Value) -> Result<EventId> {
        SqliteQueue::record_queue_event(self, kind, payload)
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        SqliteQueue::latest_queue_event(self, kinds)
    }
}

impl AskStore for SqliteQueue {
    fn open_update_ask(
        &mut self,
        subject: &str,
        question: &str,
        options: &[&str],
        asked_by: &str,
    ) -> Result<crate::domain::Ask> {
        SqliteQueue::open_update_ask(self, subject, question, options, asked_by)
    }
    fn update_answers(&self, subject: &str) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::update_answers(self, subject)
    }
    fn asks(&self, query: crate::application::AskQuery) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::asks(self, query)
    }
    fn has_unclosed_ask(&self, run_id: &RunId, kind: crate::domain::AskKind) -> Result<bool> {
        SqliteQueue::has_unclosed_ask(self, run_id, kind)
    }
    fn ask(&mut self, ask: crate::domain::NewAsk) -> Result<crate::domain::AskOutcome> {
        SqliteQueue::ask(self, ask)
    }
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome> {
        SqliteQueue::hold(self, hold)
    }
    fn hold_of(&self, run_id: &RunId) -> Result<Option<crate::domain::Ask>> {
        SqliteQueue::hold_of(self, run_id)
    }
    fn read_ask(&self, id: AskId) -> Result<crate::domain::Ask> {
        SqliteQueue::read_ask(self, id)
    }
    fn answer(&mut self, id: AskId, text: &str) -> Result<crate::domain::Ask> {
        SqliteQueue::answer(self, id, text)
    }
    fn close_ask(&mut self, id: AskId) -> Result<crate::domain::Ask> {
        SqliteQueue::close_ask(self, id)
    }
    fn landing_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::landing_answers(self)
    }
    fn triage_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::triage_answers(self)
    }
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::undelivered_answers(self, run_id)
    }
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<crate::domain::Ask> {
        SqliteQueue::ask_delivered(self, id, workspace_id)
    }
    fn has_stuck_exit_ask(&self, run_id: &RunId) -> Result<bool> {
        SqliteQueue::has_stuck_exit_ask(self, run_id)
    }
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        SqliteQueue::has_unclosed_worker_question(self, run_id)
    }
    fn close_stuck_exit_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_stuck_exit_asks(self, run_id, answer)
    }
    fn close_answer_prompt_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_answer_prompt_asks(self, run_id, answer)
    }
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<crate::domain::Ask>> {
        SqliteQueue::unclosed_stalled_ask(self, run_id)
    }
    fn close_stalled_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_stalled_asks(self, run_id, answer)
    }
    fn close_approve_landing_asks(
        &mut self,
        run_id: &RunId,
        answer: &str,
    ) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::close_approve_landing_asks(self, run_id, answer)
    }
}

/// Opens the queue at `db` with `generators` for each connection the
/// supervisor needs.
pub struct SqliteOpener {
    pub db: std::path::PathBuf,
    pub generators: crate::application::Generators,
}

impl crate::application::QueueOpener for SqliteOpener {
    fn open(&self) -> Result<Box<dyn crate::application::Queue + Send>> {
        Ok(Box::new(
            SqliteQueue::open(&self.db)?.with_generators(self.generators.clone()),
        ))
    }
}
