//! The recovery job of a running session's `long_background` alert
//! (ADR-0047 decisions 39 and 40), which widens the triage to a session
//! still alive: background work the session started has run past
//! `[stall].background_alert_secs`, so the supervisor records
//! `recovery_requested` and starts a headless job with the screen, the
//! run's processes and the worktree's state. The job only reads and
//! prints a verdict; the runtime checks each action's preconditions again
//! and applies it (`stop_processes` stops only the run's own processes),
//! recording `auto_repaired` and `recovery_finished`. A verdict it cannot
//! apply, one of low confidence, an escalation, a failed job and an alert
//! past [`MAX_RECOVERY_ATTEMPTS`] become one `stalled` ask to the inbox,
//! which the [`StallWatch`] then follows.

use super::*;
use crate::domain::recovery::{
    MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, RecoveryAction, RecoveryAlert, RecoveryVerdict,
    run_processes,
};

/// The actions a recovery job may choose for a running session's
/// `long_background` alert.
pub(super) const LONG_BACKGROUND_ACTIONS: [&str; 3] =
    ["stop_processes", "send_instruction", "wait"];

/// How long a stopped process gets between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The recovery job in progress for one alert.
pub(super) struct RecoveryJob {
    attempt: usize,
    /// The idle marker it was started for.
    marker: SystemTime,
    idle_secs: i64,
    job: HeadlessJob,
}

/// The `long_background` alert of one session: the job in progress, and
/// which idle marker the last one was started for, so one marker starts
/// one job (or one more after a `wait`).
#[derive(Default)]
pub(super) struct RecoveryWatch {
    job: Option<Box<RecoveryJob>>,
    seen: Option<SystemTime>,
    recheck: Option<SystemTime>,
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
enum Escalation {
    /// The job failed or printed no verdict.
    JobFailed(String),
    /// The job answered `escalate`, or `repair` with low confidence.
    Verdict(RecoveryVerdict),
    /// A `repair` whose action does not apply now.
    Refused(RecoveryVerdict, String),
    /// The alert got its [`MAX_RECOVERY_ATTEMPTS`] jobs already.
    UsedUp(usize),
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
        if let Some(mut job) = self.job.take() {
            job.job.stop();
            let recorded = sv.queue.record_runtime_event(
                run.id(),
                "recovery_finished",
                json!({
                    "alert": RecoveryAlert::LongBackground,
                    "attempt": job.attempt,
                    "outcome": "session_ended",
                    "marker_at_ms": millis(job.marker),
                }),
            );
            if let Err(error) = recorded {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the stopped recovery job could not be recorded: {error:#}", run.id());
            }
        }
    }

