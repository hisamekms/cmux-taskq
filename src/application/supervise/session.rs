//! A claimed run's worker session: its provisioning, the [`SessionWatch`]
//! of its wrapper, receipt, idle marker and dialogs, and the answers to
//! its `worker_question` asks.

use super::*;

impl Supervisor<'_> {
    /// Start the validation of `run` on a thread (see [`spawn_validation`]).
    pub(super) fn validate(&self, run: TaskRun) -> thread::JoinHandle<Result<Validation>> {
        spawn_validation(
            self.queues.clone(),
            self.repository.clone(),
            self.files.clone(),
            run,
        )
    }
    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
    /// The queue's workspace group, asked for with every run workspace:
    /// the call is idempotent by external ID, and cmux removes a group whose
    /// last workspace closes, so a handle kept from an earlier run could
    /// name a group that is gone. A group cmux cannot make is a warning in
    /// the log, and the run opens outside it.
    pub(super) fn workspace_group(&self) -> Option<String> {
        let name = workspace_group_name(&self.layout.repo_root);
        match self.cmux.ensure_group(&self.layout.queue_hash, &name) {
            Ok(group) => Some(group),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "warning: cmux workspace group {name:?} (external ID {}) could not be made, \
so the run workspace opens outside it: {error:#}", self.layout.queue_hash);
                None
            }
        }
    }
    pub(super) fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = &self.layout.runs_dir;
        let paths = RunPaths::new(state_dir, claimed.id());
        let run_dir = paths.run_dir.clone();
        let plan = RunPlan {
            repo_path: path_text(&self.layout.repo_root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("dagq/{}", claimed.id()),
            worktree_path: path_text(&paths.worktree)?,
            receipt_path: path_text(&paths.receipt)?,
            log_path: path_text(&paths.log)?,
        };
        // Save intended paths before any external resource is created.
        self.queue.plan_run(claimed.id(), &self.token, &plan)?;
        self.files.create_dir_all(state_dir)?;
        self.files
            .create_new_dir(&run_dir)
            .context("run directory must be new")?;
        let run_env = self.verifier.run_env(&run_dir)?;
        let run = self.queue.run(claimed.id())?;
        let task = self.queue.show(run.task_id())?.task;
        let predecessors: Vec<PredecessorSummary> = self
            .queue
            .predecessors(task.id())?
            .iter()
            .map(|predecessor| PredecessorSummary::from_predecessor(&*self.files, predecessor))
            .collect();
        let goal_predecessors: Vec<GoalPredecessorSummary> = self
            .queue
            .goal_predecessors(task.id())?
            .iter()
            .map(|goal| GoalPredecessorSummary::from_goal_predecessor(&*self.files, goal))
            .collect();
        let goal = match task.goal_id() {
            Some(goal_id) => Some(self.queue.show_goal(goal_id)?.goal),
            None => None,
        };
        let siblings = siblings_in_progress(&task, self.queue.tasks_in_progress()?);
        self.files.write(
            &run_dir.join("prompt.txt"),
            prompt(
                &task,
                &run,
                goal.as_ref(),
                &predecessors,
                &goal_predecessors,
                &siblings,
            )?
            .as_bytes(),
        )?;
        // A running wrapper must not change when the development binary is rebuilt.
        self.files
            .copy(&self.layout.runner, &run_dir.join("runner"))
            .context("snapshot runtime binary")?;
        let git_output = self.repository.create_worktree(&run)?;
        self.files
            .write(&run_dir.join("worktree-create.txt"), git_output.as_bytes())?;
        self.queue.record_runtime_event(
            run.id(),
            "worktree_created",
            json!({"path": plan.worktree_path, "branch": plan.branch}),
        )?;
        let command = shell_join(&[
            path_text(&run_dir.join("runner"))?,
            "--db".into(),
            path_text(&self.layout.db)?,
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            self.token.clone(),
            "--claude".into(),
            path_text(&self.layout.claude)?,
        ]);
        let mut env = self.layout.worker_env.clone();
        env.extend(run_env);
        let tags = WorkspaceTags {
            env,
            description: Some(workspace_description(
                SessionRole::Worker,
                &self.layout.queue_hash,
                Some(run.id()),
                Some(run.task_id()),
            )),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create(&task, &run, &command, &tags)?;
        self.queue
            .workspace_created(run.id(), &self.token, &workspace)?;
        info!(task_id = %run.task_id(), run_id = %run.id(), "task {} running in workspace {}; run {}", run.task_id(), workspace, run.id());
        Ok(SessionWatch {
            workspace,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            receipt_seen_at: None,
            exit_requested: None,
            exit_timed_out: false,
            first_commit_seen: false,
            agent_seen: None,
            prompt_checked: None,
            prompt_hash: None,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
            answer_start: None,
            stall: StallWatch::default(),
            recovery: RecoveryWatch::default(),
        })
    }
}

