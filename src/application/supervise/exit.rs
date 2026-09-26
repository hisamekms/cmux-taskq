//! The session's `/exit` ([`ExitWatch`]) and a session that holds it
//! back: a silent wrapper and the `stuck_exit` ask.

use super::*;

/// Asks the run's session to `/exit` once (unless it ended already) and
/// waits for its wrapper to exit; then the supervisor closes the workspace
/// and does `then`. The `/exit` waits while the idle marker shows background
/// work running (task 147), for at most the resume timeout. A session that
/// holds the `/exit` back past the exit timeout is recorded as
/// `exit_request_timed_out` and waited for, keeping the lease, as before
/// (ADR-0027 leaves it unchanged).
pub(super) struct ExitWatch {
    pub(super) session: Option<SessionRef>,
    /// When the watch began, for the wait on background work.
    pub(super) since: Instant,
    /// The wait on background work is logged.
    pub(super) background_noted: bool,
    pub(super) requested: Option<Instant>,
    pub(super) timed_out: bool,
    /// The `stuck_exit` ask of the exit timeout is registered (also by a
    /// previous supervisor), as for a running run's session (task 104).
    pub(super) exit_asked: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
    /// Why a `/exit` that never reached the session (task 354) could not
    /// close and land instead: the `stuck_exit` ask says so first.
    pub(super) unsent: Option<String>,
    /// The recovery job of a session that holds the `/exit` back
    /// (`stuck_exit`, ADR-0047 decision 39).
    pub(super) recovery: RecoveryWatch,
    pub(super) then: AfterExit,
}

impl ExitWatch {
    pub(super) fn new(session: Option<SessionRef>, then: AfterExit) -> Self {
        Self {
            session,
            since: Instant::now(),
            background_noted: false,
            requested: None,
            timed_out: false,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
            unsent: None,
            recovery: RecoveryWatch::default(),
            then,
        }
    }

    /// Where the run stands while its session holds the `/exit` back, and
    /// what follows once it exits: the `stuck_exit` question's sentence.
    pub(super) fn after(&self, run: &TaskRun) -> String {
        let next = match &self.then {
            AfterExit::Land => "lands on main",
            AfterExit::Ask { .. } => "opens an approve_landing ask for the person",
            AfterExit::ReviewFailed { .. } => {
                "opens an approve_landing ask for the person about its failed review"
            }
            AfterExit::Rest { close: true } if run.status() == RunStatus::AwaitingIntegration => {
                "waits for the answer to its approve_landing ask"
            }
            AfterExit::Rest { close: true } => "is resumed in a session of its own",
            AfterExit::Rest { close: false } => "is left to the person",
        };
        stuck_exit_after(
            self.exit_for_silence,
            &format!(
                "The run stays {} under the supervisor after its validation and review, and {next} once the session exits",
                run.status().as_str()
            ),
        )
    }