    pub(super) fn stop_job(&mut self) {
        if let Some(job) = &mut self.job {
            job.job.stop();
        }
    }
}

impl SessionWatch {
    /// One look at the session's background work (ADR-0047 decision 39):
    /// follow the recovery job in progress, or start one when the idle
    /// marker says background work has run past the threshold and no
    /// `stalled` ask is open for the run.
    pub(super) fn watch_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
    ) -> Result<()> {
        // After the receipt the session's own wait on background work
        // takes over (up to the resume timeout): no job acts on it.
        if self.receipt_seen {
            self.recovery.stop(sv, run);
            return Ok(());
        }
        if let Some(job) = &mut self.recovery.job {
            let Some(output) = job.job.poll(&*sv.files)? else {
                return Ok(());
            };
            let job = self.recovery.job.take().expect("polled above");
            let verdict = output.and_then(|stdout| RecoveryVerdict::parse(&stdout));
            return self.act_on_recovery(sv, run, processes, &job, verdict);
        }
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
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
        let attempts = sv
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| {
                e.kind == "recovery_requested"
                    && e.payload["alert"] == RecoveryAlert::LongBackground.as_str()
            })
            .count();
        if attempts >= MAX_RECOVERY_ATTEMPTS {
            return self.escalate(
                sv,
                run,
                attempts,
                marker,
                idle_secs,
                Escalation::UsedUp(attempts),
            );
        }
        let attempt = attempts + 1;
        let facts = json!({
            "alert": RecoveryAlert::LongBackground,
            "attempt": attempt,
            "idle_secs": idle_secs,
            "background_since_ms": millis(since),
            "threshold": BACKGROUND_THRESHOLD,
            "threshold_secs": sv.stall.background_alert_secs,
            "background_tasks": idle.background_tasks(),
            "marker_at_ms": millis(marker),
            "workspace_id": self.workspace,
        });
        sv.queue
            .record_runtime_event(run.id(), "recovery_requested", facts.clone())?;
        info!(run_id = %run.id(), "run {}: background work has run {idle_secs}s (over {}s); recovery job {attempt} starts", run.id(), sv.stall.background_alert_secs);
        match self.spawn_recovery(sv, run, processes, attempt, &facts) {
            Ok(job) => {
                self.recovery.job = Some(Box::new(RecoveryJob {
                    attempt,
                    marker,
                    idle_secs,
                    job,
                }));
                Ok(())
            }
            Err(error) => self.escalate(
                sv,
                run,
                attempt,
                marker,
                idle_secs,
                Escalation::JobFailed(format!("the recovery job could not start: {error:#}")),
            ),
        }
    }

    /// The run's processes that `stop_processes` may stop.
    fn own_processes(
        sv: &Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
    ) -> Result<Vec<crate::domain::recovery::ProcessInfo>> {
        let worktree = run.worktree_path().context("the run has no worktree")?;
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

    /// Write the job's prompt with what the runtime reads now and start it
    /// in the run directory, allowed to read only.
    fn spawn_recovery(
        &self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
        attempt: usize,
        facts: &Value,
    ) -> Result<HeadlessJob> {
        let task = sv.queue.show(run.task_id())?.task;
        let screen = match sv.cmux.capture(&self.workspace) {
            Ok(screen) => sv.signals.screen_excerpt(&screen),
            Err(error) => format!("(the screen could not be read: {error:#})"),
        };
        let listed = Self::own_processes(sv, run, processes).map_err(|error| format!("{error:#}"));
        let worktree = Path::new(run.worktree_path().context("the run has no worktree")?);
        let status = sv
            .repository
            .status(worktree)
            .unwrap_or_else(|error| format!("(unreadable: {error:#})"));
        let head = sv.repository.head(worktree).map_or_else(
            |error| format!("(unreadable: {error:#})"),
            |h| h.to_string(),
        );
        let receipt = run
            .receipt_path()
            .and_then(|path| sv.files.read(Path::new(path)).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|receipt| receipt["commit"].as_str().map(str::to_owned));
        let history: Vec<Value> = sv
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "recovery_finished" | "auto_repaired"))
            .map(super::super::health::compact_event)
            .collect();
        let material = RecoveryMaterial {
            alert: RecoveryAlert::LongBackground,
            facts,
            workspace: &self.workspace,
            screen: &screen,
            processes: listed,
            git_status: &status,
            head: &head,
            receipt_commit: receipt.as_deref(),
            history: &history,
            allowed: &LONG_BACKGROUND_ACTIONS,
        };
        let prompt = recovery_prompt(&task, run, attempt, &material)?;
        let dir = self.run_dir.clone();
        sv.files.write(
            &dir.join(format!("recovery-prompt-{attempt}.txt")),
            prompt.as_bytes(),
        )?;
        let stdout = dir.join(format!("recovery-{attempt}.out"));
        let stderr = dir.join(format!("recovery-{attempt}.err"));
        let mut command = sv.reviewer.headless_command(&dir, &prompt, TRIAGE_TOOLS)?;
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

    /// Act on the job's verdict: apply a `repair` of high confidence whose
    /// every action holds now, and ask the inbox otherwise.
    fn act_on_recovery(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
        job: &RecoveryJob,
        verdict: std::result::Result<RecoveryVerdict, String>,
    ) -> Result<()> {
        let duration_secs = job.job.started.elapsed().as_secs();
        let verdict = match verdict {
            Ok(verdict) => verdict,
            Err(error) => {
                return self.escalate(
                    sv,
                    run,
                    job.attempt,
                    job.marker,
                    job.idle_secs,
                    Escalation::JobFailed(error),
                );
            }
        };
        if !verdict.applies() {
            return self.escalate(
                sv,
                run,
                job.attempt,
                job.marker,
                job.idle_secs,
                Escalation::Verdict(verdict),
            );
        }
        if let Err(why) = self.check_actions(sv, run, processes, &verdict.actions) {
            warn!(run_id = %run.id(), "run {}: recovery job {} answered repair, but {why}; asking the inbox", run.id(), job.attempt);
            return self.escalate(
                sv,
                run,
                job.attempt,
                job.marker,
                job.idle_secs,
                Escalation::Refused(verdict, why),
            );
        }
        let mut applied = Vec::new();
        let mut recheck_at = None;
        for action in &verdict.actions {
            match action {
                RecoveryAction::StopProcesses { pids } => {
                    let stopped = match stop_processes(sv, run, processes, pids) {
                        Ok(stopped) => stopped,
                        Err(error) => {
                            return self.escalate(
                                sv,
                                run,
                                job.attempt,
                                job.marker,
                                job.idle_secs,
                                Escalation::Refused(
                                    verdict.clone(),
                                    format!("stopping the processes failed: {error:#}"),
                                ),
                            );
                        }
                    };
                    sv.queue.record_runtime_event(
                        run.id(),
                        "auto_repaired",
                        json!({
                            "layer": "recovery",
                            "repair": action.name(),
                            "alert": RecoveryAlert::LongBackground,
                            "attempt": job.attempt,
                            "processes": stopped,
                        }),
                    )?;
                    info!(run_id = %run.id(), "run {}: recovery job {} stopped processes {pids:?} of its worktree", run.id(), job.attempt);
                }
                RecoveryAction::SendInstruction { instruction } => {
                    let text = format!(
                        "dagq: the supervisor's recovery job for run {} (its background work has run too long) asks: {}",
                        run.id(),
                        instruction.trim()
                    );
                    let sent_at = sv.files.now();
                    let workspace = self.workspace.clone();
                    let submission = submit(
                        sv,
                        run,
                        &workspace,
                        Input::Text(&text),
                        "recovery instruction",
                    )?;
                    self.stall.input_sent(sent_at);
                    self.answer_start = Some(StartCheck::new(
                        "recovery instruction",
                        &text,
                        sent_at,
                        &submission,
                    ));
                    sv.queue.record_runtime_event(
                        run.id(),
                        "auto_repaired",
                        json!({
                            "layer": "recovery",
                            "repair": action.name(),
                            "alert": RecoveryAlert::LongBackground,
                            "attempt": job.attempt,
                            "instruction": instruction,
                            "workspace_id": workspace,
                        }),
                    )?;
                }
                RecoveryAction::Wait { recheck_after_secs } => {
                    let at = sv.files.now()
                        + Duration::from_secs((*recheck_after_secs).min(MAX_RECHECK_SECS));
                    self.recovery.recheck = Some(at);
                    recheck_at = Some(millis(at));
                }
                _ => unreachable!("checked above"),
            }
            applied.push(action.name());
        }
        sv.queue.record_runtime_event(
            run.id(),
            "recovery_finished",
            json!({
                "alert": RecoveryAlert::LongBackground,
                "attempt": job.attempt,
                "verdict": verdict.verdict,
                "confidence": verdict.confidence,
                "diagnosis": verdict.diagnosis,
                "applied": applied,
                "escalated": false,
                "marker_at_ms": millis(job.marker),
                "recheck_at_ms": recheck_at,
                "duration_secs": duration_secs,
            }),
        )?;
        info!(run_id = %run.id(), "run {}: recovery job {} repaired it ({}): {}", run.id(), job.attempt, applied.join(", "), verdict.diagnosis);
        Ok(())
    }

    /// Check each action's preconditions now (ADR-0047 decision 40): only
    /// the actions of a running session's `long_background` alert, only the
    /// run's own processes, and an instruction only into a session at its
    /// prompt. The first that fails refuses the whole verdict.
    fn check_actions(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
        actions: &[RecoveryAction],
    ) -> std::result::Result<(), String> {
        for action in actions {
            match action {
                RecoveryAction::StopProcesses { pids } => {
                    if pids.is_empty() {
                        return Err("stop_processes names no pid".to_owned());
                    }
                    let own = Self::own_processes(sv, run, processes).map_err(|error| {
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
                    let ready = self.prompt_hash.is_none()
                        && sv
                            .files
                            .modified(&self.idle_marker)
                            .is_ok_and(|marker| self.stall.turn_since_input(marker));
                    if !ready {
                        return Err(
                            "the session is not idle at its prompt for an instruction".to_owned()
                        );
                    }
                }
                RecoveryAction::Wait { .. } => (),
                other => {
                    return Err(format!(
                        "{} does not apply to a running session's long_background alert (allowed: {})",
                        other.name(),
                        LONG_BACKGROUND_ACTIONS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    }

    /// Raise the alert to the inbox as a `stalled` ask with the job's
    /// diagnosis, its recommended actions and why a person is needed, and
    /// hand the ask to the [`StallWatch`].
    fn escalate(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        marker: SystemTime,
        idle_secs: i64,
        escalation: Escalation,
    ) -> Result<()> {
        let (verdict, why) = match &escalation {
            Escalation::JobFailed(error) => (None, format!("the recovery job failed: {error}")),
            Escalation::Verdict(verdict) if verdict.verdict == RecoveryDecision::Escalate => {
                (Some(verdict), "the recovery job could not repair it".to_owned())
            }
            Escalation::Verdict(verdict) => (
                Some(verdict),
                "the recovery job was not sure of its repair (confidence low), so it was not applied"
                    .to_owned(),
            ),
            Escalation::Refused(verdict, why) => (
                Some(verdict),
                format!("the runtime did not apply the recovery job's repair: {why}"),
            ),
            Escalation::UsedUp(attempts) => (
                None,
                format!(
                    "the recovery job ran {attempts} times for this alert already (at most {MAX_RECOVERY_ATTEMPTS})"
                ),
            ),
        };
        let category = match verdict.and_then(|v| v.reason_category) {
            Some(category @ (AskReason::Discard | AskReason::Scope)) => category,
            _ => AskReason::RecoveryFailed,
        };
        let mut question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has had background work running for {idle_secs}s (alert: long_background, over {threshold}s), and {why}.\nWhy a person: {category}",
            run_id = run.id(),
            task_id = run.task_id(),
            workspace = self.workspace,
            threshold = sv.stall.background_alert_secs,
            category = category.as_str(),
        );
        if let Some(verdict) = verdict {
            question.push_str(&format!("\nDiagnosis: {}", verdict.diagnosis));
            if !verdict.actions.is_empty() {
                question.push_str(&format!(
                    "\nRecommended: {}",
                    serde_json::to_string(&verdict.actions)?
                ));
            }
            if !verdict.question.trim().is_empty() {
                question.push_str(&format!("\nQuestion: {}", verdict.question.trim()));
            }
        }
        if let Some(run_dir) = run.run_dir() {
            question.push_str(&format!(
                "\nRecovery material: {run_dir}/recovery-prompt-{attempt}.txt"
            ));
        }
        question.push_str("\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, stop its background work, type an instruction; see the dagq-recover skill). This ask closes itself once the session moves on.");
        // A `stalled` ask opened meanwhile (the idle detection's) already
        // has a person looking: the diagnosis is recorded, not merged into
        // it.
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            sv.queue.record_runtime_event(
                run.id(),
                "recovery_finished",
                json!({
                    "alert": RecoveryAlert::LongBackground,
                    "attempt": attempt,
                    "verdict": verdict.map(|v| v.verdict),
                    "confidence": verdict.map(|v| v.confidence),
                    "diagnosis": verdict.map(|v| v.diagnosis.clone()),
                    "applied": [],
                    "escalated": false,
                    "outcome": "already_asked",
                    "why": why,
                    "marker_at_ms": millis(marker),
                }),
            )?;
            warn!(run_id = %run.id(), "run {}: {why}; a stalled ask is already open, so no other is asked", run.id());
            return Ok(());
        }
        let mut options: Vec<String> = STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
        for option in verdict.map(|v| v.options.as_slice()).unwrap_or_default() {
            let option = option.trim();
            if !option.is_empty() && !options.iter().any(|o| o == option) {
                options.push(option.to_owned());
            }
        }
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
                reason_category: category,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let now = sv.files.now();
        self.stall
            .escalated(id, now, idle_secs, BACKGROUND_THRESHOLD);
        sv.queue.record_runtime_event(
            run.id(),
            "recovery_finished",
            json!({
                "alert": RecoveryAlert::LongBackground,
                "attempt": attempt,
                "verdict": verdict.map(|v| v.verdict),
                "confidence": verdict.map(|v| v.confidence),
                "diagnosis": verdict.map(|v| v.diagnosis.clone()),
                "applied": [],
                "escalated": true,
                "why": why,
                "reason_category": category,
                "ask_id": id,
                "marker_at_ms": millis(marker),
            }),
        )?;
        warn!(ask_id = %id, run_id = %run.id(), "run {}: {why}; stalled ask {id} (notified: {})", run.id(), outcome["notified"]);
        Ok(())
    }
}

/// Stop `pids` (SIGTERM, then SIGKILL after [`STOP_GRACE`]), checking once
/// more right before that each is the run's own. Returns what was stopped.
fn stop_processes(
    sv: &Supervisor<'_>,
    run: &TaskRun,
    processes: &[RunProcess],
    pids: &[u32],
) -> Result<Vec<Value>> {
    let own = SessionWatch::own_processes(sv, run, processes)?;
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
