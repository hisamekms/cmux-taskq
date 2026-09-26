//! The headless jobs of a run: the review (ADR-0027) and the recovery job
//! (ADR-0047 decisions 39 and 40), each a process waited for with a
//! timeout.

use super::*;

/// A headless job's process (a review or a recovery job) whose stdout and stderr
/// go to files, waited for at most `timeout`.
pub(super) struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `recovery job`.
    pub(super) what: &'static str,
    pub(super) child: Box<dyn Spawned>,
    pub(super) started: Instant,
    pub(super) timeout: Duration,
    pub(super) stdout: PathBuf,
    pub(super) stderr: PathBuf,
}

impl HeadlessJob {
    /// `Some` once the job ended: its stdout, or why it failed (a non-zero
    /// exit, or the timeout, after which the process is killed).
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<String, String>>> {
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None if self.started.elapsed() < self.timeout => return Ok(None),
            None => {
                self.stop();
                return Ok(Some(Err(format!(
                    "the headless {} did not finish within {} seconds",
                    self.what,
                    self.timeout.as_secs()
                ))));
            }
        };
        if !status.success {
            let stderr = files.read_to_string(&self.stderr).unwrap_or_default();
            return Ok(Some(Err(format!(
                "the headless {} exited with {status}: {}",
                self.what,
                or_none(tail(stderr.trim(), 500))
            ))));
        }
        Ok(Some(Ok(files
            .read_to_string(&self.stdout)
            .unwrap_or_default())))
    }

    pub(super) fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The headless review in progress, with its output in `review-N.out` /
/// `review-N.err` in the run directory.
pub(super) struct ReviewWatch {
    pub(super) session: Option<SessionRef>,
    pub(super) attempt: usize,
    /// Whether this review is the retry of one whose stdout held no
    /// readable verdict: another unreadable one is not retried again.
    pub(super) retried: bool,
    pub(super) job: HeadlessJob,
}

/// How a headless review ended.
pub(super) enum ReviewEnd {
    Verdict(ReviewVerdict),
    /// The job ended well but its stdout held no readable verdict JSON
    /// (task 328): worth one more review with the same input.
    Unreadable(String),
    /// The job itself failed: it exited non-zero or timed out.
    Failed(String),
}

impl ReviewWatch {
    /// `Some` once the review ended: its verdict, or why there is none.
    pub(super) fn poll(&mut self, files: &dyn RunFiles) -> Result<Option<ReviewEnd>> {
        Ok(self.job.poll(files)?.map(|output| match output {
            Ok(stdout) => match ReviewVerdict::parse(&stdout) {
                Ok(verdict) => ReviewEnd::Verdict(verdict),
                Err(error) => ReviewEnd::Unreadable(error),
            },
            Err(error) => ReviewEnd::Failed(error),
        }))
    }
}

/// The recovery job of a `failed` or `interrupted` run in progress, with
/// its output next to the run (see [`job_file`]).
pub(super) struct EndedRecovery {
    /// The round (`triage_started`'s `attempt`).
    pub(super) round: usize,
    pub(super) alert: RecoveryAlert,
    /// The job's number for its alert (`recovery_requested`'s `attempt`).
    pub(super) attempt: usize,
    pub(super) job: HeadlessJob,
}

impl EndedRecovery {
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<RecoveryVerdict, String>>> {
        Ok(self
            .job
            .poll(files)?
            .map(|output| output.and_then(|stdout| RecoveryVerdict::parse(&stdout))))
    }
}
