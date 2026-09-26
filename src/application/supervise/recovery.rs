//! The recovery job of a live session's alert (ADR-0047 decisions 39 and
//! 40): background work past `[stall].background_alert_secs`
//! (`long_background`), a session that holds the supervisor's `/exit` back
//! after the runtime's own repairs (`stuck_exit`), and a dialog the runtime
//! does not answer by itself (`prompt_waiting`). The supervisor records
//! `recovery_requested` and starts a headless job with the screen, the
//! run's processes and the worktree's state; the job only reads and prints
//! a verdict. The runtime checks every action's preconditions again, and
//! applies a `repair` of high confidence only when all of them hold,
//! recording `auto_repaired` (`layer: recovery`) for each and
//! `recovery_finished`. A verdict it does not apply, one of low confidence,
//! an escalation, a failed job and an alert past [`MAX_RECOVERY_ATTEMPTS`]
//! become the alert's ask to the inbox (`stalled`, `stuck_exit`,
//! `answer_prompt`), with the job's diagnosis, its actions as the
//! recommendation, its options added and its reason category. A run that
//! ended (`failed`, `interrupted`, `resume_exhausted`) is recovered in
//! `triage.rs`, with the same verdict.

use super::*;
use crate::domain::recovery::{
    MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, PROMPT_WAITING_ACTIONS, RecoveryAction, attempts,
    failed_live, run_processes,
};

/// The actions a recovery job may choose for a running session's
/// `long_background` alert.
pub(super) const LONG_BACKGROUND_ACTIONS: [&str; 3] =
    ["stop_processes", "send_instruction", "wait"];

/// The actions for a `stuck_exit` of a run that does not land after its
/// exit: [`STUCK_EXIT_ACTIONS`] without `close_and_proceed`.
pub(super) const STUCK_EXIT_HELD_ACTIONS: [&str; 3] =
    ["answer_known_dialog", "stop_processes", "wait"];

/// How long a stopped process gets between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// What a recovery job printed: its verdict, or why there is none.
type JobOutcome = std::result::Result<RecoveryVerdict, String>;

/// The recovery job in progress for one alert of a live session.
pub(super) struct RecoveryJob {
    pub(super) alert: RecoveryAlert,
    pub(super) attempt: usize,
    /// The idle marker a `long_background` job was started for.
    marker: Option<SystemTime>,
    idle_secs: i64,
    job: HeadlessJob,
}

/// The recovery of one live session: the job in progress (one at a time;
/// another alert waits for it), which idle marker the last
/// `long_background` job was started for, so one marker starts one job (or
/// one more after a `wait`), and the `wait` of a `stuck_exit` or
/// `prompt_waiting` job.
#[derive(Default)]
pub(super) struct RecoveryWatch {
    job: Option<Box<RecoveryJob>>,
    seen: Option<SystemTime>,
    recheck: Option<SystemTime>,
    held: Option<(RecoveryAlert, SystemTime)>,
}

/// What the watch of a live session offers the recovery job of an alert.
pub(super) struct Live<'a> {
    pub(super) workspace: &'a str,
    pub(super) run_dir: &'a Path,
    /// The actions that apply to the alert here.
    pub(super) allowed: &'static [&'static str],
    /// The supervisor typed the session's `/exit`: the condition of the
    /// "Background work is running" dialog (ADR-0047 decision 29).
    pub(super) exit_typed: bool,
    /// The session is idle at its prompt, for `send_instruction`.
    pub(super) at_prompt: bool,
    /// The run lands once its session exits, for `close_and_proceed`.
    pub(super) lands: bool,
}

/// What a `repair` applied to a live session changed.
#[derive(Default)]
pub(super) struct Applied {
    pub(super) names: Vec<&'static str>,
    /// The instruction typed, when and how it went.
    pub(super) sent: Option<(String, SystemTime, Submission)>,
    /// A known dialog was answered or processes were stopped: the session
    /// gets the exit timeout again.
    pub(super) exit_again: bool,
    /// The workspace was closed: the run goes on as after the session's
    /// exit.
    pub(super) closed: bool,
}

/// Where a `stuck_exit` or `prompt_waiting` alert stands after one look.
pub(super) enum LiveStep {
    /// Nothing to do now: its job runs, another alert's does, or its
    /// `wait` holds.
    Pending,
    /// The job's repair was applied.
    Repaired(Applied),
    /// A person is asked: the job's number and why.
    Escalate(usize, Escalation),
    /// The job failed: recorded as `recovery_failed`, the attention a
    /// person recovers the session by hand from (ADR-0047 decision 40); no
    /// ask is opened.
    Failed,
}

/// Record a live session's recovery job that failed (it could not start,
/// exited non-zero, timed out or printed no verdict, ADR-0047 decision 40):
/// `recovery_finished` (`outcome: job_failed`) and `recovery_failed`, the
/// `recover by hand` attention, which waits until the session moves on
/// ([`failed_live`]). Nothing is asked and the session is left as it is.
pub(super) fn fail_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    alert: RecoveryAlert,
    attempt: usize,
    error: &str,
) -> Result<()> {
    warn!(run_id = %run.id(), "run {}: recovery job {attempt} of {} failed: {error}; the session waits to be recovered by hand", run.id(), alert.as_str());
    sv.queue.record_runtime_event(
        run.id(),
        "recovery_finished",
        json!({
            "alert": alert,
            "attempt": attempt,
            "outcome": "job_failed",
            "escalated": false,
            "error": error,
            "reason_category": AskReason::RecoveryFailed,
        }),
    )?;
    sv.queue.record_runtime_event(
        run.id(),
        "recovery_failed",
        json!({
            "code": ReasonCode::JobFailed,
            "alert": alert,
            "attempt": attempt,
            "error": error,
            "workspace_id": workspace,
            "reason_category": AskReason::RecoveryFailed,
        }),
    )?;
    Ok(())
}

