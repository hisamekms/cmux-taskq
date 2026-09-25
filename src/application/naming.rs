//! How the runtime names and quotes what it hands to cmux and the shell:
//! the startup command of a session, the description and group of a
//! workspace, and the title of an ask's notification. Pure text; the cmux
//! adapter re-exports these under its own path.

use std::path::Path;

use crate::domain::{Ask, RunId, SessionRole, TaskId, TaskRun};

/// Shell boundaries are cmux's terminal startup command and Claude's hook command.
/// Quote every argument independently, including paths containing apostrophes.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

/// `dagq role=<role> queue=<queue hash>[ run=<run-id>][ task=<id>]`: the one
/// machine-readable description line every workspace of a queue carries,
/// for people reading `cmux workspace list`. The runtime finds a workspace
/// by its stable ID, never by this line (ADR-0026); only `stats` reads it
/// back, to tell which open worker workspaces belong to the queue
/// (`workspace_mismatch`, ADR-0043 decision 5).
pub fn workspace_description(
    role: SessionRole,
    queue_hash: &str,
    run: Option<&RunId>,
    task: Option<TaskId>,
) -> String {
    let mut description = format!("dagq role={} queue={queue_hash}", role.as_str());
    if let Some(run) = run {
        description.push_str(&format!(" run={run}"));
    }
    if let Some(task) = task {
        description.push_str(&format!(" task={task}"));
    }
    description
}

/// `[<repo>]`: the name of the workspace group a queue's workspaces join.
pub fn workspace_group_name(repo_root: &Path) -> String {
    format!("[{}]", repository_name(repo_root))
}

/// `run <run-id> resume`: the description of the workspace the supervisor
/// resumes a `needs_session` run's session in; its title is the worker's
/// (`run_workspace_name`, ADR-0028).
pub fn resume_workspace_description(run: &TaskRun) -> String {
    format!("run {} resume", run.id())
}

/// `[<repo>] ask #<id> <kind>`: the title of the notification `ask` sends
/// the inbox for a new ask (ADR-0022 decision 5).
pub fn ask_notification_title(repo_root: &Path, ask: &Ask) -> String {
    format!(
        "[{}] ask #{} {}",
        repository_name(repo_root),
        ask.id,
        ask.kind.as_str()
    )
}

/// `[<repo>]supervisor`: the workspace `up --in-cmux` runs `supervise`
/// in when launchd cannot reach cmux (ADR-0011). The launchd mode has no
/// workspace at all.
pub fn supervisor_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Supervisor)
}

/// `[<repo>]planner`: the session that talks with a person to register goals
/// and tasks, which `up` opens (ADR-0028).
pub fn planner_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Planner)
}

/// `[<repo>]inbox`: the session where a person answers the queue's asks,
/// which `up` opens next to the planner's.
pub fn inbox_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Inbox)
}

fn role_workspace_name(repo_root: &Path, role: SessionRole) -> String {
    format!("[{}]{}", repository_name(repo_root), role.as_str())
}

/// The repository's directory name, or the whole path when it has none.
pub fn repository_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}
