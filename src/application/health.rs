//! How an unfinished run stands, as `doctor` reports it and as `recover`
//! and the supervisor judge it before recovering a run (ADR-0024 decision
//! 3): its lease, its registered processes and its files. Liveness comes
//! through [`ProcessControl`] and the files through [`RunFiles`].

use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

use super::{ProcessControl, RunFiles};
use crate::domain::{HEARTBEAT_TIMEOUT_SECS, RunId, RunProcess, RunStatus, TaskId, TaskRun};

/// Health of one run's lease as `status` and `doctor` report it.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseHealth {
    pub pid: u32,
    pub alive: bool,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
}

/// Health of one registered wrapper/agent process. `alive` is only checked
/// while the wrapper has not reported an exit, because a dead PID may be reused.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessHealth {
    pub role: String,
    pub pid: u32,
    pub alive: Option<bool>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub heartbeat_stale: bool,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// One unfinished run. `blockers` lists why `recover` would refuse it; an
/// empty list means it is recoverable now. Only this run's own lease and
/// processes count; other runs never block it.
#[derive(Debug, Clone, Serialize)]
pub struct RunHealth {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_exists: Option<bool>,
    pub run_dir: Option<String>,
    pub run_dir_exists: Option<bool>,
    pub receipt_exists: Option<bool>,
    pub last_error: Option<String>,
    pub lease: Option<LeaseHealth>,
    pub processes: Vec<ProcessHealth>,
    pub blockers: Vec<String>,
    pub recoverable: bool,
}

impl RunHealth {
    /// The run in `doctor`'s default output: whether it can be recovered and
    /// where it is, with `blockers` counted (`blocker_count`) and the lease
    /// reduced to `lease_stale` (null without a lease).
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "task_id": self.task_id,
            "status": self.status,
            "lease_stale": self.lease.as_ref().map(|lease| lease.stale),
            "recoverable": self.recoverable,
            "blocker_count": self.blockers.len(),
            "workspace_id": self.workspace_id,
            "worktree_path": self.worktree_path,
        })
    }
}

/// The health of `run` at `now`: a live process of the run, a fresh lease
/// or a live lease holder blocks its recovery.
pub fn run_health(
    run: &TaskRun,
    processes: &[RunProcess],
    lease: Option<LeaseHealth>,
    now: i64,
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
) -> RunHealth {
    let mut blockers = Vec::new();
    let processes: Vec<ProcessHealth> = processes
        .iter()
        .map(|process| {
            let age = now - process.heartbeat_at;
            let alive = process
                .exited_at
                .is_none()
                .then(|| control.alive(process.pid));
            if alive == Some(true) {
                blockers.push(format!("{} pid {} is alive", process.role, process.pid));
            }
            ProcessHealth {
                role: process.role.clone(),
                pid: process.pid,
                alive,
                heartbeat_at: process.heartbeat_at,
                heartbeat_age_secs: age,
                heartbeat_stale: process.exited_at.is_none() && age > HEARTBEAT_TIMEOUT_SECS,
                exited_at: process.exited_at,
                exit_code: process.exit_code,
            }
        })
        .collect();
    if let Some(lease) = &lease {
        if !lease.stale {
            blockers.push(format!(
                "lease heartbeat is {}s old (limit {HEARTBEAT_TIMEOUT_SECS}s)",
                lease.heartbeat_age_secs
            ));
        }
        if lease.alive {
            blockers.push(format!("supervisor pid {} is alive", lease.pid));
        }
    }
    let exists = |path: Option<&str>| path.map(|p| files.exists(Path::new(p)));
    RunHealth {
        run_id: run.id().clone(),
        task_id: run.task_id(),
        status: run.status(),
        workspace_id: run.workspace_id().map(str::to_owned),
        worktree_path: run.worktree_path().map(str::to_owned),
        worktree_exists: exists(run.worktree_path()),
        run_dir: run.run_dir().map(str::to_owned),
        run_dir_exists: exists(run.run_dir()),
        receipt_exists: exists(run.receipt_path()),
        last_error: run.last_error().map(str::to_owned),
        lease,
        processes,
        recoverable: blockers.is_empty(),
        blockers,
    }
}