/// A person's note for the ask an escalation opens.
pub(super) struct Note {
    /// Why the recovery job did not repair it, as a sentence.
    pub(super) why: String,
    /// The lines for the ask's question: why a person, the diagnosis, the
    /// recommended actions, the job's question and its material.
    pub(super) text: String,
    /// The job's options, to add to the kind's own.
    pub(super) options: Vec<String>,
    pub(super) category: AskReason,
}

fn millis(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn at_millis(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

fn secs_between(from: SystemTime, to: SystemTime) -> i64 {
    to.duration_since(from)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// When the longest running of the marker's `tasks` was first listed in
/// the hook's `idle.log` next to the marker (the streak that ends with the
/// marker written `at`); the marker's own time when the log shows none
/// earlier or cannot be read.
fn background_since(
    sv: &Supervisor<'_>,
    marker_path: &Path,
    at: SystemTime,
    tasks: &[BackgroundTask],
) -> SystemTime {
    let at_ms = millis(at);
    let log = marker_path.with_file_name(crate::domain::stall::IDLE_LOG);
    let Ok(Some((_, bytes))) = sv.files.read_stamped(&log) else {
        return at;
    };
    let history = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (secs, marker) = line.split_once('\t')?;
            let secs: i64 = secs.trim().parse().ok()?;
            Some((
                secs.saturating_mul(1000),
                sv.signals.idle_hook(marker.as_bytes()).background_tasks,
            ))
        })
        .chain([(at_ms, tasks.to_vec())])
        .collect::<Vec<_>>();
    let seen = crate::domain::stall::background_first_seen(history);
    tasks
        .iter()
        .filter_map(|task| seen.get(&task.id).copied())
        .min()
        .filter(|&since| since < at_ms)
        .map_or(at, at_millis)
}

/// Why a person is asked instead of the verdict being applied.
pub(super) enum Escalation {
    /// The job failed or printed no verdict.
    JobFailed(String),
    /// The job answered `escalate`, or `repair` with low confidence.
    Verdict(RecoveryVerdict),
    /// A `repair` whose action does not apply now.
    Refused(RecoveryVerdict, String),
    /// The alert got its [`MAX_RECOVERY_ATTEMPTS`] jobs already.
    UsedUp(usize),
}

impl Escalation {
    pub(super) fn verdict(&self) -> Option<&RecoveryVerdict> {
        match self {
            Self::Verdict(verdict) | Self::Refused(verdict, _) => Some(verdict),
            Self::JobFailed(_) | Self::UsedUp(_) => None,
        }
    }

    /// What the inbox is told (ADR-0047 decision 40): the reason category
    /// is the job's `discard` or `scope`, else `recovery_failed`.
    pub(super) fn note(&self, run: &TaskRun, alert: RecoveryAlert, attempt: usize) -> Note {
        let why = match self {
            Self::JobFailed(error) => {
                format!("the recovery job failed ({error}), so recover it by hand")
            }
            Self::Verdict(verdict) if verdict.verdict == RecoveryDecision::Escalate => {
                "the recovery job could not repair it".to_owned()
            }
            Self::Verdict(_) => {
                "the recovery job was not sure of its repair (confidence low), so it was not applied"
                    .to_owned()
            }
            Self::Refused(_, why) => {
                format!("the runtime did not apply the recovery job's repair: {why}")
            }
            Self::UsedUp(attempts) => format!(
                "the recovery job ran {attempts} times for this alert already (at most {MAX_RECOVERY_ATTEMPTS})"
            ),
        };
        let verdict = self.verdict();
        let category = match verdict.and_then(|v| v.reason_category) {
            Some(category @ (AskReason::Discard | AskReason::Scope)) => category,
            _ => AskReason::RecoveryFailed,
        };
        let mut text = format!("Why a person: {}", category.as_str());
        if let Some(verdict) = verdict {
            text.push_str(&format!("\nDiagnosis: {}", verdict.diagnosis));
            if !verdict.actions.is_empty() {
                text.push_str(&format!(
                    "\nRecommended: {}",
                    serde_json::to_string(&verdict.actions).unwrap_or_default()
                ));
            }
            if !verdict.question.trim().is_empty() {
                text.push_str(&format!("\nQuestion: {}", verdict.question.trim()));
            }
        }
        if !matches!(self, Self::UsedUp(_))
            && let Some(run_dir) = run.run_dir()
        {
            text.push_str(&format!(
                "\nRecovery material: {run_dir}/{}",
                job_file(alert, attempt, "prompt.txt")
            ));
        }
        let mut options = Vec::new();
        for option in verdict.map(|v| v.options.as_slice()).unwrap_or_default() {
            let option = option.trim();
            if !option.is_empty() && !options.iter().any(|o| o == option) {
                options.push(option.to_owned());
            }
        }
        Note {
            why,
            text,
            options,
            category,
        }
    }

    /// `recovery_finished` of an escalation: `ask_id` names the ask it
    /// opened, `None` when an open ask of the run already has a person
    /// looking (`outcome: already_asked`); `extra` adds to it.
    pub(super) fn finished(
        &self,
        alert: RecoveryAlert,
        attempt: usize,
        note: &Note,
        ask_id: Option<AskId>,
        extra: Value,
    ) -> Value {
        let verdict = self.verdict();
        let mut payload = json!({
            "alert": alert,
            "attempt": attempt,
            "verdict": verdict.map(|v| v.verdict),
            "confidence": verdict.map(|v| v.confidence),
            "diagnosis": verdict.map(|v| v.diagnosis.clone()),
            "applied": [],
            "escalated": ask_id.is_some(),
            "why": note.why,
            "reason_category": note.category,
            "ask_id": ask_id,
        });
        if ask_id.is_none() {
            payload["outcome"] = json!("already_asked");
        }
        if let (Value::Object(payload), Value::Object(extra)) = (&mut payload, extra) {
            payload.extend(extra);
        }
        payload
    }

    /// Record [`Self::finished`] for a live session's alert.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record(
        &self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        alert: RecoveryAlert,
        attempt: usize,
        note: &Note,
        ask_id: Option<AskId>,
        extra: Value,
    ) -> Result<()> {
        let payload = self.finished(alert, attempt, note, ask_id, extra);
        sv.queue
            .record_runtime_event(run.id(), "recovery_finished", payload)?;
        Ok(())
    }
}

