//! `rebind` (ADR-0020): bind the queue to the repository it moved to, the
//! only way the binding changes.

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;

use super::{Clock, ProcessControl, Repository, RunFiles, RunStore};
use crate::domain::RunStatus;

/// Append-only record of `rebind` under the queue's `logs/`, one JSON
/// object per changed binding; the schema has no queue-level event.
pub const REBIND_LOG: &str = "rebind.jsonl";

/// What `rebind` reads and changes through.
pub struct Rebind<'a> {
    pub queue: &'a mut dyn RunStore,
    /// The repository the queue is bound to from now on.
    pub repository: &'a dyn Repository,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
}

/// Where the queue and the new repository are.
pub struct RebindTarget {
    /// The queue database, canonical.
    pub db: PathBuf,
    /// The Git common directory of the new repository.
    pub common_dir: String,
    pub queue_dir: PathBuf,
    pub log_dir: PathBuf,
    /// The queue directory's `repository` file, rewritten when it exists.
    pub repository_file: PathBuf,
    /// Where a repository-resolved queue of the new repository lives;
    /// `None` without a data home.
    pub repository_queue_dir: Option<PathBuf>,
}

/// Bind the queue to the repository at `target.common_dir` after the
/// repository moved. Refused while a registered supervisor or an
/// `integrate` still lives, since both hold paths of the old repository.
/// The change is appended to `logs/rebind.jsonl`, the queue directory's
/// `repository` file (if any) is rewritten, and every run worktree still on
/// disk gets its Git link repaired from the new repository. Reports where a
/// repository-resolved queue now lives, which differs from the queue's own
/// directory until it is moved there.
pub fn rebind(ports: Rebind<'_>, target: RebindTarget) -> Result<Value> {
    let Rebind {
        queue,
        repository,
        files,
        processes,
        clock,
    } = ports;
    let common_dir = target.common_dir;
    let live = queue
        .supervisors()?
        .into_iter()
        .filter(|registration| processes.alive(registration.pid))
        .map(|registration| registration.pid)
        .collect::<Vec<_>>();
    ensure!(
        live.is_empty(),
        "refusing to rebind while a supervisor is running (pid {live:?}); stop it with `down --wait` first"
    );
    let leases = queue.run_leases()?;
    if let Some(run) = queue
        .runs_with_status(RunStatus::Integrating)?
        .into_iter()
        .find(|run| {
            leases
                .iter()
                .any(|lease| lease.run_id == *run.id() && processes.alive(lease.pid))
        })
    {
        bail!(
            "refusing to rebind while run {} of task {} is integrating",
            run.id(),
            run.task_id()
        );
    }
    let previous = queue.rebind_repository(&common_dir)?;
    let changed = previous.as_deref() != Some(common_dir.as_str());
    if changed {
        files.create_dir_all(&target.log_dir)?;
        files.append_line(
            &target.log_dir.join(REBIND_LOG),
            &json!({
                "at": clock.now(),
                "previous_git_common_dir": previous,
                "git_common_dir": common_dir,
                "binary_version": crate::VERSION,
            })
            .to_string(),
        )?;
        if files.is_file(&target.repository_file) {
            files.write(
                &target.repository_file,
                format!("{common_dir}\n").as_bytes(),
            )?;
        }
    }
    let worktrees = queue
        .all_runs()?
        .into_iter()
        .filter_map(|run| {
            let path = PathBuf::from(run.worktree_path()?);
            Some((run.id().clone(), path))
        })
        .filter(|(_, path)| files.is_dir(path))
        .map(|(run_id, path)| {
            let error = repository.repair_worktree(&path).err();
            json!({
                "run_id": run_id,
                "worktree_path": path,
                "repaired": error.is_none(),
                "error": error.map(|e| format!("{e:#}")),
            })
        })
        .collect::<Vec<_>>();
    let resolved = target.repository_queue_dir;
    let move_to = resolved
        .clone()
        .filter(|dir| files.canonicalize(dir).ok().as_deref() != Some(target.queue_dir.as_path()));
    Ok(json!({
        "outcome": if changed { "rebound" } else { "unchanged" },
        "db": target.db,
        "previous_git_common_dir": previous,
        "git_common_dir": common_dir,
        "queue_dir": target.queue_dir,
        "repository_queue_dir": resolved,
        "move_to": move_to,
        "worktrees": worktrees,
    }))
}
