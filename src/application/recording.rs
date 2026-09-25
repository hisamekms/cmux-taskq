//! Every failed cmux call on record (task 109): [`RecordingBackend`] wraps
//! a [`WorkspaceBackend`] and writes `backend_call_failed` with the load
//! the call failed under. A call that timed out is made again after a
//! backoff when making it again is safe (task 326).

use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, thread, time::Duration};

use super::{QueueOpener, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags};
use crate::domain::{Reason, ReasonCode, RunId, Task, TaskRun};

/// `backend_call_failed` keeps this many leading characters of the error.
pub const BACKEND_ERROR_CHARS: usize = 300;

/// Which attempt of a call failed: `number` of at most `of`, and the
/// backoff before the next one, `None` when the call is not made again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub number: u32,
    pub of: u32,
    pub retry_after: Option<Duration>,
}

impl Attempt {
    /// The only attempt of a call that is never made again.
    pub const ONLY: Self = Self {
        number: 1,
        of: 1,
        retry_after: None,
    };
}

/// The payload of `backend_call_failed`: the call (`op`, the workspace it
/// was for, the backend's per-call timeout), its error cut to
/// [`BACKEND_ERROR_CHARS`] characters, the load it failed under — the
/// 1-minute load average (null when unavailable), the slots held and the
/// `parallel` offered (null without a supervisor) — and which attempt it
/// was (`attempt` of `max_attempts`, with `retry_after_ms`, the backoff
/// before the next one, null when there is none; task 326). `code` is
/// `backend_timeout` or `backend_failed` (ADR-0034).
#[allow(clippy::too_many_arguments)]
pub fn backend_failure_payload(
    op: &str,
    workspace_id: Option<&str>,
    timeout: Duration,
    error: &str,
    load_avg: Option<f64>,
    slots: i64,
    parallel: Option<i64>,
    attempt: Attempt,
) -> Value {
    json!({
        "code": ReasonCode::of_backend_error(error),
        "op": op,
        "workspace_id": workspace_id,
        "timeout_secs": timeout.as_secs(),
        "error": error.chars().take(BACKEND_ERROR_CHARS).collect::<String>(),
        "load_avg": load_avg,
        "slots": slots,
        "parallel": parallel,
        "attempt": attempt.number,
        "max_attempts": attempt.of,
        "retry_after_ms": attempt.retry_after.map(|backoff| backoff.as_millis() as u64),
    })
}

/// Whether the last [`TEXT_TAIL_LINES`] lines of `screen` show any trace
/// of `text`: the head of its first non-blank line as it is typed (tabs as
/// spaces, up to a backslash), or the `[Pasted text` Claude Code folds a
/// long paste into. Only the last lines are read, so that an earlier copy
/// of the same text in the scrollback does not count. A text with no head
/// to look for counts as shown, so that it is never typed twice on a guess.
pub fn text_on_screen(screen: &str, text: &str) -> bool {
    let head: String = text
        .split(['\n', '\r'])
        .map(|line| line.replace('\t', " "))
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .chars()
        .take_while(|c| *c != '\\')
        .take(TEXT_HEAD_CHARS)
        .collect();
    let head = head.trim_end();
    let lines: Vec<&str> = screen.lines().collect();
    let tail = lines[lines.len().saturating_sub(TEXT_TAIL_LINES)..].join("\n");
    head.is_empty() || tail.contains(head) || tail.contains("[Pasted text")
}

/// [`text_on_screen`] looks for this many leading characters of a text,
/// few enough to fit on the first line it wraps to.
const TEXT_HEAD_CHARS: usize = 24;

/// [`text_on_screen`] reads this many last lines of the screen: the input
/// box and what was submitted last.
const TEXT_TAIL_LINES: usize = 30;

/// A failed backend call, handed back by [`RecordingBackend`] so that
/// whoever records the error later can tell a cmux failure from others
/// ([`reason_of_error`]). It prints exactly as the error it wraps, with or
/// without `{:#}`, and its sources are that error's.
#[derive(Debug)]
pub struct BackendFailure {
    pub op: String,
    /// The failed call is known to have left nothing behind (a read, or a
    /// text the screen shows no trace of), so it could be made again.
    pub effect_free: bool,
    error: anyhow::Error,
}

