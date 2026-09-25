//! The headless jobs of a run: the review (ADR-0027) and the triage
//! (ADR-0024 decision 3), each a process waited for with a timeout.

use super::*;

/// A headless job's process (a review or a triage) whose stdout and stderr
/// go to files, waited for at most `timeout`.
pub(super) struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `triage`.
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

/// The headless triage in progress, with its output in `triage-N.out` /
/// `triage-N.err` next to the run.
pub(super) struct TriageWatch {
    pub(super) attempt: usize,
    pub(super) job: HeadlessJob,
}

impl TriageWatch {
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<TriageVerdict, String>>> {
        Ok(self
            .job
            .poll(files)?
            .map(|output| output.and_then(|stdout| TriageVerdict::parse(&stdout))))
    }
}