/// Watches one session: wrapper registration and heartbeat, receipt and idle
/// marker, the single exit request, and the wrapper's exit.
pub(super) struct SessionWatch {
    pub(super) workspace: String,
    pub(super) run_dir: PathBuf,
    pub(super) receipt_path: PathBuf,
    pub(super) idle_marker: PathBuf,
    pub(super) startup: Instant,
    pub(super) receipt_seen: bool,
    /// When this supervisor first saw the receipt, for the wait on
    /// background work the session left running after it.
    pub(super) receipt_seen_at: Option<Instant>,
    pub(super) exit_requested: Option<Instant>,
    /// `exit_request_timed_out` is recorded once per run; the lease is kept.
    pub(super) exit_timed_out: bool,
    /// `first_commit_observed` is recorded (also by a previous supervisor).
    pub(super) first_commit_seen: bool,
    /// When this supervisor first saw the agent registered.
    pub(super) agent_seen: Option<Instant>,
    /// When the screen was last read for a dialog.
    pub(super) prompt_checked: Option<Instant>,
    /// `screen_hash` of the dialog last recorded as `prompt_waiting` and not
    /// cleared since.
    pub(super) prompt_hash: Option<String>,
    /// The `stuck_exit` ask of the exit timeout is registered (also by a
    /// previous supervisor).
    pub(super) exit_asked: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
    /// Whether the session took the last answer delivered or the nudge
    /// (task 285).
    pub(super) answer_start: Option<StartCheck>,
    /// Idle without a receipt: the nudge and the `stalled` ask (ADR-0043
    /// decision 1).
    pub(super) stall: StallWatch,
    /// Background work past its threshold: the recovery job (ADR-0047
    /// decision 39).
    pub(super) recovery: RecoveryWatch,
}