impl BackendFailure {
    fn wrap(op: &str, effect_free: bool, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self {
            op: op.to_owned(),
            effect_free,
            error,
        })
    }

    fn timed_out(&self) -> bool {
        ReasonCode::of_backend_error(&format!("{:#}", self.error)) == ReasonCode::BackendTimeout
    }

    /// `backend_timeout` or `backend_failed`, with the call's `op`.
    pub fn reason(&self) -> Reason {
        Reason::new(ReasonCode::of_backend_error(&format!("{:#}", self.error)))
            .with("op", self.op.as_str())
    }
}

impl std::fmt::Display for BackendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for BackendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// The reason code of an error a run step failed with: that of a failed
/// cmux call anywhere in its chain, else `fallback`.
pub fn reason_of_error(error: &anyhow::Error, fallback: ReasonCode) -> Reason {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendFailure>())
        .map_or_else(|| Reason::new(fallback), BackendFailure::reason)
}

/// Whether `error` is a cmux call that timed out without it being known
/// whether it took effect: a send that may have reached the session, whose
/// screen then tells whether it did (task 285), rather than be sent again.
pub fn timed_out_maybe_sent(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendFailure>())
        .is_some_and(|failure| failure.timed_out() && !failure.effect_free)
}

fn timed_out(error: &anyhow::Error) -> bool {
    ReasonCode::of_backend_error(&format!("{error:#}")) == ReasonCode::BackendTimeout
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

    /// A call that is made once.
    fn recorded<T>(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        result: Result<T>,
    ) -> Result<T> {
        result.map_err(|error| {
            let _ = self.record(
                op,
                workspace_id,
                run_id,
                &format!("{error:#}"),
                Attempt::ONLY,
            );
            BackendFailure::wrap(op, false, error)
        })
    }

    /// `call`, made again after a backoff (doubled each time) up to the
    /// backend's `call_attempts` in all while it fails with a timeout that
    /// `effect_free` says left nothing behind. Every failed attempt is
    /// recorded with its number and the backoff that follows it.
    fn retried<T>(
        &self,
        op: &str,
        workspace_id: &str,
        mut call: impl FnMut() -> Result<T>,
        effect_free: impl Fn() -> bool,
    ) -> Result<T> {
        let attempts = self.inner.call_attempts().max(1);
        let mut backoff = self.inner.retry_backoff();
        let mut number = 1;
        loop {
            let error = match call() {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let effect_free = timed_out(&error) && effect_free();
            let retry = effect_free && number < attempts;
            let attempt = Attempt {
                number,
                of: attempts,
                retry_after: retry.then_some(backoff),
            };
            let _ = self.record(op, Some(workspace_id), None, &format!("{error:#}"), attempt);
            if !retry {
                return Err(BackendFailure::wrap(op, effect_free, error));
            }
            thread::sleep(backoff);
            backoff = backoff.saturating_mul(2);
            number += 1;
        }
    }

    fn record(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        error: &str,
        attempt: Attempt,
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
                attempt,
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
    /// A text that timed out is typed again only while the screen shows no
    /// trace of it: one that got there is left to the submit check (task
    /// 285), and one whose screen cannot be read is not guessed at.
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        self.retried(
            "send_text",
            workspace_id,
            || self.inner.send_text(workspace_id, text),
            // One read: a screen that does not answer at once is not
            // waited for, and the text is not typed again.
            || {
                let screen = self.inner.capture(workspace_id);
                self.recorded("capture", Some(workspace_id), None, screen)
                    .is_ok_and(|screen| !text_on_screen(&screen, text))
            },
        )
    }
    fn send_enter(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_enter(workspace_id);
        self.recorded("send_enter", Some(workspace_id), None, result)
    }
    fn capture(&self, workspace_id: &str) -> Result<String> {
        self.retried(
            "capture",
            workspace_id,
            || self.inner.capture(workspace_id),
            || true,
        )
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.close(workspace_id);
        self.recorded("close", Some(workspace_id), None, result)
    }
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        let result = self.inner.set_color(workspace_id, color);
        self.recorded("set_color", Some(workspace_id), None, result)
    }
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        let result = self.inner.set_status(workspace_id, key, value, icon);
        self.recorded("set_status", Some(workspace_id), None, result)
    }
    fn pin(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.pin(workspace_id);
        self.recorded("pin", Some(workspace_id), None, result)
    }
    /// Never made again: a second `/exit` could pick a dialog's option.
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_exit(workspace_id);
        self.recorded("send_exit", Some(workspace_id), None, result)
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        self.retried(
            "exists",
            workspace_id,
            || self.inner.exists(workspace_id),
            || true,
        )
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        let result = self.inner.listed_workspace_ids();
        self.recorded("listed_workspace_ids", None, None, result)
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
    fn call_attempts(&self) -> u32 {
        self.inner.call_attempts()
    }
    fn retry_backoff(&self) -> Duration {
        self.inner.retry_backoff()
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
    fn submit_check_interval(&self) -> Duration {
        self.inner.submit_check_interval()
    }
    fn start_wait(&self) -> Duration {
        self.inner.start_wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn a_backend_failure_prints_as_the_error_it_wraps_and_classifies_it() {
        let inner = || anyhow!("Command timed out").context("cmux capture-pane failed");
        let wrapped = BackendFailure::wrap("capture", true, inner());
        assert_eq!(format!("{wrapped:#}"), format!("{:#}", inner()));
        assert_eq!(format!("{wrapped}"), format!("{}", inner()));
        let outer = Err::<(), _>(wrapped)
            .context("run could not be watched")
            .unwrap_err();
        assert_eq!(
            format!("{outer:#}"),
            "run could not be watched: cmux capture-pane failed: Command timed out"
        );
        let reason = reason_of_error(&outer, ReasonCode::Other);
        assert_eq!(reason.code, ReasonCode::BackendTimeout);
        assert_eq!(reason.detail["op"], "capture");
        let failed = BackendFailure::wrap("close", false, anyhow!("workspace not found"));
        assert_eq!(
            reason_of_error(&failed, ReasonCode::Other).code,
            ReasonCode::BackendFailed
        );
        assert_eq!(
            reason_of_error(&anyhow!("git failed"), ReasonCode::Other),
            Reason::new(ReasonCode::Other)
        );
    }

    #[test]
    fn a_backend_failure_payload_carries_its_code() {
        let payload = backend_failure_payload(
            "send_exit",
            Some("ws"),
            Duration::from_secs(30),
            "\"cmux\" send did not finish within 30s",
            None,
            1,
            Some(4),
            Attempt::ONLY,
        );
        assert_eq!(payload["code"], "backend_timeout");
        assert_eq!(payload["op"], "send_exit");
        assert_eq!(
            (
                &payload["attempt"],
                &payload["max_attempts"],
                &payload["retry_after_ms"]
            ),
            (&json!(1), &json!(1), &Value::Null)
        );
        let retried = Attempt {
            number: 2,
            of: 3,
            retry_after: Some(Duration::from_secs(4)),
        };
        let payload = backend_failure_payload(
            "capture",
            Some("ws"),
            Duration::from_secs(30),
            "Command timed out",
            Some(91.5),
            4,
            Some(4),
            retried,
        );
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["max_attempts"], 3);
        assert_eq!(payload["retry_after_ms"], 4000);
    }

    #[test]
    fn a_text_is_on_the_screen_by_the_head_of_its_first_line() {
        let text = "answer to ask 12: rebase onto main and run the tests again\nthen commit";
        assert!(text_on_screen("> answer to ask 12: rebase onto ma", text));
        assert!(!text_on_screen("> ready", text));
        // Up to a backslash, which is typed otherwise, and tabs as spaces.
        assert!(text_on_screen("> see C:", "see C:\\path"));
        assert!(text_on_screen("> a b", "\n\na\tb"));
        // A long paste folded in the input box got there.
        assert!(text_on_screen("> [Pasted text #1 +3 lines]", text));
        // A copy far up the scrollback is not this one.
        let old = format!("> {text}\n{}> ready", "work\n".repeat(40));
        assert!(!text_on_screen(&old, text));
        // Nothing to look for is never typed again on a guess.
        assert!(text_on_screen("", "  "));
    }
}