    /// Whether the session is gone (or there was none): it exited, or its
    /// wrapper died without recording its exit. A session that is gone has
    /// its `stuck_exit` asks closed.
    pub(super) fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        let Some(session) = self.session.clone() else {
            return Ok(true);
        };
        let processes = sv.queue.processes(run.id())?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        let now = sv.generators.clock.now();
        // A wrapper that died without recording its exit left no session to
        // ask (task 236): the run goes on as if it had exited.
        let Some(wrapper) = wrapper.filter(|w| w.exited_at.is_none() && !wrapper_dead(sv, w, now))
        else {
            if self.requested.is_some() {
                let name = match session.resume {
                    Some(attempt) => format!("terminal-resume-{attempt}.txt"),
                    None => "terminal-final.txt".to_owned(),
                };
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                match sv.cmux.capture(&session.workspace) {
                    Ok(screen) => sv.files.write(&run_dir.join(name), screen.as_bytes())?,
                    Err(error) => sv.queue.record_runtime_event(
                        run.id(),
                        "screen_capture_failed",
                        reason_of_error(&error, ReasonCode::BackendFailed)
                            .on(json!({"error": format!("{error:#}")})),
                    )?,
                }
            }
            // Nobody needs to send /exit to a session that exited, nor
            // anything else.
            self.recovery.stop(sv, run);
            close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
            for ask in sv
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                info!(run_id = %run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", run.id(), ask.id);
            }
            return Ok(true);
        };
        // A silent wrapper's session gets the same single /exit.
        let pulse = wrapper_pulse(
            sv,
            run,
            wrapper,
            &session.workspace,
            &mut self.silent,
            "wrapper heartbeat expired; session may still be alive",
        )?;
        if matches!(pulse, WrapperPulse::Exited) {
            return Ok(false);
        }
        match self.requested {
            None if self.since.elapsed() < sv.cmux.resume_timeout()
                && background_running(&*sv.files, sv.signals, &run.idle_marker_path()?)? =>
            {
                // A /exit now would stop at the "Background work is
                // running" dialog; Claude Code takes the turn up again when
                // the work ends and writes a marker without it.
                if !self.background_noted {
                    self.background_noted = true;
                    info!(run_id = %run.id(), "session of {} has background work running; /exit waits for it", run.id());
                }
            }
            None => {
                // Recorded before sending: the session may exit, and its
                // wrapper record `session_exited`, before the send returns.
                let timeout = sv.cmux.exit_timeout();
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_requested",
                    json!({"workspace_id": session.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                // Ask once, the way a person would; never kill the session.
                let workspace = session.workspace.clone();
                let submission = submit(sv, run, &workspace, Input::Exit, "/exit")?;
                self.requested = Some(Instant::now());
                self.exit_for_silence = matches!(pulse, WrapperPulse::Silent);
                if submission == Submission::Unsent {
                    // Waiting out the exit timeout would not help: the
                    // session was never asked.
                    if self.unsent(sv, run, &workspace)? {
                        return Ok(true);
                    }
                } else {
                    info!(run_id = %run.id(), "exit requested for {}; waiting for session exit", run.id());
                }
            }
            Some(requested) if !self.timed_out && requested.elapsed() >= sv.cmux.exit_timeout() => {
                // A known dialog answered by rule gets the exit timeout
                // again to let the session go (ADR-0047 decision 29).
                let workspace = session.workspace.clone();
                if answer_exit_dialog(sv, run, &workspace, true)? {
                    self.requested = Some(Instant::now());
                    return Ok(false);
                }
                let timeout = sv.cmux.exit_timeout();
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_request_timed_out",
                    json!({"code": ReasonCode::ExitTimeout, "workspace_id": session.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                warn!(run_id = %run.id(), "session for {} did not exit within {}s of the exit request in workspace {}; keeping the run for its recovery job", run.id(), timeout.as_secs(), session.workspace);
                self.timed_out = true;
            }
            Some(_) => (),
        }
        if self.timed_out && !self.exit_asked {
            return self.recover(sv, run, &session.workspace);
        }
        Ok(false)
    }

    /// The session holds its `/exit` back (or the `/exit` never reached
    /// it): its recovery job (`stuck_exit`, ADR-0047 decision 39), and the
    /// `stuck_exit` ask once it escalates. A repair that answered a dialog
    /// or stopped processes gives the session the exit timeout again; one
    /// that closed the workspace of a run that lands goes on as if the
    /// session had exited (`true`).
    fn recover(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, workspace: &str) -> Result<bool> {
        let lands = matches!(self.then, AfterExit::Land);
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let live = Live {
            workspace,
            run_dir: &run_dir,
            allowed: if lands {
                &STUCK_EXIT_ACTIONS
            } else {
                &STUCK_EXIT_HELD_ACTIONS
            },
            exit_typed: self.requested.is_some() && self.unsent.is_none(),
            at_prompt: false,
            lands,
        };
        let timeout = sv.cmux.exit_timeout().as_secs();
        let unsent = self.unsent.clone();
        let step = self
            .recovery
            .follow(sv, run, &live, RecoveryAlert::StuckExit, || {
                json!({"timeout_secs": timeout, "unsent": unsent, "then": if lands { "land" } else { "rest" }})
            })?;
        match step {
            LiveStep::Pending => Ok(false),
            // A person recovers it by hand from the attention: asked.
            LiveStep::Failed => {
                self.exit_asked = true;
                Ok(false)
            }
            LiveStep::Repaired(applied) if applied.closed => {
                match self.session.take().and_then(|session| session.resume) {
                    None => {
                        sv.queue.workspace_closed(run.id(), &sv.token)?;
                    }
                    Some(attempt) => sv.queue.record_runtime_event(
                        run.id(),
                        "workspace_closed",
                        json!({"workspace_id": workspace, "resume_attempt": attempt}),
                    )?,
                }
                close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
                info!(run_id = %run.id(), "the recovery job closed workspace {workspace} of {}, whose receipt holds against its clean worktree; it goes on to land", run.id());
                Ok(true)
            }
            LiveStep::Repaired(applied) => {
                if applied.exit_again {
                    self.requested = Some(Instant::now());
                    self.timed_out = false;
                }
                Ok(false)
            }
            LiveStep::Escalate(attempt, escalation) => {
                let note = escalation.note(run, RecoveryAlert::StuckExit, attempt);
                let after = match &self.unsent {
                    Some(why) => format!("{EXIT_UNSENT} ({why}). {}", self.after(run)),
                    None => self.after(run),
                };
                let id = ask_stuck_exit(sv, run, workspace, &after, Some(&note))?;
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
                Ok(false)
            }
        }
    }
}

impl ExitWatch {
    /// A `/exit` that cmux timed out on every attempt without it reaching
    /// the session (task 354). A run whose landing is safe without the
    /// session's exit (see [`landable_without_exit`]) has its workspace
    /// closed here, which ends the session, and goes on as if its session
    /// had exited: `true`, and the supervisor lands it. Any other run, or
    /// one whose workspace cannot be closed, is the `stuck_exit` ask, as a
    /// session that held its `/exit` back is. Either way `exit_unsent`
    /// records it.
    fn unsent(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, workspace: &str) -> Result<bool> {
        let mut held = match &self.then {
            AfterExit::Land => landable_without_exit(sv, run),
            _ => Some("the run does not land after its exit".to_owned()),
        };
        if held.is_none()
            && let Err(error) = sv.cmux.close(workspace)
        {
            held = Some(format!("its workspace could not be closed: {error:#}"));
        }
        let mut payload = json!({
            "code": ReasonCode::BackendTimeout,
            "workspace_id": workspace,
            "attempts": sv.cmux.call_attempts().max(1),
            "action": if held.is_none() { "close_and_land" } else { "recover" },
        });
        if let Some(why) = &held {
            payload["held"] = json!(why);
        }
        sv.queue
            .record_runtime_event(run.id(), "exit_unsent", payload)?;
        let Some(why) = held else {
            // Closed above: the supervisor closes nothing more, and a
            // dialog ask of the session is closed as for one that exited.
            match self.session.take().and_then(|session| session.resume) {
                None => {
                    sv.queue.workspace_closed(run.id(), &sv.token)?;
                }
                Some(attempt) => sv.queue.record_runtime_event(
                    run.id(),
                    "workspace_closed",
                    json!({"workspace_id": workspace, "resume_attempt": attempt}),
                )?,
            }
            close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
            // Closing the workspace to land is a repair (ADR-0047 decisions
            // 25 and 38); the workspace is closed, so a record that fails is
            // only noted.
            if let Err(error) = sv.queue.record_runtime_event(
                run.id(),
                "auto_repaired",
                json!({
                    "layer": "runtime",
                    "repair": "exit_forced_close",
                    "conditions": {
                        "cause": ReasonCode::BackendTimeout,
                        "attempts": sv.cmux.call_attempts().max(1),
                        "exit_reached": false,
                        "then": "land",
                        "review": "pass",
                        "receipt_holds": true,
                    },
                    "detail": {"workspace_id": workspace},
                }),
            ) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
            }
            info!(run_id = %run.id(), "/exit could not be sent to {} in workspace {workspace}; it lands and its receipt still holds against its clean worktree, so its workspace was closed and it goes on to land", run.id());
            return Ok(true);
        };
        warn!(run_id = %run.id(), "/exit could not be sent to {} in workspace {workspace} and the run cannot land without its exit ({why}); its recovery job looks at it", run.id());
        let timeout = sv.cmux.exit_timeout();
        sv.queue.record_runtime_event(
            run.id(),
            "exit_request_timed_out",
            json!({"code": ReasonCode::ExitTimeout, "workspace_id": workspace, "timeout_secs": timeout.as_secs(), "unsent": true}),
        )?;
        self.timed_out = true;
        self.unsent = Some(why);
        self.recover(sv, run, workspace)
    }
}

/// What a `stuck_exit` ask says first when the `/exit` never got there.
pub(super) const EXIT_UNSENT: &str = "The supervisor's /exit timed out in cmux on every attempt and its screen showed each time that it had not reached the session (exit_unsent), so the session was not asked to exit and the run cannot land without it";

/// Why `run` cannot land without its session's exit, `None` when it can:
/// its receipt still stands against its worktree (the commit it names is
/// the head of the run branch checked out there, the worktree is clean, and
/// the evidence and scope hold, as validation checked) and that head is the
/// commit the review passed. A check that fails to run holds it too.
pub(super) fn landable_without_exit(sv: &mut Supervisor<'_>, run: &TaskRun) -> Option<String> {
    let checked = sv
        .queue
        .show(run.task_id())
        .and_then(|detail| check_receipt(&*sv.repository, &*sv.files, &detail.task, run));
    match checked {
        Ok(Ok((_, commit))) => match run.result_commit() {
            Some(reviewed) if *reviewed == commit => None,
            Some(reviewed) => Some(format!(
                "the head {commit} is not the reviewed commit {reviewed}"
            )),
            None => Some("the run has no reviewed commit".to_owned()),
        },
        Ok(Err(rejection)) => Some(format!("its receipt no longer holds: {}", rejection.reason)),
        Err(error) => Some(format!("its receipt could not be checked: {error:#}")),
    }
}

/// Raise a session that held `/exit` back as a `stuck_exit` ask to the
/// inbox once its recovery job escalated (ADR-0047 decision 40), with the
/// job's `note` and the last lines of its screen, through the ask path that
/// notifies once when the ask is new (ADR-0022 decision 5); the options are
/// `exit` and `wait` and the job's, the reason category the job's. An open
/// ask of the run is not registered twice. A screen that cannot be read
/// leaves the ask without an excerpt. `after` says where the run stands and
/// what follows once the session exits: a `running` run goes on to
/// validating, one the supervisor holds after its review (ADR-0027) to its
/// landing, its ask or its rest. The inbox shows the ask to the person, who
/// acts on the answer through it (the `dagq-recover` skill).
pub(super) fn ask_stuck_exit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    after: &str,
    note: Option<&Note>,
) -> Result<AskId> {
    let screen = match sv.cmux.capture(workspace) {
        Ok(screen) => sv.signals.screen_excerpt(&screen),
        Err(error) => format!("(the screen could not be read: {error:#})"),
    };
    let recovery = note.map_or_else(String::new, |note| {
        format!(
            "\n\nIts recovery job looked first, and {}.\n{}",
            note.why, note.text
        )
    });
    let question = format!(
        "The session of run {run_id} (task {task_id}) did not exit within {timeout}s of the supervisor's /exit (exit_request_timed_out): something on its screen, usually one of Claude Code's own dialogs such as \"Background work is running\", holds the exit back. {after}; this ask then closes itself. Answer `exit` to have the dialog answered so that the session exits and /exit sent in workspace {workspace}, or `wait` to leave the session as it is (or write what to do instead).{recovery}\n\nLast lines of the screen:\n{screen}",
        run_id = run.id(),
        task_id = run.task_id(),
        timeout = sv.cmux.exit_timeout().as_secs(),
    );
    let mut options: Vec<String> = vec!["exit".into(), "wait".into()];
    for option in note.map(|note| note.options.as_slice()).unwrap_or_default() {
        if !options.contains(option) {
            options.push(option.clone());
        }
    }
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::StuckExit,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options,
            asked_by: SessionRole::Supervisor.as_str().into(),
            reason_category: note.map_or(AskReason::RecoveryFailed, |note| note.category),
            finding_id: None,
        },
        sv.cmux,
    )?;
    info!(ask_id = %outcome["id"], run_id = %run.id(), "stuck_exit ask {} for {} (notified: {})", outcome["id"], run.id(), outcome["notified"]);
    Ok(AskId::new(
        outcome["id"].as_i64().context("ask returned no id")?,
    ))
}