impl SessionWatch {
    /// One observation. `Some` once supervision finished (`validating` or
    /// `failed`): the wrapper exited, or the session went idle after its
    /// receipt and stays open for the review; an error means the run must
    /// be retained. An `/exit` an earlier supervisor already requested is
    /// waited out as before.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<TaskRun>> {
        let processes = sv.queue.processes(run.id())?;
        self.watch_first_commit(sv, run)?;
        if !self.receipt_seen && sv.files.is_file(&self.receipt_path) {
            self.receipt_seen = true;
            self.receipt_seen_at = Some(Instant::now());
            sv.queue.record_runtime_event(
                run.id(),
                "receipt_observed",
                json!({"path": path_text(&self.receipt_path)?, "validated": false}),
            )?;
            info!(run_id = %run.id(), "receipt received for {}; waiting for the session to go idle (or a person's /exit)", run.id());
        }
        self.stall.settle(sv, run, self.receipt_seen)?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        // A session that already ended (on its own, by a person's /exit,
        // or before this supervisor adopted the run) is not asked to exit.
        let session_ended = wrapper.is_some_and(|w| w.exited_at.is_some());
        // Background work the session left running after its receipt is
        // waited for up to the resume timeout, like a resumed session's:
        // work that never ends must not hold the run without an attention.
        // Past it the run goes on, and a /exit held back by the dialog
        // becomes a stuck_exit ask.
        let waited_out = self
            .receipt_seen_at
            .is_some_and(|at| at.elapsed() >= sv.cmux.resume_timeout());
        if self.receipt_seen
            && self.exit_requested.is_none()
            && !session_ended
            && let Some(evidence) =
                match IdleMarker::read(&*sv.files, sv.signals, &self.idle_marker)? {
                    Some(idle) if waited_out => {
                        idle.stopped_after_receipt(&*sv.files, &self.receipt_path)?
                    }
                    Some(idle) => idle.idle_after_receipt(&*sv.files, &self.receipt_path)?,
                    None => None,
                }
        {
            sv.queue
                .record_runtime_event(run.id(), "session_idle_observed", evidence)?;
            // The session stays open through validation and review, and
            // is asked to exit only once the verdict is known (ADR-0027
            // decision 1).
            info!(run_id = %run.id(), "session of {} is idle after its receipt; validating with the session open", run.id());
            self.recovery.stop(sv, run);
            return sv
                .queue
                .finish_supervision_live(run.id(), &sv.token)
                .map(Some);
        }
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                match sv.cmux.capture(&self.workspace) {
                    Ok(screen) => sv
                        .files
                        .write(&self.run_dir.join("terminal-final.txt"), screen.as_bytes())?,
                    Err(error) => sv.queue.record_runtime_event(
                        run.id(),
                        "screen_capture_failed",
                        reason_of_error(&error, ReasonCode::BackendFailed)
                            .on(json!({"error": format!("{error:#}")})),
                    )?,
                }
                // Nobody needs to send /exit to a session that exited, nor
                // answer its dialog.
                for ask in sv
                    .queue
                    .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
                {
                    info!(run_id = %run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", run.id(), ask.id);
                }
                close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
                self.stall.ended(sv, run)?;
                self.recovery.stop(sv, run);
                return sv.queue.finish_supervision(run.id(), &sv.token).map(Some);
            }
            let pulse = wrapper_pulse(
                sv,
                run,
                wrapper,
                &self.workspace,
                &mut self.silent,
                "wrapper heartbeat expired; session may still be alive",
            )?;
            match pulse {
                WrapperPulse::Silent if self.exit_requested.is_none() => {
                    // The same single /exit a finished session gets,
                    // recorded before it is sent.
                    let timeout = sv.cmux.exit_timeout();
                    sv.queue.record_runtime_event(
                        run.id(),
                        "exit_requested",
                        json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                    )?;
                    let workspace = self.workspace.clone();
                    submit(sv, run, &workspace, Input::Exit, "/exit")?;
                    info!(run_id = %run.id(), "exit requested for {} after its wrapper went silent; waiting for session exit", run.id());
                    self.exit_requested = Some(Instant::now());
                    self.exit_for_silence = true;
                }
                WrapperPulse::Silent => (),
                WrapperPulse::Exited => return Ok(None),
                WrapperPulse::Fresh => {
                    if self.exit_requested.is_none() {
                        self.deliver_answers(sv, run)?;
                        if !self.receipt_seen
                            && let Some(start) = &mut self.answer_start
                        {
                            start.poll(sv, run, &self.workspace, &self.idle_marker)?;
                        }
                        if !self.receipt_seen
                            && let Some(start) = self.stall.poll(
                                sv,
                                run,
                                &self.workspace,
                                &self.idle_marker,
                                self.prompt_hash.is_some(),
                            )?
                        {
                            self.answer_start = Some(start);
                        }
                        self.watch_background(sv, run, &processes)?;
                    }
                    if let Some(agent) = processes.iter().find(|p| p.role == "agent") {
                        self.watch_prompt(sv, run, agent)?;
                    }
                }
            }
        } else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "wrapper did not register within {} seconds",
                timeout.as_secs()
            );
        }
        if let Some(requested) = self.exit_requested
            && !self.exit_timed_out
        {
            let timeout = sv.cmux.exit_timeout();
            let workspace = self.workspace.clone();
            if requested.elapsed() >= timeout && answer_exit_dialog(sv, run, &workspace, true)? {
                // A known dialog answered by rule gets the exit timeout
                // again (ADR-0047 decision 29).
                self.exit_requested = Some(Instant::now());
            } else if requested.elapsed() >= timeout {
                // Something in the session (for example a dialog) held the
                // /exit back. Keep the lease and keep watching: the run
                // proceeds to validation once the session exits. /exit is not
                // sent again, since it could pick another option of a dialog.
                sv.queue.record_runtime_event(
                    run.id(),
                    "exit_request_timed_out",
                    json!({"code": ReasonCode::ExitTimeout, "workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                warn!(run_id = %run.id(), "session for {} did not exit within {}s of the exit request; keeping the run and asking the inbox to send /exit in workspace {}", run.id(), timeout.as_secs(), self.workspace);
                self.exit_timed_out = true;
            }
        }
        if self.exit_timed_out && !self.exit_asked {
            ask_stuck_exit(
                sv,
                run,
                &self.workspace,
                &stuck_exit_after(
                    self.exit_for_silence,
                    "The run stays running, and goes on to validating once the session exits",
                ),
            )?;
            self.exit_asked = true;
        }
        Ok(None)
    }

    /// Record `first_commit_observed` once, the first time the worktree's
    /// HEAD is seen away from the run's base commit: with `agent_started` it
    /// measures how long a session takes to start working (`stats`'s
    /// `startup`). The time is when this poll saw it, at most a tick late.
    /// A HEAD that cannot be read is noted and checked again next poll.
    pub(super) fn watch_first_commit(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        if self.first_commit_seen {
            return Ok(());
        }
        let Some(worktree) = run.worktree_path() else {
            return Ok(());
        };
        let head = match sv.repository.head(Path::new(worktree)) {
            Ok(head) => head,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "HEAD of {} could not be read for its first commit: {error:#}", run.id());
                return Ok(());
            }
        };
        if head != *run.base_commit() {
            sv.queue.record_runtime_event(
                run.id(),
                "first_commit_observed",
                json!({"commit": head, "base_commit": run.base_commit()}),
            )?;
            self.first_commit_seen = true;
        }
        Ok(())
    }

    /// Read the screen of a session that has run for `prompt_wait` with
    /// neither a receipt nor an idle marker, its wrapper and agent alive, and
    /// record a dialog found there as `prompt_waiting` (once per screen) and
    /// its disappearance as `prompt_cleared`. A known dialog whose
    /// conditions hold is answered by rule instead (ADR-0047 decision 29);
    /// no other dialog gets a key. A dialog is raised to the inbox as an `answer_prompt` ask with the
    /// screen's excerpt (ADR-0024's Consequences), which the runtime closes
    /// once the dialog is gone, the receipt arrives or the session exits.
    pub(super) fn watch_prompt(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        agent: &RunProcess,
    ) -> Result<()> {
        let started = *self.agent_seen.get_or_insert_with(Instant::now);
        let wait = sv.cmux.prompt_wait();
        if self.receipt_seen {
            // `receipt_observed` ends the dialog by itself.
            if self.prompt_hash.take().is_some() {
                close_answer_prompt_asks(sv, run, PROMPT_RECEIPT_CLOSED)?;
            }
            return Ok(());
        }
        if sv.files.exists(&self.idle_marker)
            || !sv.processes.alive(agent.pid)
            || sv.queue.has_unclosed_worker_question(run.id())?
        {
            // The agent finished a response, is gone, or stopped at an ask
            // that waits for its answer: no dialog holds it now, and a
            // recorded one must not stay an attention.
            return self.clear_prompt(sv, run);
        }
        // A recorded dialog (also one adopted from the previous supervisor)
        // is rechecked without waiting again, so an answer clears it soon.
        if (self.prompt_hash.is_none() && started.elapsed() < wait)
            || self
                .prompt_checked
                .is_some_and(|at| at.elapsed() < wait.min(PROMPT_CHECK_INTERVAL))
        {
            return Ok(());
        }
        self.prompt_checked = Some(Instant::now());
        let screen = match sv.cmux.capture(&self.workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a dialog: {error:#}", run.id());
                return Ok(());
            }
        };
        // A login that ran out is no dialog to answer: only a person logs
        // in again, once for every session it stopped (ADR-0047 decision 42).
        let workspace = self.workspace.clone();
        if sv.signals.auth_required(&screen) && raise_auth(sv, run, &workspace, &screen)? {
            return self.clear_prompt(sv, run);
        }
        // A known dialog is answered by rule once its conditions hold
        // (ADR-0047 decision 29); otherwise, or once answered in vain, it is
        // raised like any other.
        if answer_known_dialog(sv, run, &workspace, &screen, false)? {
            return Ok(());
        }
        match sv.signals.detect_prompt(&screen) {
            Some(kind) => {
                let excerpt = sv.signals.screen_excerpt(&screen);
                let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
                if self.prompt_hash.as_deref() != Some(hash.as_str()) {
                    sv.queue.record_runtime_event(
                        run.id(),
                        "prompt_waiting",
                        json!({
                            "workspace_id": self.workspace,
                            "excerpt": excerpt,
                            "screen_hash": hash,
                            "prompt": kind,
                        }),
                    )?;
                    info!(run_id = %run.id(), "run {} waits at a {} dialog in workspace {}; asking the inbox", run.id(), kind, self.workspace);
                    // A changed screen under an open ask keeps that ask (the
                    // open ask of the run is returned, and nobody is notified
                    // again), so a ticking line cannot flood the inbox.
                    self.prompt_hash = Some(hash);
                    ask_answer_prompt(sv, run, &self.workspace, kind, &excerpt)?;
                }
            }
            None => self.clear_prompt(sv, run)?,
        }
        Ok(())
    }

    /// Type the answer of each answered `worker_question` of the run into
    /// the worker's terminal, prefixed `answer to ask <id>:`, once the worker
    /// went idle after asking (its idle marker is no older than the ask, to
    /// the second), then close the ask and record `ask_delivered` (ADR-0022
    /// decision 2). Each answer is sent at most once: a failed send records
    /// `ask_delivery_failed` and leaves the ask unclosed for the inbox.
    pub(super) fn deliver_answers(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let answers = sv.queue.undelivered_answers(run.id())?;
        if answers.is_empty() {
            return Ok(());
        }
        // Background work does not hold an answer back: typing into the
        // prompt opens no dialog, only /exit does.
        let idle_at = match sv.files.modified(&self.idle_marker) {
            Ok(modified) => unix_seconds(modified),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let failed: Vec<AskId> = sv
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "ask_delivery_failed")
            .filter_map(|e| e.payload.get("ask_id").and_then(Value::as_i64))
            .map(AskId::new)
            .collect();
        for ask in answers {
            if failed.contains(&ask.id) || idle_at < ask.created_at {
                continue;
            }
            let text = format!(
                "answer to ask {}: {}",
                ask.id,
                ask.answer.as_deref().unwrap_or_default()
            );
            let what = format!("answer of ask {}", ask.id);
            let sent_at = sv.files.now();
            let workspace = self.workspace.clone();
            match submit(sv, run, &workspace, Input::Text(&text), &what) {
                // Sent: failing to record it must not cost the live run its
                // lease, so it is only noted (the ask then shows unclosed).
                Ok(submission) => {
                    self.stall.input_sent(sent_at);
                    self.answer_start = Some(StartCheck::new(&what, &text, sent_at, &submission));
                    match sv.queue.ask_delivered(ask.id, &self.workspace) {
                        Ok(_) => {
                            info!(ask_id = %ask.id, run_id = %run.id(), "answer of ask {} sent to run {} in workspace {}", ask.id, run.id(), self.workspace)
                        }
                        Err(error) => {
                            warn!(ask_id = %ask.id, run_id = %run.id(), error = %format_args!("{error:#}"), "answer of ask {} was sent to run {} but could not be recorded: {error:#}", ask.id, run.id())
                        }
                    }
                }
                Err(error) => {
                    sv.queue.record_runtime_event(
                        run.id(),
                        "ask_delivery_failed",
                        reason_of_error(&error, ReasonCode::BackendFailed).on(json!({
                            "ask_id": ask.id,
                            "workspace_id": self.workspace,
                            "error": format!("{error:#}"),
                        })),
                    )?;
                    warn!(ask_id = %ask.id, run_id = %run.id(), error = %format_args!("{error:#}"), "answer of ask {} could not be sent to run {} in workspace {}: {error:#}; it is left to the inbox", ask.id, run.id(), self.workspace);
                }
            }
        }
        Ok(())
    }

    /// Record `prompt_cleared` if a dialog is recorded and not cleared yet.
    pub(super) fn clear_prompt(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if self.prompt_hash.take().is_some() {
            sv.queue.record_runtime_event(
                run.id(),
                "prompt_cleared",
                json!({"workspace_id": self.workspace}),
            )?;
            info!(run_id = %run.id(), "dialog of {} is gone", run.id());
            close_answer_prompt_asks(sv, run, PROMPT_CLEARED_CLOSED)?;
        }
        Ok(())
    }
}

