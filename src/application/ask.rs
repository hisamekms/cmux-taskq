//! `ask`: register a question for a person and, when it is new, tell them
//! with one notification aimed at the inbox workspace `up` recorded
//! (ADR-0022 decision 5). The supervisor's own asks take this path too.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::{Queue, WorkspaceBackend, naming::ask_notification_title};
use crate::domain::{NewAsk, SessionRole};

/// Characters of an ask's question the notification keeps before `…`.
const NOTIFY_QUESTION_CHARS: usize = 200;

/// Register `ask` and, when it is new, notify the inbox (without a
/// workspace when there is none). A repeated ask notifies nobody. The ask
/// stands whether or not the notification goes out; a failure is reported
/// as `notify_error` next to `notified: false`. `checkout` names the
/// repository in the title when the queue is bound to none.
pub fn ask(
    queue: &mut dyn Queue,
    checkout: &Path,
    ask: NewAsk,
    cmux: &dyn WorkspaceBackend,
) -> Result<Value> {
    let outcome = queue.ask(ask)?;
    let mut value = serde_json::to_value(&outcome)?;
    if !outcome.created {
        value["notified"] = json!(false);
        return Ok(value);
    }
    // The main checkout names the repository, as the workspace group does;
    // a queue bound to no repository falls back to the working directory.
    let common_dir = queue.repository_binding()?.map(PathBuf::from);
    let repo_root = match &common_dir {
        Some(dir) if dir.file_name() == Some(".git".as_ref()) => dir.parent().unwrap_or(dir),
        Some(dir) => dir.as_path(),
        None => checkout,
    };
    let ask = &outcome.ask;
    let question = super::health::truncate(&ask.question, NOTIFY_QUESTION_CHARS)
        .unwrap_or_else(|| ask.question.clone());
    // An observer's blocked ask may belong to no task (and then no run).
    let mut body = question;
    if let Some(task_id) = ask.task_id {
        body.push_str(&format!("\ntask {task_id}"));
        if let Some(run_id) = &ask.run_id {
            body.push_str(&format!(" run {run_id}"));
        }
    }
    let inbox = queue.session_workspace(SessionRole::Inbox)?;
    match cmux.notify(
        &ask_notification_title(repo_root, ask),
        &body,
        inbox.as_deref(),
    ) {
        Ok(()) => value["notified"] = json!(true),
        Err(error) => {
            value["notified"] = json!(false);
            value["notify_error"] = json!(format!("{error:#}"));
        }
    }
    Ok(value)
}