/// The name of a recovery job's file in the run directory: its prompt
/// (`prompt.txt`), stdout (`out`) and stderr (`err`).
pub(super) fn job_file(alert: RecoveryAlert, attempt: usize, what: &str) -> String {
    format!("recovery-{}-{attempt}.{what}", alert.as_str())
}

/// The event an alert of a live session is raised from, the evidence of
/// its `recovery_requested`.
fn evidence_kind(alert: RecoveryAlert) -> Option<&'static str> {
    match alert {
        RecoveryAlert::StuckExit => Some("exit_request_timed_out"),
        RecoveryAlert::PromptWaiting => Some("prompt_waiting"),
        _ => None,
    }
}

impl RecoveryWatch {
    /// Rebuild the watch of an adopted run from its events, so a marker
    /// already handed to a job is not handed again.
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let alert = |e: &&crate::domain::RunEvent| {
            e.payload["alert"] == RecoveryAlert::LongBackground.as_str()
        };
        let mut watch = Self::default();
        if let Some(event) = events
            .iter()
            .filter(alert)
            // A job the previous supervisor left running is gone: its
            // marker gets a new one (counted as another attempt).
            .rfind(|e| e.kind == "recovery_finished")
        {
            watch.seen = event.payload["marker_at_ms"].as_i64().map(at_millis);
            watch.recheck = event.payload["recheck_at_ms"].as_i64().map(at_millis);
        }
        Ok(watch)
    }

    /// Stop a job still running: the session ended or the run moved on.
    pub(super) fn stop(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) {
        self.stop_for(sv, run, None, "session_ended");
    }

    /// Stop the job of `alert` (any alert's when `None`), recording
    /// `outcome` as its `recovery_finished`.
    pub(super) fn stop_for(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        alert: Option<RecoveryAlert>,
        outcome: &str,
    ) {
        if alert.is_some_and(|alert| self.held.is_some_and(|(held, _)| held == alert)) {
            self.held = None;
        }
        if self
            .job
            .as_ref()
            .is_none_or(|job| alert.is_some_and(|alert| job.alert != alert))
        {
            return;
        }
        let mut job = self.job.take().expect("checked above");
        job.job.stop();
        let recorded = sv.queue.record_runtime_event(
            run.id(),
            "recovery_finished",
            json!({
                "alert": job.alert,
                "attempt": job.attempt,
                "outcome": outcome,
                "marker_at_ms": job.marker.map(millis),
            }),
        );
        if let Err(error) = recorded {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the stopped recovery job could not be recorded: {error:#}", run.id());
        }
    }

    /// Whether a recovery job is running for the session: a run then waits
    /// for the job, not for a person (ADR-0062 decision 6).
    pub(super) fn running(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn stop_job(&mut self) {
        if let Some(job) = &mut self.job {
            job.job.stop();
        }
    }

    /// Record `recovery_requested` for `alert` with `facts` and start its
    /// job; `Some` when a person is asked at once instead: the alert got
    /// its jobs already (nothing is recorded), or the job could not start.
    fn start(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        alert: RecoveryAlert,
        facts: Value,
        marker: Option<(SystemTime, i64)>,
    ) -> Result<Option<(usize, Escalation)>> {
        let events = sv.queue.run_events(run.id())?;
        let done = attempts(&events, alert);
        if done >= MAX_RECOVERY_ATTEMPTS {
            return Ok(Some((done, Escalation::UsedUp(done))));
        }
        let attempt = done + 1;
        let evidence: Vec<EventId> = evidence_kind(alert)
            .and_then(|kind| events.iter().rev().find(|e| e.kind == kind))
            .map(|e| e.id)
            .into_iter()
            .collect();
        let mut payload = json!({
            "alert": alert,
            "attempt": attempt,
            "evidence": evidence,
            "workspace_id": live.workspace,
        });
        if let (Value::Object(payload), Value::Object(facts)) = (&mut payload, facts) {
            payload.extend(facts);
        }
        sv.queue
            .record_runtime_event(run.id(), "recovery_requested", payload.clone())?;
        info!(run_id = %run.id(), "run {}: alert {}; recovery job {attempt} starts", run.id(), alert.as_str());
        match spawn_live(sv, run, live, alert, attempt, &payload) {
            Ok(job) => {
                self.job = Some(Box::new(RecoveryJob {
                    alert,
                    attempt,
                    marker: marker.map(|(at, _)| at),
                    idle_secs: marker.map_or(0, |(_, secs)| secs),
                    job,
                }));
                Ok(None)
            }
            Err(error) => Ok(Some((
                attempt,
                Escalation::JobFailed(format!("the recovery job could not start: {error:#}")),
            ))),
        }
    }

    /// The job of `alert` that ended, with its verdict or why there is
    /// none.
    fn ended(
        &mut self,
        sv: &Supervisor<'_>,
        alert: RecoveryAlert,
    ) -> Result<Option<(Box<RecoveryJob>, JobOutcome)>> {
        let Some(job) = self.job.as_mut().filter(|job| job.alert == alert) else {
            return Ok(None);
        };
        let Some(output) = job.job.poll(&*sv.files)? else {
            return Ok(None);
        };
        let job = self.job.take().expect("polled above");
        Ok(Some((
            job,
            output.and_then(|stdout| RecoveryVerdict::parse(&stdout)),
        )))
    }

    /// Follow a `stuck_exit` or `prompt_waiting` alert that holds now: act
    /// on its job once it ended, or start one when no job runs, no `wait`
    /// holds the alert and no failed job of it waits for a person
    /// ([`failed_live`]). A job that failed is recorded by [`fail_live`]
    /// and returned as [`LiveStep::Failed`].
    pub(super) fn follow(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        alert: RecoveryAlert,
        facts: impl FnOnce() -> Value,
    ) -> Result<LiveStep> {
        if let Some((job, verdict)) = self.ended(sv, alert)? {
            let verdict = match verdict {
                Ok(verdict) => verdict,
                Err(error) => {
                    fail_live(sv, run, live.workspace, alert, job.attempt, &error)?;
                    return Ok(LiveStep::Failed);
                }
            };
            return Ok(match apply_live(sv, run, live, &job, verdict)? {
                Ok(applied) => {
                    if let Some(at) = applied_recheck(sv, &job, run)? {
                        self.held = Some((alert, at));
                    }
                    LiveStep::Repaired(applied)
                }
                Err(escalation) => LiveStep::Escalate(job.attempt, escalation),
            });
        }
        if self.job.is_some()
            || self
                .held
                .is_some_and(|(held, at)| held == alert && sv.files.now() < at)
        {
            return Ok(LiveStep::Pending);
        }
        if failed_live(&sv.queue.run_events(run.id())?, Some(alert)).is_some() {
            return Ok(LiveStep::Pending);
        }
        self.held = None;
        Ok(match self.start(sv, run, live, alert, facts(), None)? {
            Some((attempt, Escalation::JobFailed(error))) => {
                fail_live(sv, run, live.workspace, alert, attempt, &error)?;
                LiveStep::Failed
            }
            Some((attempt, escalation)) => LiveStep::Escalate(attempt, escalation),
            None => LiveStep::Pending,
        })
    }
}

/// When the `wait` of the job's applied verdict ends, from its
/// `recovery_finished`.
fn applied_recheck(
    sv: &Supervisor<'_>,
    job: &RecoveryJob,
    run: &TaskRun,
) -> Result<Option<SystemTime>> {
    Ok(sv
        .queue
        .run_events(run.id())?
        .iter()
        .rev()
        .find(|e| {
            e.kind == "recovery_finished"
                && e.payload["alert"] == job.alert.as_str()
                && e.payload["attempt"] == job.attempt
        })
        .and_then(|e| e.payload["recheck_at_ms"].as_i64())
        .map(at_millis))
}

/// The run's processes that `stop_processes` may stop: those of its
/// worktree or under its session, never the session's wrapper or agent.
fn own_processes(
    sv: &Supervisor<'_>,
    run: &TaskRun,
) -> Result<Vec<crate::domain::recovery::ProcessInfo>> {
    let worktree = run.worktree_path().context("the run has no worktree")?;
    let processes = sv.queue.processes(run.id())?;
    let all = sv.processes.list()?;
    let pid = |role: &str| {
        processes
            .iter()
            .find(|p| p.role == role)
            .map(|p| p.pid)
            .with_context(|| format!("the session's {role} is not registered"))
    };
    Ok(run_processes(
        &all,
        Path::new(worktree),
        Some(pid("wrapper")?),
        Some(pid("agent")?),
        std::process::id(),
    )
    .into_iter()
    .cloned()
    .collect())
}

/// Write the job's prompt with what the runtime reads now and start it in
/// the run directory, allowed to read only.
fn spawn_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    alert: RecoveryAlert,
    attempt: usize,
    facts: &Value,
) -> Result<HeadlessJob> {
    let task = sv.queue.show(run.task_id())?.task;
    let screen = match sv.cmux.capture(live.workspace) {
        Ok(screen) => sv.signals.screen_excerpt(&screen),
        Err(error) => format!("(the screen could not be read: {error:#})"),
    };
    let listed = own_processes(sv, run).map_err(|error| format!("{error:#}"));
    let (status, head, receipt) = git_facts(sv, run)?;
    let history = repair_history(sv, run)?;
    let material = RecoveryMaterial {
        alert,
        ended: None,
        facts,
        workspace: live.workspace,
        screen: &screen,
        processes: listed,
        git_status: &status,
        head: &head,
        receipt_commit: receipt.as_deref(),
        history: &history,
        allowed: live.allowed,
    };
    let prompt = recovery_prompt(&task, run, attempt, &material)?;
    start_job(sv, live.run_dir, alert, attempt, &prompt, None)
}

