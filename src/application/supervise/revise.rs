//! A `revise` verdict or a conflict sent to the live session
//! ([`ReviseWatch`], ADR-0027 decision 2).

use super::*;

/// A `revise` verdict, or a conflict the precheck found, sent to the live
/// session: it is waited for until the session rewrites its receipt and
/// goes idle.
pub(super) struct ReviseWatch {
    pub(super) session: SessionRef,
    pub(super) attempt: usize,
    pub(super) fix: Fix,
    /// A receipt or idle marker no newer than this predates the request.
    pub(super) sent_at: SystemTime,
    pub(super) sent: Instant,
    /// Whether the session took the request, or the last answer typed
    /// (task 285).
    pub(super) start: Option<StartCheck>,
    /// The answers of the session's `worker_question`s and the dialogs it
    /// stops at, followed as a worker's own session's are (task 238).
    pub(super) live: Box<SessionWatch>,
}

/// What the live session was asked to fix (ADR-0027 decisions 2 and 4).
pub(super) enum Fix {
    /// A `revise` verdict's findings.
    Revise(Vec<String>),
    /// A conflict with main found after a `pass`; the passed verdict, for
    /// the ask if the session does not resolve it.
    Conflict(ReviewVerdict),
}

impl Fix {
    /// How the request is named in logs and texts: `revise N` or
    /// `conflict request N`.
    pub(super) fn label(&self, attempt: usize) -> String {
        match self {
            Fix::Revise(_) => format!("revise {attempt}"),
            Fix::Conflict(_) => format!("conflict request {attempt}"),
        }
    }

    /// The `approve_landing` ask when the session cannot fix it: a revise
    /// asks with `summary`; a conflict asks with its passed verdict.
    pub(super) fn ask(&self, summary: String, why: String) -> AfterExit {
        match self {
            Fix::Revise(reasons) => AfterExit::Ask {
                decision: ReviewDecision::Revise,
                reasons: reasons.clone(),
                summary,
                why: Some(why),
            },
            Fix::Conflict(verdict) => AfterExit::Ask {
                decision: verdict.verdict,
                reasons: verdict.reasons.clone(),
                summary: verdict.summary.clone(),
                why: Some(why),
            },
        }
    }
}

pub(super) enum ReviseOutcome {
    /// The receipt was rewritten after the request, names the clean
    /// worktree HEAD (or reports `failed`), and the session went idle after
    /// it; the worktree HEAD at that time. Validation judges the receipt.
    Rewritten(CommitSha),
    /// The session rewrote the receipt and went idle, but the receipt does
    /// not name the clean worktree HEAD (an old commit, a commit after the
    /// receipt, uncommitted changes) or cannot be read: validation would
    /// fail the run and its work, so the session is asked to fix it.
    Mismatch(ReasonCode, String),
    /// The session will not rewrite it: why.
    Ended(String),
}

impl ReviseWatch {
    pub(super) fn new(
        run: &TaskRun,
        session: SessionRef,
        attempt: usize,
        fix: Fix,
        sent_at: SystemTime,
        start: Option<StartCheck>,
    ) -> Result<Self> {
        let live = Box::new(SessionWatch::fixing(run, &session.workspace, sent_at)?);
        Ok(ReviseWatch {
            session,
            attempt,
            fix,
            sent_at,
            sent: Instant::now(),
            start,
            live,
        })
    }

    /// Another request typed at `sent_at` (a receipt to fix): only what the
    /// session does after it counts.
    pub(super) fn requested(&mut self, sent_at: SystemTime, start: StartCheck) {
        self.sent_at = sent_at;
        self.live.input_at = Some(sent_at);
        self.start = Some(start);
    }

    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<ReviseOutcome>> {
        let outcome = self.observe(sv, run)?;
        if matches!(
            outcome,
            Some(ReviseOutcome::Rewritten(_) | ReviseOutcome::Ended(_))
        ) {
            // The revise ends here: a dialog recorded during it is no
            // attention any more.
            self.live.clear_prompt(sv, run)?;
            self.live.recovery.stop(sv, run);
        }
        Ok(outcome)
    }

