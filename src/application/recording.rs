//! Every failed cmux call on record (task 109): [`RecordingBackend`] wraps
//! a [`WorkspaceBackend`] and writes `backend_call_failed` with the load
//! the call failed under.

use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

use super::{QueueOpener, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags};
use crate::domain::{RunId, Task, TaskRun};

/// `backend_call_failed` keeps this many leading characters of the error.
pub const BACKEND_ERROR_CHARS: usize = 300;

/// The payload of `backend_call_failed`: the call (`op`, the workspace it
/// was for, the backend's per-call timeout), its error cut to
/// [`BACKEND_ERROR_CHARS`] characters, and the load it failed under — the
/// 1-minute load average (null when unavailable), the slots held and the
/// `parallel` offered (null without a supervisor).
pub fn backend_failure_payload(
    op: &str,
    workspace_id: Option<&str>,
    timeout: Duration,
    error: &str,
    load_avg: Option<f64>,
    slots: i64,
    parallel: Option<i64>,
) -> Value {
    json!({
        "op": op,
        "workspace_id": workspace_id,
        "timeout_secs": timeout.as_secs(),
        "error": error.chars().take(BACKEND_ERROR_CHARS).collect::<String>(),
        "load_avg": load_avg,
        "slots": slots,
        "parallel": parallel,
    })
}

/// A [`WorkspaceBackend`] that records every failed or timed-out call as
/// `backend_call_failed` before handing the error back unchanged, so the
/// queue keeps how often cmux fails and under what load (task 109). The
/// record is made here, in the application layer, and not in the cmux
/// adapter (ADR-0013). A call made for a run (`create`, or any call on a
/// workspace a run opened) is recorded on that run; one that belongs to no
/// run (`up`'s workspaces, the queue's group, `down`'s close) without one.
/// `token` is the supervisor whose slots are reported; `None` (`up`,
/// `down`) reports every lease and supervisor. The record is written
/// through its own connection, and a record that cannot be written is
/// dropped: it must never hide the backend's error.
pub struct RecordingBackend<'a> {
    inner: &'a dyn WorkspaceBackend,
    queues: Arc<dyn QueueOpener>,
    token: Option<String>,
    /// The 1-minute load average, `None` where it cannot be read.
    load_average: fn() -> Option<f64>,
}

impl<'a> RecordingBackend<'a> {
    /// `inner`, recording its failures through a connection `queues`
    /// opens for each.
    pub fn over(
        inner: &'a dyn WorkspaceBackend,
        queues: Arc<dyn QueueOpener>,
        token: Option<String>,
        load_average: fn() -> Option<f64>,
    ) -> Self {
        Self {
            inner,
            queues,
            token,
            load_average,
        }
    }

    fn recorded<T>(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        result: Result<T>,
    ) -> Result<T> {
        if let Err(error) = &result {
            let _ = self.record(op, workspace_id, run_id, &format!("{error:#}"));
        }
        result
    }

    fn record(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        error: &str,
    ) -> Result<()> {
        let queue = self.queues.open()?;
        let run_id = match (run_id, workspace_id) {
            (Some(run_id), _) => Some(run_id.clone()),
            (None, Some(workspace_id)) => queue.run_in_workspace(workspace_id)?,
            (None, None) => None,
        };
        let (slots, parallel) = queue.backend_slots(self.token.as_deref())?;
        queue.record_backend_failure(
            run_id.as_ref(),
            backend_failure_payload(
                op,
                workspace_id,
                self.inner.call_timeout(),
                error,
                (self.load_average)(),
                slots,
                parallel,
            ),
        )
    }
}

impl WorkspaceBackend for RecordingBackend<'_> {
    fn preflight(&self) -> Result<()> {
        self.inner.preflight()
    }
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.inner.preflight_detached(environment)
    }
    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create(task, run, command, tags);
        self.recorded("create", None, Some(run.id()), result)
    }
    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_resume(task, run, command, tags);
        self.recorded("create_resume", None, Some(run.id()), result)
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        let result = self.inner.send_text(workspace_id, text);
        self.recorded("send_text", Some(workspace_id), None, result)
    }
    fn capture(&self, workspace_id: &str) -> Result<String> {
        let result = self.inner.capture(workspace_id);
        self.recorded("capture", Some(workspace_id), None, result)
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.close(workspace_id);
        self.recorded("close", Some(workspace_id), None, result)
    }
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_exit(workspace_id);
        self.recorded("send_exit", Some(workspace_id), None, result)
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        let result = self.inner.exists(workspace_id);
        self.recorded("exists", Some(workspace_id), None, result)
    }
    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_named(name, cwd, command, tags);
        self.recorded("create_named", None, None, result)
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let result = self.inner.ensure_group(external_id, name);
        self.recorded("ensure_group", None, None, result)
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let result = self.inner.notify(title, body, workspace);
        self.recorded("notify", workspace, None, result)
    }
    fn call_timeout(&self) -> Duration {
        self.inner.call_timeout()
    }
    fn exit_timeout(&self) -> Duration {
        self.inner.exit_timeout()
    }
    fn registration_timeout(&self) -> Duration {
        self.inner.registration_timeout()
    }
    fn prompt_wait(&self) -> Duration {
        self.inner.prompt_wait()
    }
    fn resume_prompt_delay(&self) -> Duration {
        self.inner.resume_prompt_delay()
    }
    fn resume_timeout(&self) -> Duration {
        self.inner.resume_timeout()
    }
}
