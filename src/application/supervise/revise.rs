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
    Mismatch(String),
    /// The session will not rewrite it: why.
    Ended(String),
}

impl ReviseWatch {
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<ReviseOutcome>> {
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
            ensure!(
                sv.processes.alive(wrapper.pid),
                "wrapper heartbeat expired; session may still be alive"
            );
            // The exit that follows records `wrapper_heartbeat_expired` and
            // sends the /exit.
            return Ok(Some(ReviseOutcome::Ended(
                "went silent (its wrapper stopped heartbeating while its process lives on)"
                    .to_owned(),
            )));
        }
        let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
        // The idle marker is read before the receipt: a receipt rewritten
        // after this read is judged at the next poll, never as idle without
        // it.
        let idle = IdleMarker::read(&*sv.files, sv.signals, &run.idle_marker_path()?)?;
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
                    ReviseOutcome::Mismatch(format!(
                        "the rewritten receipt names commit {} but the worktree HEAD is {head}",
                        receipt.commit
                    ))
                }
                Ok(_) => ReviseOutcome::Mismatch(format!(
                    "the worktree has uncommitted changes on top of HEAD {head}"
                )),
                Err(error) => {
                    ReviseOutcome::Mismatch(format!("the rewritten receipt is invalid: {error:#}"))
                }
            }));
        }
        if !rewritten && idle.is_some_and(|idle| idle.idle_since(self.sent_at)) {
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