    fn observe(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<ReviseOutcome>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes
            .iter()
            .find(|p| p.role == "wrapper" && p.exited_at.is_none())
        else {
            return Ok(Some(ReviseOutcome::Ended(
                "ended before it rewrote the receipt".to_owned(),
            )));
        };
        if sv.generators.clock.now() - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS {
            // A session that died will not rewrite it either (task 236).
            if !sv.processes.alive(wrapper.pid) {
                return Ok(Some(ReviseOutcome::Ended(
                    "died without recording its exit".to_owned(),
                )));
            }
            // The exit that follows records `wrapper_heartbeat_expired` and
            // sends the /exit.
            return Ok(Some(ReviseOutcome::Ended(
                "went silent (its wrapper stopped heartbeating while its process lives on)"
                    .to_owned(),
            )));
        }
        // A silence its wait recorded ended before the revise did: the
        // session may wait again, and a later silence is recorded again
        // (task 606).
        self.live.silent = false;
        // An answer to a question the session asked during the revise is
        // typed once it went idle at it: the session works again, and gets
        // the resume timeout again (task 238).
        if let Some(typed) = self.live.deliver_answers(sv, run)? {
            self.live.input_at = Some(typed);
            self.sent = Instant::now();
            self.start = self.live.answer_start.take();
        }
        if let Some(agent) = processes
            .iter()
            .find(|p| p.role == "agent" && p.exited_at.is_none())
        {
            self.live.watch_prompt(sv, run, agent)?;
        }
        if let Some(start) = &mut self.start {
            let workspace = self.session.workspace.clone();
            start.poll(sv, run, &workspace, &run.idle_marker_path()?)?;
        }
        // A session stopped at its own question waits for its answer, however
        // long a person takes: it neither went idle without rewriting the
        // receipt nor ran out of time.
        if sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(None);
        }
        // An answer delivered by hand (or by the supervisor this one
        // adopted the run from) is input too: the idle marker of the stop at
        // the question is older than it. Its close is known to the second: a
        // close in a later second than the last input moves it there (one
        // this watch typed closes in the second it was typed).
        let input_at = self.live.input_at.unwrap_or(self.sent_at);
        if let Some(closed) = sv.queue.last_worker_question_closed(run.id())?
            && closed > unix_seconds(input_at)
        {
            self.live.input_at = Some(UNIX_EPOCH + Duration::from_secs(closed.max(0) as u64));
            self.sent = Instant::now();
        }
        let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
        // The idle marker is read before the receipt: a receipt rewritten
        // after this read is judged at the next poll, never as idle without
        // it. A marker from before the last input typed is not this turn's.
        let input_at = self.live.input_at.unwrap_or(self.sent_at);
        let idle = IdleMarker::read(&*sv.files, sv.signals, &run.idle_marker_path()?)?
            .filter(|idle| idle.modified() > input_at);
        let rewritten = sv
            .files
            .modified(receipt)
            .is_ok_and(|modified| modified > self.sent_at);
        let idle_after_receipt = match &idle {
            Some(idle) if rewritten => idle.idle_after_receipt(&*sv.files, receipt)?.is_some(),
            _ => false,
        };
        if idle_after_receipt {
            let worktree = Path::new(run.worktree_path().context("missing worktree")?);
            let head = sv.repository.head(worktree)?;
            let clean = sv.repository.status(worktree)?.trim().is_empty();
            let parsed = sv
                .files
                .read_to_string(receipt)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(Receipt::parse(&text)?));
            return Ok(Some(match parsed {
                // A session that gives the change up is validation's to fail.
                Ok(receipt) if receipt.result == ReceiptResult::Failed => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if receipt.commit.to_ascii_lowercase() == head.as_str() && clean => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if receipt.commit.to_ascii_lowercase() != head.as_str() => {
                    ReviseOutcome::Mismatch(
                        ReasonCode::CommitMismatch,
                        format!(
                            "the rewritten receipt names commit {} but the worktree HEAD is {head}",
                            receipt.commit
                        ),
                    )
                }
                Ok(_) => ReviseOutcome::Mismatch(
                    ReasonCode::WorktreeDirty,
                    format!("the worktree has uncommitted changes on top of HEAD {head}"),
                ),
                Err(error) => ReviseOutcome::Mismatch(
                    ReasonCode::ReceiptInvalid,
                    format!("the rewritten receipt is invalid: {error:#}"),
                ),
            }));
        }
        if !rewritten && idle.is_some_and(|idle| idle.idle_since(input_at)) {
            return Ok(Some(ReviseOutcome::Ended(
                "went idle without rewriting the receipt".to_owned(),
            )));
        }
        if self.sent.elapsed() >= sv.cmux.resume_timeout() {
            return Ok(Some(ReviseOutcome::Ended(format!(
                "did not rewrite the receipt within {} seconds",
                sv.cmux.resume_timeout().as_secs()
            ))));
        }
        Ok(None)
    }
}