/// The worktree's `git status`, HEAD and the receipt's `commit`, for the
/// recovery job; what cannot be read says so.
pub(super) fn git_facts(
    sv: &Supervisor<'_>,
    run: &TaskRun,
) -> Result<(String, String, Option<String>)> {
    let worktree = run.worktree_path().map(Path::new);
    let status = match worktree {
        Some(worktree) if sv.files.is_dir(worktree) => sv
            .repository
            .status(worktree)
            .unwrap_or_else(|error| format!("(unreadable: {error:#})")),
        _ => "(no worktree)".to_owned(),
    };
    let head = match worktree {
        Some(worktree) if sv.files.is_dir(worktree) => sv.repository.head(worktree).map_or_else(
            |error| format!("(unreadable: {error:#})"),
            |h| h.to_string(),
        ),
        _ => "(no worktree)".to_owned(),
    };
    let receipt = run
        .receipt_path()
        .and_then(|path| sv.files.read(Path::new(path)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|receipt| receipt["commit"].as_str().map(str::to_owned));
    Ok((status, head, receipt))
}

/// The run's earlier recovery verdicts and automatic repairs.
pub(super) fn repair_history(sv: &Supervisor<'_>, run: &TaskRun) -> Result<Vec<Value>> {
    Ok(sv
        .queue
        .run_events(run.id())?
        .iter()
        .filter(|e| matches!(e.kind.as_str(), "recovery_finished" | "auto_repaired"))
        .map(super::super::health::compact_event)
        .collect())
}

/// Write `prompt` next to the run and start the headless job in `dir`,
/// allowed to read only; the job's environment and CLI are the review's.
/// `session_id` is the Claude session id the job runs as, when its start
/// recorded one (ADR-0048 decision 4).
pub(super) fn start_job(
    sv: &mut Supervisor<'_>,
    dir: &Path,
    alert: RecoveryAlert,
    attempt: usize,
    prompt: &str,
    session_id: Option<&str>,
) -> Result<HeadlessJob> {
    sv.files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    sv.files.write(
        &dir.join(job_file(alert, attempt, "prompt.txt")),
        prompt.as_bytes(),
    )?;
    let stdout = dir.join(job_file(alert, attempt, "out"));
    let stderr = dir.join(job_file(alert, attempt, "err"));
    let mut command = sv.reviewer.headless_command(dir, prompt, TRIAGE_TOOLS)?;
    if let Some(session_id) = session_id {
        sv.reviewer.assign_session_id(&mut command, session_id);
    }
    command.envs(sv.layout.job_env.iter().cloned());
    let child = sv
        .spawner
        .spawn(
            &command,
            Streams::Files {
                stdout: &stdout,
                stderr: &stderr,
            },
        )
        .context("start the recovery job")?;
    Ok(HeadlessJob {
        what: "recovery job",
        child,
        started: Instant::now(),
        timeout: sv.reviewer.review_timeout(),
        stdout,
        stderr,
    })
}

/// Check each action's preconditions now (ADR-0047 decision 40): only the
/// actions allowed for the alert here, only the run's own processes, an
/// instruction only into a session at its prompt, a known dialog only
/// under its rule, and a close only of a run that lands and whose receipt
/// still holds. The first that fails refuses the whole verdict.
fn check_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    actions: &[RecoveryAction],
) -> std::result::Result<(), String> {
    for action in actions {
        if !live.allowed.contains(&action.name()) {
            return Err(format!(
                "{} does not apply to this alert of a live session (allowed: {})",
                action.name(),
                live.allowed.join(", ")
            ));
        }
        match action {
            RecoveryAction::StopProcesses { pids } => {
                if pids.is_empty() {
                    return Err("stop_processes names no pid".to_owned());
                }
                let own = own_processes(sv, run).map_err(|error| {
                    format!("the run's processes could not be listed: {error:#}")
                })?;
                if let Some(pid) = pids.iter().find(|pid| !own.iter().any(|p| p.pid == **pid)) {
                    return Err(format!(
                        "pid {pid} is not one of the run's own processes (working directory in its worktree, or under its session; never the session itself)"
                    ));
                }
            }
            RecoveryAction::SendInstruction { instruction } => {
                if instruction.trim().is_empty() {
                    return Err("send_instruction has no instruction".to_owned());
                }
                if !live.at_prompt {
                    return Err(
                        "the session is not idle at its prompt for an instruction".to_owned()
                    );
                }
            }
            RecoveryAction::AnswerKnownDialog { dialog } => {
                known_dialog_ready(sv, run, live.workspace, live.exit_typed, dialog)
                    .map_err(|why| format!("answer_known_dialog: {why}"))?;
            }
            RecoveryAction::CloseAndProceed => {
                if !live.lands {
                    return Err(
                        "close_and_proceed: the run does not land after its session's exit"
                            .to_owned(),
                    );
                }
                if let Some(why) = landable_without_exit(sv, run) {
                    return Err(format!("close_and_proceed: {why}"));
                }
            }
            RecoveryAction::Wait { .. } => (),
            other => {
                return Err(format!("{} does not apply to a live session", other.name()));
            }
        }
    }
    Ok(())
}