/// How a registered wrapper that has not recorded its exit stands. Its
/// heartbeat is the supervisor's sign of life, but a wrapper whose
/// heartbeat stopped while its process lives on (a heartbeat that fails
/// against the queue, a stall) still holds a live session: waiting for
/// its exit alone left such sessions running for hours.
pub(super) enum WrapperPulse {
    Fresh,
    /// The heartbeat expired while the wrapper's process is alive: the
    /// session is asked to `/exit` the way a finished one is, and a
    /// `stuck_exit` ask follows when it does not.
    Silent,
    /// The wrapper recorded its exit after this poll read its row: the next
    /// poll handles the exit.
    Exited,
}

/// `Silent` also records `wrapper_heartbeat_expired` once per silence
/// (`noted`) and logs it. `Fresh` leaves `noted` as it is: a watch that has
/// not sent its `/exit` clears it itself (task 606), so that its session
/// may wait again and a later silence is recorded again, while one past its
/// `/exit` keeps it for the `stuck_exit` ask closed below. A wrapper whose
/// heartbeat expired and whose process is gone is an error with `message`,
/// as before: nothing is left to ask to exit, and the run is given up to
/// `recover`; a `stuck_exit` ask the silence raised is closed then, since
/// no session is left to exit. The row is read again first, so a wrapper
/// that recorded its exit just before it died is `Exited`, not an error.
pub(super) fn wrapper_pulse(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    noted: &mut bool,
    message: &str,
) -> Result<WrapperPulse> {
    let age = sv.generators.clock.now() - wrapper.heartbeat_at;
    if age <= HEARTBEAT_TIMEOUT_SECS {
        return Ok(WrapperPulse::Fresh);
    }
    if !sv.processes.alive(wrapper.pid) {
        let exited = sv
            .queue
            .processes(run.id())?
            .iter()
            .any(|p| p.role == "wrapper" && p.pid == wrapper.pid && p.exited_at.is_some());
        if exited {
            return Ok(WrapperPulse::Exited);
        }
        if *noted {
            for ask in sv
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                info!(run_id = %run.id(), ask_id = %ask.id, "wrapper of {} died without recording its exit; closed its stuck_exit ask {}", run.id(), ask.id);
            }
        }
        bail!("{message}");
    }
    if !*noted {
        sv.queue.record_runtime_event(
            run.id(),
            "wrapper_heartbeat_expired",
            json!({"code": ReasonCode::HeartbeatLost, "pid": wrapper.pid, "heartbeat_age_secs": age, "workspace_id": workspace}),
        )?;
        info!(run_id = %run.id(), "wrapper of {} (pid {}) stopped heartbeating {age}s ago but its process is alive; asking its session in workspace {workspace} to exit", run.id(), wrapper.pid);
        *noted = true;
    }
    Ok(WrapperPulse::Silent)
}

/// What a `stuck_exit` ask says first when the `/exit` was sent because the
/// wrapper went silent, not because the session finished (a silence that
/// began after the `/exit` does not change why it was sent).
pub(super) const SILENT_WRAPPER_EXIT: &str = "Its wrapper stopped heartbeating while its process lived on (wrapper_heartbeat_expired), so the supervisor sent the /exit";

/// `after` for a `stuck_exit` ask, led by [`SILENT_WRAPPER_EXIT`] when the
/// `/exit` was sent because the wrapper went silent.
pub(super) fn stuck_exit_after(silent: bool, after: &str) -> String {
    if silent {
        format!("{SILENT_WRAPPER_EXIT}. {after}")
    } else {
        after.to_owned()
    }
}

/// The answer the runtime writes into an open `stuck_exit` ask it closes.
pub(super) const STUCK_EXIT_CLOSED: &str = "the session exited; closed by the runtime";
