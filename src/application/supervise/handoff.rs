//! The handoff of a supervisor to another binary (ADR-0045 decision 10):
//! asked through its registration, the supervisor starts no new work, lets
//! the validations and landings in progress finish, stops its headless
//! jobs, and ends its loop so the entry point can exec the new binary under
//! the same pid. The new process keeps the token, the registration and
//! every lease, and rebuilds each run's slot the way an adoption does
//! (ADR-0039), from the queue and the run files, without writing
//! `run_adopted`. What the queue does not hold — a resumed session's
//! request, an `/exit` a rejected run waits for — goes through a
//! `handoff.json` in the run directory.

use super::*;
use serde::Deserialize;

/// The run event of each run the process that took a registration over
/// after an exec found leased to it.
pub const SUPERVISOR_HANDED_OFF: &str = "supervisor_handed_off";

/// What a run directory's `handoff.json` holds for the next process.
const SNAPSHOT: &str = "handoff.json";

impl Phase {
    /// Whether the next process can rebuild this phase from the queue and
    /// the run files: everything but a validation or a landing in progress
    /// (and a run waiting for the landing slot, which starts one), which a
    /// handoff waits for. A headless review or triage is rebuildable because
    /// it is stopped and started again.
    pub(super) fn rebuildable(&self) -> bool {
        !matches!(
            self,
            Phase::Validating(..) | Phase::AwaitingSlot | Phase::Landing(_)
        )
    }
}

