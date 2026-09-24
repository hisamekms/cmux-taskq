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
            then,
        }
    }

    /// Where the run stands while its session holds the `/exit` back, and
    /// what follows once it exits: the `stuck_exit` question's sentence.
    pub(super) fn after(&self, run: &TaskRun) -> String {
        let next = match &self.then {
            AfterExit::Land => "lands on main",
            AfterExit::Ask { .. } => "opens an approve_landing ask for the person",
            AfterExit::ReviewFailed { .. } => "waits for a review by hand",
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

    /// Whether the session is gone (or there was none). A session that
    /// exited has its `stuck_exit` asks closed.
    pub(super) fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        let Some(session) = &self.session else {
            return Ok(true);
        };
        let processes = sv.queue.processes(run.id())?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        let Some(wrapper) = wrapper.filter(|w| w.exited_at.is_none()) else {
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
                        json!({"error": format!("{error:#}")}),
                    )?,
                }
            }
            // Nobody needs to send /exit to a session that exited.
            for ask in sv
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                sv.log.note(&format!(
                    "session of {} exited; closed its stuck_exit ask {}",
                    run.id(),
                    ask.id
                ));
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
                    sv.log.note(&format!(
                        "session of {} has background work running; /exit waits for it",
                        run.id()
                    ));
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
                sv.cmux.send_exit(&session.workspace)?;
                sv.log.note(&format!(
                    "exit requested for {}; waiting for session exit",
                    run.id()
                ));
                self.requested = Some(Instant::now());
                self.exit_for_silence = matches!(pulse, WrapperPulse::Silent);
            }
            Some(requested) if !self.timed_out && requested.elapsed() >= sv.cmux.exit_timeout() => {
                let timeout = sv.cmux.exit_timeout();
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_request_timed_out",
                    json!({"workspace_id": session.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                sv.log.note(&format!(
                    "session for {} did not exit within {}s of the exit request; keeping the run and asking the inbox to send /exit in workspace {}",
                    run.id(),
                    timeout.as_secs(),
                    session.workspace
                ));
                self.timed_out = true;
            }
            Some(_) => (),
        }
        if self.timed_out && !self.exit_asked {
            let workspace = session.workspace.clone();
            ask_stuck_exit(sv, run, &workspace, &self.after(run))?;
            self.exit_asked = true;
        }
        Ok(false)
    }
}

/// Raise a session that held `/exit` back as a `stuck_exit` ask to the
/// inbox, with the last lines of its screen, through the ask path that
/// notifies once when the ask is new (ADR-0022 decision 5). An open ask of
/// the run is not registered twice. A screen that cannot be read leaves the
/// ask without an excerpt. `after` says where the run stands and what
/// follows once the session exits: a `running` run goes on to validating,
/// one the supervisor holds after its review (ADR-0027) to its landing, its
/// ask or its rest. The inbox shows the ask to the person, who acts on the
/// answer through it (the `dagq-recover` skill).
pub(super) fn ask_stuck_exit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    after: &str,
) -> Result<()> {
    let screen = match sv.cmux.capture(workspace) {
        Ok(screen) => sv.signals.screen_excerpt(&screen),
        Err(error) => format!("(the screen could not be read: {error:#})"),
    };
    let question = format!(
        "The session of run {run_id} (task {task_id}) did not exit within {timeout}s of the supervisor's /exit (exit_request_timed_out): something on its screen, usually one of Claude Code's own dialogs such as \"Background work is running\", holds the exit back. {after}; this ask then closes itself. Answer `exit` to have the dialog answered so that the session exits and /exit sent in workspace {workspace}, or `wait` to leave the session as it is (or write what to do instead).\n\nLast lines of the screen:\n{screen}",
        run_id = run.id(),
        task_id = run.task_id(),
        timeout = sv.cmux.exit_timeout().as_secs(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::StuckExit,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: vec!["exit".into(), "wait".into()],
            asked_by: SessionRole::Supervisor.as_str().into(),
        },
        sv.cmux,
    )?;
    sv.log.note(&format!(
        "stuck_exit ask {} for {} (notified: {})",
        outcome["id"],
        run.id(),
        outcome["notified"]
    ));
    Ok(())
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

/// `Silent` also records `wrapper_heartbeat_expired` once per watch (`noted`)
/// and logs it. A wrapper whose heartbeat expired and whose process is gone
/// is an error with `message`, as before: nothing is left to ask to exit,
/// and the run is given up to `recover`; a `stuck_exit` ask the silence
/// raised is closed then, since no session is left to exit. The row is read
/// again first, so a wrapper that recorded its exit just before it died is
/// `Exited`, not an error.
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
                sv.log.note(&format!(
                    "wrapper of {} died without recording its exit; closed its stuck_exit ask {}",
                    run.id(),
                    ask.id
                ));
            }
        }
        bail!("{message}");
    }
    if !*noted {
        sv.queue.record_runtime_event(
            run.id(),
            "wrapper_heartbeat_expired",
            json!({"pid": wrapper.pid, "heartbeat_age_secs": age, "workspace_id": workspace}),
        )?;
        sv.log.note(&format!(
            "wrapper of {} (pid {}) stopped heartbeating {age}s ago but its process is alive; asking its session in workspace {workspace} to exit",
            run.id(),
            wrapper.pid
        ));
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
