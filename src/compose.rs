//! The entry points of `review`, `rebind` and `stats`: each opens the queue
//! and the repository, builds the adapters of the ports and calls the use
//! case in `application` (ADR-0013). `runtime` re-exports them under the
//! names the CLI and the tests use.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

use crate::{
    application::{
        Repository,
        rebind::{self as rebinding, Rebind, RebindTarget},
        review::{self as reviewing, Review},
        stats as statistics,
    },
    domain::{TaskId, stats::StatsQuery},
    infrastructure::{
        adapters::{GitRepository, SystemProcesses, path_text},
        location::{QueueLocation, REPOSITORY_FILE_NAME, data_home},
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
};

/// `review`: see [`reviewing::review`]. The run's checkout is opened as a
/// Git repository.
pub fn review(db: &Path, task_id: TaskId) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let open_repository = |checkout: &Path| -> Result<Box<dyn Repository>> {
        Ok(Box::new(GitRepository::inspect(checkout)?))
    };
    reviewing::review(
        Review {
            queue: &mut queue,
            files: &LocalRunFiles,
            open_repository: &open_repository,
            pid: std::process::id(),
        },
        task_id,
    )
}

/// `rebind`: bind the queue at `db` to the repository containing `repo`
/// (see [`rebinding::rebind`]).
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let repository = GitRepository::inspect(repo)?;
    let common_dir = path_text(&repository.common_dir)?;
    let location = QueueLocation::explicit(&db);
    let repository_queue_dir = data_home()
        .ok()
        .map(|home| QueueLocation::for_repository(&repository.common_dir, &home).queue_dir);
    let clock = queue.generators().clock.clone();
    rebinding::rebind(
        Rebind {
            queue: &mut queue,
            repository: &repository,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            clock: &*clock,
        },
        RebindTarget {
            repository_file: location.queue_dir.join(REPOSITORY_FILE_NAME),
            db,
            common_dir,
            queue_dir: location.queue_dir,
            log_dir: location.log_dir,
            repository_queue_dir,
        },
    )
}

/// `stats`: see [`statistics::stats`], measured to the queue clock's now.
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = queue.generators().clock.now();
    Ok(serde_json::to_value(statistics::stats(
        &queue,
        &SystemProcesses,
        now,
        query,
    )?)?)
}