/// `handoff.json`: the part of a slot the queue does not hold, written
/// before the exec by the supervisor `token`. Only that supervisor's next
/// process reads it; a file another process left behind is ignored.
#[derive(Debug, Serialize, Deserialize)]
struct Stamped {
    token: String,
    #[serde(flatten)]
    snapshot: Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Snapshot {
    /// A resumed session of a `needs_session` run.
    Resume {
        workspace: String,
        attempt: usize,
        started_at: f64,
        message: String,
        message_sent_at: Option<f64>,
        not_ready_asked: bool,
        exit_requested: bool,
        exit_for_silence: bool,
        approved: bool,
    },
    /// The `/exit` of a run that rests after its validation.
    Exit {
        workspace: Option<String>,
        resume: Option<usize>,
        close: bool,
        requested: bool,
        timed_out: bool,
        exit_asked: bool,
        exit_for_silence: bool,
    },
}

fn seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn time(seconds: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(seconds.max(0.0))
}

impl Supervisor<'_> {
    /// The last step before the exec: stop the observer and every headless
    /// job (their runs start them again), give a triaged run's lease back
    /// (the next process triages it again), and write what a resumed
    /// session or a rejected run's `/exit` needs. Returns how many runs the
    /// next process takes over.
    pub(super) fn prepare_handoff(&mut self) -> usize {
        if let Some((mode, mut child)) = self.observer.take() {
            let _ = child.kill();
            let _ = child.wait();
            info!(
                "observer ({}) stopped for the handoff; it runs again when due",
                mode.as_str()
            );
        }
        // Its row stays unfinished under this token; the next process's
        // first plan review marks it interrupted and reviews again.
        if let Some(mut watch) = self.plan_review.take() {
            watch.headless.stop();
            info!(
                "plan review {} stopped for the handoff; it runs again",
                watch.job.attempt
            );
        }
        let mut kept = 0;
        for mut slot in std::mem::take(&mut self.slots) {
            let run = slot.run.clone();
            // A live session's recovery job does not outlive this process;
            // its alert starts another once the run is rebuilt.
            stop_recovery(&mut slot);
            let snapshot = match &mut slot.phase {
                Phase::Review(watch) => {
                    watch.job.stop();
                    info!(run_id = %run.id(), "run {}: review {} stopped for the handoff; it is reviewed again", run.id(), watch.attempt);
                    None
                }
                Phase::Recovery(watch) => {
                    watch.job.stop();
                    info!(run_id = %run.id(), "run {}: recovery round {} stopped for the handoff; it is taken again", run.id(), watch.round);
                    if let Err(error) = self.queue.release_lease(run.id(), &self.token) {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
                    }
                    continue;
                }
                Phase::Resume(watch) => Some(Snapshot::Resume {
                    workspace: watch.workspace.clone(),
                    attempt: watch.attempt,
                    started_at: seconds(watch.started_at),
                    message: watch.message.clone(),
                    message_sent_at: watch.message_sent.map(|(_, at)| seconds(at)),
                    not_ready_asked: watch.not_ready_asked,
                    exit_requested: watch.exit_requested.is_some(),
                    exit_for_silence: watch.exit_for_silence,
                    approved: watch.approved,
                }),
                Phase::Exiting(watch) if run.status() != RunStatus::AwaitingIntegration => {
                    match watch.then {
                        AfterExit::Rest { close } => Some(Snapshot::Exit {
                            workspace: watch.session.as_ref().map(|s| s.workspace.clone()),
                            resume: watch.session.as_ref().and_then(|s| s.resume),
                            close,
                            requested: watch.requested.is_some(),
                            timed_out: watch.timed_out,
                            exit_asked: watch.exit_asked,
                            exit_for_silence: watch.exit_for_silence,
                        }),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(snapshot) = snapshot {
                let written = run
                    .run_dir()
                    .context("missing run directory")
                    .and_then(|dir| {
                        let text = serde_json::to_vec(&Stamped {
                            token: self.token.clone(),
                            snapshot,
                        })?;
                        self.files
                            .write(&Path::new(dir).join(SNAPSHOT), &text)
                            .map_err(Into::into)
                    });
                if let Err(error) = written {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its handoff state could not be written; the next supervisor gives its lease back: {error:#}", run.id());
                }
            }
            kept += 1;
        }
        kept
    }

    /// The first step after the exec: a slot for every run whose lease
    /// carries this process's token, rebuilt the way an adopted run's is
    /// (ADR-0039) or from its `handoff.json`. A run that cannot be rebuilt
    /// gives its lease back, so the supervisor's resume or triage (or
    /// `recover`) picks it up; one whose rebuild fails is abandoned like
    /// any other runtime error.
    pub(super) fn rebuild_own_runs(&mut self, previous_version: Option<&str>) -> Result<()> {
        for run in self.queue.runs_leased_by(&self.token)? {
            let snapshot = self.take_snapshot(&run);
            self.queue.record_runtime_event(
                run.id(),
                SUPERVISOR_HANDED_OFF,
                json!({
                    "supervisor": self.token,
                    "pid": self.layout.pid,
                    "previous_version": previous_version,
                    "version": self.layout.version,
                    "status": run.status().as_str(),
                    "state": snapshot.as_ref().map(|s| match s {
                        Snapshot::Resume { .. } => "resume",
                        Snapshot::Exit { .. } => "exit",
                    }),
                }),
            )?;
            let resumed = matches!(snapshot, Some(Snapshot::Resume { .. }));
            let phase = match (run.status(), snapshot) {
                (_, Some(snapshot)) => self.rebuild_from(&run, snapshot),
                (
                    RunStatus::Claimed
                    | RunStatus::Starting
                    | RunStatus::Running
                    | RunStatus::Validating,
                    None,
                ) => self.resume(&run),
                (RunStatus::AwaitingIntegration, None) => self.adopt_review(&run),
                (status, None) => {
                    info!(run_id = %run.id(), "run {} ({}) has nothing to take over after the handoff; its lease is given back", run.id(), status.as_str());
                    self.queue.release_lease(run.id(), &self.token)?;
                    continue;
                }
            };
            match phase {
                Ok(phase) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} taken over after the handoff ({})", run.id(), run.task_id(), run.status().as_str());
                    // A resumed session goes on watched instead of being
                    // resumed again (ADR-0047 decision 24).
                    // A record that fails is only noted: the run is taken
                    // over either way.
                    if let (true, Phase::Resume(watch)) = (resumed, &phase)
                        && let Err(error) = self.queue.record_runtime_event(
                            run.id(),
                            "auto_repaired",
                            json!({
                                "layer": "runtime",
                                "repair": "resume_adopted",
                                "conditions": {
                                    "handoff": true,
                                    "attempt": watch.attempt,
                                    "request_sent": watch.message_sent.is_some(),
                                },
                                "detail": {
                                    "workspace_id": watch.workspace,
                                    "previous_version": previous_version,
                                    "version": self.layout.version,
                                },
                            }),
                        )
                    {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
                    }
                    let mut slot = Slot::new(run, phase);
                    // Kept as it was, even past the limit (ADR-0062
                    // decision 7).
                    self.restore_waiting(&mut slot, true)?;
                    self.slots.push(slot);
                }
                Err(error) => {
                    let message = format!(
                        "run {} could not be taken over after the handoff: {error:#}",
                        run.id()
                    );
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }

    /// Read and remove the run's `handoff.json`; `None` without one or when
    /// it does not parse.
    fn take_snapshot(&self, run: &TaskRun) -> Option<Snapshot> {
        let path = Path::new(run.run_dir()?).join(SNAPSHOT);
        if !self.files.is_file(&path) {
            return None;
        }
        let text = self.files.read(&path).ok();
        let _ = self.files.remove_file(&path);
        text.and_then(|text| serde_json::from_slice::<Stamped>(&text).ok())
            .filter(|stamped| stamped.token == self.token)
            .map(|stamped| stamped.snapshot)
    }

    fn rebuild_from(&mut self, run: &TaskRun, snapshot: Snapshot) -> Result<Phase> {
        Ok(match snapshot {
            Snapshot::Resume {
                workspace,
                attempt,
                started_at,
                message,
                message_sent_at,
                not_ready_asked,
                exit_requested,
                exit_for_silence,
                approved,
            } => {
                let task = self.queue.show(run.task_id())?.task;
                let now = Instant::now();
                // Answers and dialogs are followed from the request on; an
                // answer typed since closed its ask, which moves the last
                // input on (ADR-0071 decision 17).
                let live = Box::new(SessionWatch::fixing(
                    run,
                    &workspace,
                    time(message_sent_at.unwrap_or(started_at)),
                )?);
                Phase::Resume(ResumeWatch {
                    live,
                    workspace,
                    attempt,
                    run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
                    receipt_path: PathBuf::from(
                        run.receipt_path().context("missing receipt path")?,
                    ),
                    idle_marker: run.idle_marker_path()?,
                    started_at: time(started_at),
                    startup: now,
                    message,
                    agent_seen: None,
                    ready_since: None,
                    not_ready_asked,
                    // The resume timeout runs from the send, not the exec.
                    message_sent: message_sent_at.map(|at| {
                        let at = time(at);
                        let ago = self.files.now().duration_since(at).unwrap_or_default();
                        (now.checked_sub(ago).unwrap_or(now), at)
                    }),
                    // Whether the session took the request is not checked
                    // again, as for an adopted revise request.
                    start: None,
                    // Never a second /exit; its timeout restarts now.
                    exit_requested: exit_requested.then_some(now),
                    // Whether that /exit was typed is not handed over: its
                    // "Background work is running" dialog is left to the
                    // stuck_exit ask (ADR-0047 decision 29).
                    exit_typed: false,
                    required_evidence: task.required_evidence().to_vec(),
                    approved,
                    silent: false,
                    exit_for_silence,
                    stale: adopted_stale_nudge(&*self.queue, run, RESUME_PHASE, Some(attempt))?,
                    // A recovery job the previous process ran is gone: the
                    // exit timeout starts another (counted as an attempt).
                    recovery: RecoveryWatch::default(),
                })
            }
            Snapshot::Exit {
                workspace,
                resume,
                close,
                requested,
                timed_out,
                exit_asked,
                exit_for_silence,
            } => {
                let session = workspace.map(|workspace| SessionRef { workspace, resume });
                let mut watch = ExitWatch::new(session, AfterExit::Rest { close });
                watch.requested = requested.then(Instant::now);
                watch.timed_out = timed_out;
                watch.exit_asked = exit_asked;
                watch.exit_for_silence = exit_for_silence;
                Phase::Exiting(watch)
            }
        })
    }
}