/// Raise a dialog a worker's session stopped at as an `answer_prompt` ask
/// to the inbox (ADR-0024's Consequences, in place of the attention of
/// ADR-0019 decision 6): the question names the run, the workspace and the
/// kind of dialog and carries the screen's excerpt. An open ask of the run
/// is not registered twice. The runtime sends no key: the person answers
/// the dialog, and the ask closes itself once the dialog is gone.
#[allow(clippy::too_many_arguments)]
pub(super) fn ask_answer_prompt(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    prompt: &str,
    excerpt: &str,
) -> Result<()> {
    let question = format!(
        "The session of run {run_id} (task {task_id}) waits at a {prompt} dialog in workspace {workspace}. Answer with the choice to send to it (or what to do instead); the dialog is answered in that workspace, and this ask closes itself once the dialog is gone.\n\nLast lines of the screen:\n{excerpt}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: Vec::new(),
            asked_by: SessionRole::Supervisor.as_str().into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
        },
        sv.cmux,
    )?;
    info!(ask_id = %outcome["id"], run_id = %run.id(), "answer_prompt ask {} for {} (notified: {})", outcome["id"], run.id(), outcome["notified"]);
    Ok(())
}

/// Raise a worker's session stopped at a login that ran out (ADR-0047
/// decision 42): the run joins the queue's open `authentication` ask, or
/// opens it, and a run that joined records `auth_required` with the
/// screen's excerpt and its hash. However many sessions stop at it, the
/// inbox gets one ask and one notification, with the runs it holds listed.
/// The error stays on the screen after a person logged in and answered the
/// ask, so a screen the run already raised under an ask that is answered
/// now is not raised again: returns `false`, and the caller goes on as if
/// no login held the session (the stall nudge then tells it to go on).
pub(super) fn raise_auth(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    screen: &str,
) -> Result<bool> {
    let excerpt = sv.signals.screen_excerpt(screen);
    let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
    let last = sv
        .queue
        .run_events(run.id())?
        .into_iter()
        .rev()
        .find(|e| e.kind == "auth_required");
    if let Some(last) = last
        && last.payload.get("screen_hash").and_then(Value::as_str) == Some(hash.as_str())
        && let Some(id) = last.payload.get("ask_id").and_then(Value::as_i64)
        && !sv.queue.read_ask(AskId::new(id))?.is_open()
    {
        return Ok(false);
    }
    let (outcome, value) = ask::hold(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewHold {
            reason_category: AskReason::Authentication,
            subject: None,
            run_id: run.id().clone(),
            question: AUTH_QUESTION.into(),
            options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: SessionRole::Supervisor.as_str().into(),
        },
        sv.cmux,
    )?;
    if outcome.joined {
        sv.queue.record_runtime_event(
            run.id(),
            "auth_required",
            json!({
                "workspace_id": workspace,
                "excerpt": excerpt,
                "screen_hash": hash,
                "ask_id": outcome.ask.id,
            }),
        )?;
        warn!(ask_id = %outcome.ask.id, run_id = %run.id(), "run {} stopped at a login that ran out in workspace {workspace}; authentication ask {} holds {} run(s) (notified: {})", run.id(), outcome.ask.id, outcome.ask.affected.len(), value["notified"]);
    }
    Ok(true)
}