/// Apply a verdict to a live session: a `repair` of high confidence whose
/// every action holds now, each recorded as `auto_repaired`, and
/// `recovery_finished`; otherwise the escalation.
pub(super) fn apply_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    job: &RecoveryJob,
    verdict: RecoveryVerdict,
) -> Result<std::result::Result<Applied, Escalation>> {
    let duration_secs = job.job.started.elapsed().as_secs();
    if !verdict.applies() {
        return Ok(Err(Escalation::Verdict(verdict)));
    }
    if let Err(why) = check_live(sv, run, live, &verdict.actions) {
        warn!(run_id = %run.id(), "run {}: recovery job {} of {} answered repair, but {why}; asking the inbox", run.id(), job.attempt, job.alert.as_str());
        return Ok(Err(Escalation::Refused(verdict, why)));
    }
    let mut applied = Applied::default();
    let mut recheck_at = None;
    let repaired = |sv: &mut Supervisor<'_>, action: &RecoveryAction, detail: Value| {
        let mut payload = json!({
            "layer": "recovery",
            "repair": action.name(),
            "alert": job.alert,
            "attempt": job.attempt,
        });
        if let (Value::Object(payload), Value::Object(detail)) = (&mut payload, detail) {
            payload.extend(detail);
        }
        sv.queue
            .record_runtime_event(run.id(), "auto_repaired", payload)
    };
    for action in &verdict.actions {
        match action {
            RecoveryAction::StopProcesses { pids } => {
                let stopped = match stop_processes(sv, run, pids) {
                    Ok(stopped) => stopped,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("stopping the processes failed: {error:#}"),
                        )));
                    }
                };
                repaired(sv, action, json!({"processes": stopped}))?;
                applied.exit_again = true;
                info!(run_id = %run.id(), "run {}: recovery job {} stopped processes {pids:?} of its worktree", run.id(), job.attempt);
            }
            RecoveryAction::SendInstruction { instruction } => {
                let text = format!(
                    "dagq: the supervisor's recovery job for run {} (alert {}) asks: {}",
                    run.id(),
                    job.alert.as_str(),
                    instruction.trim()
                );
                let sent_at = sv.files.now();
                let submission = match submit(
                    sv,
                    run,
                    live.workspace,
                    Input::Text(&text),
                    "recovery instruction",
                ) {
                    Ok(submission) => submission,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("the instruction could not be typed: {error:#}"),
                        )));
                    }
                };
                repaired(
                    sv,
                    action,
                    json!({"instruction": instruction, "workspace_id": live.workspace}),
                )?;
                applied.sent = Some((text, sent_at, submission));
            }
            RecoveryAction::AnswerKnownDialog { .. } => {
                let screen = match sv.cmux.capture(live.workspace) {
                    Ok(screen) => screen,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("the screen could not be read: {error:#}"),
                        )));
                    }
                };
                if !answer_known_dialog(
                    sv,
                    run,
                    live.workspace,
                    &screen,
                    live.exit_typed,
                    Some((job.alert, job.attempt)),
                )? {
                    return Ok(Err(Escalation::Refused(
                        verdict.clone(),
                        "the known dialog's keys could not be sent".to_owned(),
                    )));
                }
                applied.exit_again = true;
            }
            RecoveryAction::CloseAndProceed => {
                if let Err(error) = sv.cmux.close(live.workspace) {
                    return Ok(Err(Escalation::Refused(
                        verdict.clone(),
                        format!("its workspace could not be closed: {error:#}"),
                    )));
                }
                repaired(
                    sv,
                    action,
                    json!({
                        "workspace_id": live.workspace,
                        "conditions": {"review": "pass", "clean": true, "receipt": "head", "reviewed": true},
                    }),
                )?;
                applied.closed = true;
            }
            RecoveryAction::Wait { recheck_after_secs } => {
                let at = sv.files.now()
                    + Duration::from_secs((*recheck_after_secs).min(MAX_RECHECK_SECS));
                recheck_at = Some(millis(at));
            }
            _ => unreachable!("checked above"),
        }
        applied.names.push(action.name());
    }
    sv.queue.record_runtime_event(
        run.id(),
        "recovery_finished",
        json!({
            "alert": job.alert,
            "attempt": job.attempt,
            "verdict": verdict.verdict,
            "confidence": verdict.confidence,
            "diagnosis": verdict.diagnosis,
            "applied": applied.names,
            "escalated": false,
            "marker_at_ms": job.marker.map(millis),
            "recheck_at_ms": recheck_at,
            "duration_secs": duration_secs,
        }),
    )?;
    info!(run_id = %run.id(), "run {}: recovery job {} of {} repaired it ({}): {}", run.id(), job.attempt, job.alert.as_str(), applied.names.join(", "), verdict.diagnosis);
    Ok(Ok(applied))
}

