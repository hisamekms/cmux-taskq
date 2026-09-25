//! Follow-up triage (ADR-0037): apply a job's verdict about a follow_up
//! draft and tell the inbox of the `follow_up` ask it may open. The store
//! applies the verdict in one transaction; the notification goes out after
//! it, as for every ask (ADR-0022 decision 5).

use anyhow::Result;
use std::path::Path;
use tracing::warn;

use super::{FollowUpApplied, FollowUpJob, Queue, WorkspaceBackend, ask};
use crate::domain::{FollowUpVerdict, TaskId};

/// Apply `verdict` to `draft` under the lease `token` holds and, when it
/// opened a new `follow_up` ask, notify the inbox. `Ok(None)` means the
/// lease was lost and nothing was applied. A failed notification (even an
/// error reading the inbox's workspace) leaves the ask standing and is only
/// reported.
pub fn finish(
    queue: &mut dyn Queue,
    checkout: &Path,
    cmux: &dyn WorkspaceBackend,
    draft: TaskId,
    token: &str,
    job: &FollowUpJob,
    verdict: &FollowUpVerdict,
) -> Result<Option<FollowUpApplied>> {
    let Some(applied) = queue.finish_follow_up_triage(draft, token, job, verdict)? else {
        return Ok(None);
    };
    if let Some(outcome) = &applied.ask {
        // The verdict is applied: a notification that fails is only
        // reported, never a failed job.
        let error = match ask::notify(queue, checkout, outcome, cmux) {
            Ok(notified) => notified.get("notify_error").map(ToString::to_string),
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(error) = error {
            warn!(
                op = "follow_up",
                task_id = %draft,
                ask_id = %outcome.ask.id,
                error = %error,
                "task {draft}: the inbox was not notified of ask {}: {error}",
                outcome.ask.id
            );
        }
    }
    Ok(Some(applied))
}