/// The question of the authentication ask; the runs it holds follow it.
pub(super) const AUTH_QUESTION: &str = "Claude Code's login ran out: worker sessions stopped at an authentication error (`Please run /login`, an API 401). Only a person can log in again: run `claude` in a terminal, `/login`, and answer `done`; the supervisor then nudges each held run that stays idle without a receipt to go on. Answer `cancel_affected` to give the held runs up instead (the inbox recovers them; the runtime does not apply it yet). More runs that stop at the login join this ask instead of opening another.";

/// Close the run's `answer_prompt` asks nobody closed, noting each.
pub(super) fn close_answer_prompt_asks(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    answer: &str,
) -> Result<()> {
    for ask in sv.queue.close_answer_prompt_asks(run.id(), answer)? {
        info!(ask_id = %ask.id, run_id = %run.id(), "closed the answer_prompt ask {} of {}: {answer}", ask.id, run.id());
    }
    Ok(())
}

/// The answers the runtime writes into an open `answer_prompt` ask it closes.
pub(super) const PROMPT_CLEARED_CLOSED: &str = "the dialog is gone; closed by the runtime";

pub(super) const PROMPT_RECEIPT_CLOSED: &str = "the receipt arrived; closed by the runtime";

pub(super) const PROMPT_EXITED_CLOSED: &str = "the session exited; closed by the runtime";

pub(super) const INPUT_READY_CLOSED: &str =
    "the input box got ready and the request was sent; closed by the runtime";

/// A session's screen is read for a dialog at most this often.
pub(super) const PROMPT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