impl SessionWatch {
    /// Whether the session is idle at its prompt, for `send_instruction`:
    /// no dialog recorded, and a turn ended since the last input.
    fn at_prompt(&self, sv: &Supervisor<'_>) -> bool {
        self.prompt_hash.is_none()
            && sv
                .files
                .modified(&self.idle_marker)
                .is_ok_and(|marker| self.stall.turn_since_input(marker))
    }

    /// One look at the session's background work (ADR-0047 decision 39):
    /// follow its recovery job in progress, or start one when the idle
    /// marker says background work has run past the threshold, no other
    /// alert's job runs and no `stalled` ask is open for the run.
    pub(super) fn watch_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        // After the receipt the session's own wait on background work
        // takes over (up to the resume timeout): no job acts on it.
        if self.receipt_seen {
            self.recovery.stop_for(
                sv,
                run,
                Some(RecoveryAlert::LongBackground),
                "session_ended",
            );
            return Ok(());
        }
        if let Some((job, verdict)) = self.recovery.ended(sv, RecoveryAlert::LongBackground)? {
            return self.act_on_background(sv, run, &job, verdict);
        }
        if self.recovery.running()
            || sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)?
            || failed_live(
                &sv.queue.run_events(run.id())?,
                Some(RecoveryAlert::LongBackground),
            )
            .is_some()
        {
            return Ok(());
        }
        let Some(idle) = IdleMarker::read(&*sv.files, sv.signals, &self.idle_marker)? else {
            return Ok(());
        };
        if !idle.background_running() {
            return Ok(());
        }
        let now = sv.files.now();
        let marker = idle.modified();
        // Timed from when the running tasks were first listed, as `stats`
        // does (task 331), so turns the session keeps taking do not reset it.
        let since = background_since(sv, &self.idle_marker, marker, idle.background_tasks());
        let idle_secs = secs_between(since, now);
        if idle_secs <= sv.stall.background_alert_secs
            || self.recovery.recheck.is_some_and(|at| now < at)
            || (self.recovery.recheck.is_none()
                && self.recovery.seen.is_some_and(|seen| marker <= seen))
        {
            return Ok(());
        }
        self.recovery.recheck = None;
        self.recovery.seen = Some(marker);
        let facts = json!({
            "idle_secs": idle_secs,
            "background_since_ms": millis(since),
            "threshold": BACKGROUND_THRESHOLD,
            "threshold_secs": sv.stall.background_alert_secs,
            "background_tasks": idle.background_tasks(),
            "marker_at_ms": millis(marker),
        });
        info!(run_id = %run.id(), "run {}: background work has run {idle_secs}s (over {}s)", run.id(), sv.stall.background_alert_secs);
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &LONG_BACKGROUND_ACTIONS,
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
        };
        let started = self.recovery.start(
            sv,
            run,
            &live,
            RecoveryAlert::LongBackground,
            facts,
            Some((marker, idle_secs)),
        )?;
        match started {
            None => Ok(()),
            Some((attempt, escalation)) => {
                self.escalate_background(sv, run, attempt, marker, idle_secs, escalation)
            }
        }
    }

    /// Act on the `long_background` job's verdict: apply a `repair` of high
    /// confidence whose every action holds now, and ask the inbox
    /// otherwise.
    fn act_on_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        job: &RecoveryJob,
        verdict: std::result::Result<RecoveryVerdict, String>,
    ) -> Result<()> {
        let marker = job.marker.unwrap_or(UNIX_EPOCH);
        let verdict = match verdict {
            Ok(verdict) => verdict,
            Err(error) => {
                return self.escalate_background(
                    sv,
                    run,
                    job.attempt,
                    marker,
                    job.idle_secs,
                    Escalation::JobFailed(error),
                );
            }
        };
        let (workspace, run_dir) = (self.workspace.clone(), self.run_dir.clone());
        let live = Live {
            workspace: &workspace,
            run_dir: &run_dir,
            allowed: &LONG_BACKGROUND_ACTIONS,
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
        };
        match apply_live(sv, run, &live, job, verdict)? {
            Ok(applied) => {
                if let Some((text, sent_at, submission)) = applied.sent {
                    self.stall.input_sent(sent_at);
                    self.answer_start = Some(StartCheck::new(
                        "recovery instruction",
                        &text,
                        sent_at,
                        &submission,
                    ));
                }
                self.recovery.recheck = applied_recheck(sv, job, run)?;
                Ok(())
            }
            Err(escalation) => {
                self.escalate_background(sv, run, job.attempt, marker, job.idle_secs, escalation)
            }
        }
    }

    /// Raise the `long_background` alert to the inbox as a `stalled` ask
    /// with the job's diagnosis, its recommended actions and why a person
    /// is needed, and hand the ask to the [`StallWatch`].
    fn escalate_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        marker: SystemTime,
        idle_secs: i64,
        escalation: Escalation,
    ) -> Result<()> {
        let alert = RecoveryAlert::LongBackground;
        if let Escalation::JobFailed(error) = &escalation {
            let workspace = self.workspace.clone();
            return fail_live(sv, run, &workspace, alert, attempt, error);
        }
        let note = escalation.note(run, alert, attempt);
        let extra = json!({"marker_at_ms": millis(marker)});
        // A `stalled` ask opened meanwhile (the idle detection's) already
        // has a person looking: the diagnosis is recorded, not merged into
        // it.
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            escalation.record(sv, run, alert, attempt, &note, None, extra)?;
            warn!(run_id = %run.id(), "run {}: {}; a stalled ask is already open, so no other is asked", run.id(), note.why);
            return Ok(());
        }
        let question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has had background work running for {idle_secs}s (alert: long_background, over {threshold}s), and {why}.\n{text}\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, stop its background work, type an instruction; see the dagq-recover skill). This ask closes itself once the session moves on.",
            run_id = run.id(),
            task_id = run.task_id(),
            workspace = self.workspace,
            threshold = sv.stall.background_alert_secs,
            why = note.why,
            text = note.text,
        );
        let mut options: Vec<String> = STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
        options.extend(
            note.options
                .iter()
                .filter(|o| !options.contains(o))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.repo_root,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: note.category,
                finding_id: None,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let now = sv.files.now();
        self.stall
            .escalated(id, now, idle_secs, BACKGROUND_THRESHOLD);
        escalation.record(sv, run, alert, attempt, &note, Some(id), extra)?;
        warn!(ask_id = %id, run_id = %run.id(), "run {}: {}; stalled ask {id} (notified: {})", run.id(), note.why, outcome["notified"]);
        Ok(())
    }

    /// The session holds its `/exit` back past the exit timeout: its
    /// recovery job (`stuck_exit`), and the `stuck_exit` ask once it
    /// escalates. A repair that answered a dialog or stopped processes
    /// gives the session the exit timeout again.
    pub(super) fn recover_stuck_exit(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        // A `long_background` job is only followed before the `/exit`: left
        // running, it would hold this alert's job back for good.
        self.recovery.stop_for(
            sv,
            run,
            Some(RecoveryAlert::LongBackground),
            "exit_requested",
        );
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &STUCK_EXIT_HELD_ACTIONS,
            exit_typed: self.exit_requested.is_some(),
            at_prompt: false,
            lands: false,
        };
        let timeout = sv.cmux.exit_timeout().as_secs();
        let step = self.recovery.follow(sv, run, &live, RecoveryAlert::StuckExit, || {
            json!({"timeout_secs": timeout, "exit_typed": true, "silent": self.exit_for_silence})
        })?;
        match step {
            LiveStep::Pending => {}
            // A person recovers it by hand from the attention: asked.
            LiveStep::Failed => self.exit_asked = true,
            LiveStep::Repaired(applied) => {
                if applied.exit_again {
                    self.exit_requested = Some(Instant::now());
                    self.exit_timed_out = false;
                }
            }
            LiveStep::Escalate(attempt, escalation) => {
                let note = escalation.note(run, RecoveryAlert::StuckExit, attempt);
                let after = stuck_exit_after(
                    self.exit_for_silence,
                    "The run stays running, and goes on to validating once the session exits",
                );
                let workspace = self.workspace.clone();
                let id = ask_stuck_exit(sv, run, &workspace, &after, Some(&note))?;
                escalation.record(
                    sv,
                    run,
                    RecoveryAlert::StuckExit,
                    attempt,
                    &note,
                    Some(id),
                    json!({}),
                )?;
                self.exit_asked = true;
            }
        }
        Ok(())
    }

    /// The session waits at a dialog the runtime does not answer: its
    /// recovery job (`prompt_waiting`), and the `answer_prompt` ask once it
    /// escalates. `kind` and `excerpt` are the dialog on `screen` now.
    pub(super) fn recover_prompt(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        kind: &str,
        excerpt: &str,
    ) -> Result<()> {
        if sv.queue.has_unclosed_ask(run.id(), AskKind::AnswerPrompt)? {
            return Ok(());
        }
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &PROMPT_WAITING_ACTIONS,
            exit_typed: self.exit_requested.is_some(),
            at_prompt: false,
            lands: false,
        };
        let hash = self.prompt_hash.clone();
        let step = self.recovery.follow(
            sv,
            run,
            &live,
            RecoveryAlert::PromptWaiting,
            || json!({"prompt": kind, "screen_hash": hash, "excerpt": excerpt}),
        )?;
        match step {
            LiveStep::Pending | LiveStep::Repaired(_) | LiveStep::Failed => {}
            LiveStep::Escalate(attempt, escalation) => {
                let note = escalation.note(run, RecoveryAlert::PromptWaiting, attempt);
                let workspace = self.workspace.clone();
                let id = ask_answer_prompt(sv, run, &workspace, kind, excerpt, Some(&note))?;
                escalation.record(
                    sv,
                    run,
                    RecoveryAlert::PromptWaiting,
                    attempt,
                    &note,
                    Some(id),
                    json!({}),
                )?;
            }
        }
        Ok(())
    }
}

/// Stop `pids` (SIGTERM, then SIGKILL after [`STOP_GRACE`]), checking once
/// more right before that each is the run's own. Returns what was stopped.
fn stop_processes(sv: &Supervisor<'_>, run: &TaskRun, pids: &[u32]) -> Result<Vec<Value>> {
    let own = own_processes(sv, run)?;
    // A pid no longer among the run's own ended by itself (or is another
    // process now): it is left alone and recorded as gone.
    let (targets, gone): (Vec<_>, Vec<_>) = pids
        .iter()
        .map(|pid| (pid, own.iter().find(|p| p.pid == *pid)))
        .partition(|(_, found)| found.is_some());
    let targets: Vec<&crate::domain::recovery::ProcessInfo> =
        targets.into_iter().filter_map(|(_, found)| found).collect();
    for process in &targets {
        if let Err(error) = sv.processes.terminate(process.pid)
            && sv.processes.alive(process.pid)
        {
            return Err(error.context(format!("stop pid {}", process.pid)));
        }
    }
    let started = Instant::now();
    while started.elapsed() < STOP_GRACE && targets.iter().any(|p| sv.processes.alive(p.pid)) {
        thread::sleep(Duration::from_millis(50));
    }
    let mut stopped = Vec::new();
    for process in targets {
        let killed = sv.processes.alive(process.pid);
        if killed
            && let Err(error) = sv.processes.kill(process.pid)
            && sv.processes.alive(process.pid)
        {
            return Err(error.context(format!("kill pid {}", process.pid)));
        }
        stopped.push(json!({
            "pid": process.pid,
            "ppid": process.ppid,
            "command": process.command,
            "cwd": process.cwd,
            "killed": killed,
        }));
    }
    for (pid, _) in gone {
        stopped.push(json!({"pid": pid, "gone": true}));
    }
    Ok(stopped)
}
