//! The review of an accepted run and its landing: the headless review,
//! its verdict, the conflict precheck, the `approve_landing` ask and the
//! landing itself (ADR-0023, ADR-0027).

use super::*;

/// The answer the supervisor closes an earlier, unclosed `approve_landing`
/// ask of a run with when a later review of the run fails (task 328).
const STALE_LANDING_ASK_CLOSED: &str =
    "a later review of the run failed and asks again; closed by the runtime";

impl Supervisor<'_> {
    /// Record `landing_queued` as `run` starts to wait for the integration
    /// slot (`Phase::AwaitingSlot`): `stats` counts the wait from there to
    /// `integration_started` as its `landing_queue` phase (goal 36). `via`
    /// says what sent it: `exit` (its session exited after a passed review
    /// or with the run approved) or `resume` (an approved resolved resume).
    /// Only `stats` reads it, so a failure to record it is only reported:
    /// the step it follows has already changed the run.
    pub(super) fn queue_landing(&mut self, run: &TaskRun, via: &str) {
        if let Err(error) =
            self.queue
                .record_runtime_event(run.id(), "landing_queued", json!({"via": via}))
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record landing_queued: {error:#}", run.id());
        }
    }
    /// Land `run`, which holds the integration slot under this token, on a
    /// thread (`previous` is where an error before `main` moved returns it).
    /// It pushes unless an approving `integrate --no-push` recorded
    /// `push: false`; a run landed on a passed review always pushes.
    pub(super) fn spawn_landing(
        &self,
        run: TaskRun,
        previous: RunStatus,
        main: CommitSha,
    ) -> Result<thread::JoinHandle<Result<IntegrationOutcome>>> {
        let queues = self.queues.clone();
        let repository = self.repository.clone();
        let remote = self.remote.clone();
        let verifier = self.verifier.clone();
        let processes = self.processes.clone();
        let files = self.files.clone();
        let pid = self.layout.pid;
        let token = self.token.clone();
        let common_dir = path_text(&self.layout.common_dir)?;
        let push = self
            .queue
            .run_events(run.id())?
            .iter()
            .find(|e| e.kind == "integration_approved")
            .is_none_or(|e| e.payload.get("push") != Some(&json!(false)));
        let generators = self.generators.clone();
        Ok(spawn_traced(move || {
            let mut queue = queues.open()?;
            integration::land_integrating(
                &mut Integration {
                    queue: &mut *queue,
                    repository: &*repository,
                    verifier: &*verifier,
                    remote: push.then_some(&*remote as &dyn MainRemote),
                    files: &*files,
                    common_dir: &common_dir,
                    clock: &*generators.clock,
                    ids: &*generators.ids,
                    processes: &*processes,
                    pid,
                },
                &run,
                previous,
                &main,
                &token,
            )
        }))
    }
    /// Record `review_started` and start the headless review of an accepted
    /// run whose session stays open (ADR-0027 decision 1): write
    /// `review.md`, then run the reviewer's command with the task's
    /// acceptance and the verdict schema. A review that cannot even start
    /// is a failed one.
    pub(super) fn start_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
    ) -> Result<Phase> {
        self.begin_review(run, session, false)
    }
    /// Review the run once more with the same input after review `attempt`
    /// printed no readable verdict (task 328): record `review_retried` with
    /// why, then start the next review, whose own unreadable verdict is not
    /// retried again.
    pub(super) fn retry_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        attempt: usize,
        error: &str,
    ) -> Result<Phase> {
        self.queue.record_runtime_event(
            run.id(),
            "review_retried",
            json!({"attempt": attempt, "error": error}),
        )?;
        warn!(run_id = %run.id(), error = %error, "run {} review {attempt} printed no readable verdict: {error}; reviewing it once more", run.id());
        self.begin_review(run, session, true)
    }
    fn begin_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        let attempt = self
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "review_started")
            .count()
            + 1;
        let live = match &session {
            Some(_) => session_alive(self, run.id())?,
            None => false,
        };
        self.queue.record_runtime_event(
            run.id(),
            "review_started",
            json!({
                "attempt": attempt,
                "workspace_id": session.as_ref().map(|s| s.workspace.clone()),
                "session_live": live,
            }),
        )?;
        Ok(match self.spawn_review(run, attempt) {
            Ok((child, stdout, stderr)) => {
                info!(run_id = %run.id(), "run {} review {attempt} started (session {})", run.id(), if live { "kept open" } else { "ended" });
                Phase::Review(ReviewWatch {
                    session,
                    attempt,
                    retried,
                    job: HeadlessJob {
                        what: "review",
                        child,
                        started: Instant::now(),
                        timeout: self.reviewer.review_timeout(),
                        stdout,
                        stderr,
                    },
                })
            }
            Err(error) => {
                let error = format!("the headless review could not start: {error:#}");
                warn!(run_id = %run.id(), error = %error, "run {}: {error}", run.id());
                Phase::Exiting(ExitWatch::new(
                    session,
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs: 0,
                    },
                ))
            }
        })
    }
    pub(super) fn spawn_review(
        &mut self,
        run: &TaskRun,
        attempt: usize,
    ) -> Result<(Box<dyn Spawned>, PathBuf, PathBuf)> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let material = (self.review_material)(run.task_id())?;
        let path = material["path"]
            .as_str()
            .context("review wrote no path")?
            .to_owned();
        let task = self.queue.show(run.task_id())?.task;
        let prompt = review_prompt(&task, run, &path);
        self.files.write(
            &run_dir.join(format!("review-prompt-{attempt}.txt")),
            prompt.as_bytes(),
        )?;
        let stdout = run_dir.join(format!("review-{attempt}.out"));
        let stderr = run_dir.join(format!("review-{attempt}.err"));
        let mut command = self.reviewer.review_command(run, &prompt)?;
        // The repository's [run.env] reaches the review too (ADR-0023
        // decision 3).
        command
            .envs(self.verifier.run_env(&run_dir)?)
            // Like the observer's job: the CLI knows the review by its role
            // and allows it only reads of this queue.
            .envs(self.layout.job_env.iter().cloned());
        let child = self
            .spawner
            .spawn(
                &command,
                Streams::Files {
                    stdout: &stdout,
                    stderr: &stderr,
                },
            )
            .context("start the review")?;
        Ok((child, stdout, stderr))
    }
    /// Move on from a verdict: `pass` exits the session and lands; `revise`
    /// goes to the live session while revises are left (ADR-0027 decision
    /// 2); anything else exits the session and asks a person.
    pub(super) fn act_on_verdict(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
    ) -> Result<Phase> {
        let ask = |why: Option<String>, verdict: ReviewVerdict, session| {
            Phase::Exiting(ExitWatch::new(
                session,
                AfterExit::Ask {
                    decision: verdict.verdict,
                    reasons: verdict.reasons,
                    summary: verdict.summary,
                    why,
                },
            ))
        };
        match verdict.verdict {
            ReviewDecision::Pass => self.precheck(run, session, verdict),
            ReviewDecision::Concern => Ok(ask(None, verdict, session)),
            ReviewDecision::Revise => {
                let revises = self
                    .queue
                    .run_events(run.id())?
                    .iter()
                    .filter(|e| e.kind == "revise_requested")
                    .count();
                if revises >= MAX_REVISE_ATTEMPTS {
                    let why = format!("the review still asks for changes after {revises} revises");
                    return Ok(ask(Some(why), verdict, session));
                }
                let Some(live) = session
                    .clone()
                    .filter(|_| session_alive(self, run.id()).unwrap_or(false))
                else {
                    let why = "the session had ended, so nobody could revise the run".to_owned();
                    return Ok(ask(Some(why), verdict, session));
                };
                let attempt = revises + 1;
                let task = self.queue.show(run.task_id())?.task;
                let message = revise_request(&task, run, attempt, &verdict.reasons)?;
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                self.files.write(
                    &run_dir.join(format!("revise-{attempt}.txt")),
                    message.as_bytes(),
                )?;
                let sent_at = self.files.now();
                let submission = match submit(
                    self,
                    run,
                    &live.workspace,
                    Input::Text(&message),
                    "revise request",
                ) {
                    Ok(submission) => submission,
                    Err(error) => {
                        let why = format!("the revise request could not be sent: {error:#}");
                        warn!(run_id = %run.id(), "run {}: {why}", run.id());
                        return Ok(ask(Some(why), verdict, session));
                    }
                };
                self.queue.record_runtime_event(
                    run.id(),
                    "revise_requested",
                    json!({"attempt": attempt, "reasons": verdict.reasons, "sent_at": unix_seconds(sent_at)}),
                )?;
                info!(run_id = %run.id(), "revise {attempt} of {MAX_REVISE_ATTEMPTS} sent to run {} in workspace {}", run.id(), live.workspace);
                Ok(Phase::Revise(ReviseWatch {
                    session: live,
                    attempt,
                    fix: Fix::Revise(verdict.reasons),
                    sent_at,
                    sent: Instant::now(),
                    start: Some(StartCheck::new(
                        "revise request",
                        &message,
                        sent_at,
                        &submission,
                    )),
                }))
            }
        }
    }
    /// Before a passed run's session is asked to exit, judge with `git
    /// merge-tree` whether its head conflicts with the current main,
    /// without touching the worktree (ADR-0027 decision 4). A clean merge
    /// exits the session and lands. A conflict records `conflict_precheck`
    /// and sends the live session the resolution request of a resume; the
    /// session's rewritten receipt is validated and reviewed again. The
    /// requests and the run's resumes share `MAX_RESUME_ATTEMPTS`: past it,
    /// the session exits and a person is asked. Without a live session to
    /// ask (or when Git cannot judge), the run lands as before, and a
    /// conflicting landing parks it for a resume.
    pub(super) fn precheck(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
    ) -> Result<Phase> {
        let land = |session| Phase::Exiting(ExitWatch::new(session, AfterExit::Land));
        let head = run
            .result_commit()
            .cloned()
            .context("accepted run has no result commit")?;
        let main = self.repository.main_head()?;
        let conflicts = match self
            .repository
            .merge_conflicts(main.as_str(), head.as_str())
        {
            Ok(conflicts) => conflicts,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the conflict precheck against main {main} failed: {error:#}; landing", run.id());
                return Ok(land(session));
            }
        };
        if conflicts.is_empty() {
            return Ok(land(session));
        }
        let events = self.queue.run_events(run.id())?;
        let requested = events
            .iter()
            .filter(|e| e.kind == "conflict_precheck" && e.payload["requested"] == true)
            .count();
        // Resumes of the run parked only by a conflict after its review
        // passed are not counted (ADR-0047 decision 24).
        let resumes = ResumeCount::of(&events).counted;
        let attempt = requested + 1;
        let mut payload = json!({
            "code": ReasonCode::RebaseConflict,
            "main": main,
            "head": head,
            // Recorded for the reader; a failure to find it does not stop the request.
            "merge_base": self.repository.merge_base(main.as_str(), head.as_str()).ok().flatten(),
            "conflicts": conflicts,
            "attempt": attempt,
            "requested": false,
        });
        let why = format!(
            "git merge-tree finds that main {main} conflicts with the run in {}",
            conflicts.join(", ")
        );
        if requested + resumes >= MAX_RESUME_ATTEMPTS {
            let why = format!("{why}, after {requested} conflict requests and {resumes} resumes");
            // What an adopter asks, if it takes the run over before the ask.
            payload["asked"] = json!(why);
            self.queue
                .record_runtime_event(run.id(), "conflict_precheck", payload)?;
            info!(run_id = %run.id(), "run {}: {why}; asking a person", run.id());
            return Ok(Phase::Exiting(ExitWatch::new(
                session,
                Fix::Conflict(verdict).ask(String::new(), why),
            )));
        }
        let live = session
            .clone()
            .filter(|_| session_alive(self, run.id()).unwrap_or(false));
        let sent = match &live {
            Some(live) => {
                let task = self.queue.show(run.task_id())?.task;
                let landed = landed_since(
                    &mut *self.queue,
                    &*self.repository,
                    &*self.files,
                    run,
                    &main,
                )?;
                let request = ResumeRequest {
                    main: main.clone(),
                    reason: why.clone(),
                    kind: ResumeKind::Precheck,
                };
                let message = resume_request(&task, run, &request, &landed)?;
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                self.files.write(
                    &run_dir.join(format!("conflict-{attempt}.txt")),
                    message.as_bytes(),
                )?;
                let sent_at = self.files.now();
                submit(
                    self,
                    run,
                    &live.workspace,
                    Input::Text(&message),
                    "conflict request",
                )
                .map(|submission| {
                    (
                        sent_at,
                        StartCheck::new("conflict request", &message, sent_at, &submission),
                    )
                })
                .map_err(|error| format!("the request could not be sent: {error:#}"))
            }
            None => Err("the session had ended".to_owned()),
        };
        let (Some(live), Ok((sent_at, start))) = (live, sent.clone()) else {
            let error = sent.err().unwrap_or_default();
            payload["error"] = json!(error);
            self.queue
                .record_runtime_event(run.id(), "conflict_precheck", payload)?;
            warn!(run_id = %run.id(), error = %error, "run {}: {why}, and {error}; landing, whose rebase parks it for a resume", run.id());
            return Ok(land(session));
        };
        payload["requested"] = json!(true);
        payload["sent_at"] = json!(unix_seconds(sent_at));
        self.queue
            .record_runtime_event(run.id(), "conflict_precheck", payload)?;
        info!(run_id = %run.id(), "run {}: {why}; asked its live session in workspace {} to rebase (request {attempt})", run.id(), live.workspace);
        Ok(Phase::Revise(ReviseWatch {
            session: live,
            attempt,
            fix: Fix::Conflict(verdict),
            sent_at,
            sent: Instant::now(),
            start: Some(start),
        }))
    }
    /// Close the session's workspace after it exited: the worker's own
    /// through [`close_workspace`], a resume's by recording
    /// `workspace_closed` with its attempt.
    pub(super) fn close_session(&mut self, run: &TaskRun, session: &SessionRef) -> Result<TaskRun> {
        match session.resume {
            None if run.workspace_closed_at().is_none() && run.workspace_id().is_some() => {
                close_workspace(&mut *self.queue, self.cmux, &self.token, run)
            }
            None => Ok(run.clone()),
            Some(attempt) => {
                match self.cmux.close(&session.workspace) {
                    Ok(()) => self.queue.record_runtime_event(
                        run.id(),
                        "workspace_closed",
                        json!({"workspace_id": session.workspace, "resume_attempt": attempt}),
                    )?,
                    Err(error) => {
                        let message = format!(
                            "resume workspace {} could not be closed: {error:#}",
                            session.workspace
                        );
                        warn!(run_id = %run.id(), "run {}: {message}", run.id());
                        self.queue.record_cleanup_failure(
                            run.id(),
                            &message,
                            &reason_of_error(&error, ReasonCode::BackendFailed),
                        )?;
                    }
                }
                self.queue.run(run.id())
            }
        }
    }
    /// Open the `approve_landing` ask of a run whose review did not pass
    /// (ADR-0027, ADR-0022 decision 3) and notify the inbox; returns its ID.
    pub(super) fn open_landing_ask(
        &mut self,
        run: &TaskRun,
        decision: ReviewDecision,
        reasons: &[String],
        summary: &str,
        why: Option<&str>,
    ) -> Result<AskId> {
        let mut question = format!(
            "The supervisor's review of run {} (task {}) returned {}{}: {summary}",
            run.id(),
            run.task_id(),
            decision.as_str(),
            why.map(|why| format!(" ({why})")).unwrap_or_default()
        );
        for reason in reasons {
            question.push_str(&format!("\n- {reason}"));
        }
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!("\nReview material: {run_dir}/review.md"));
        }
        question.push_str(
            "\nland: land it as it is. send_back: resume the session with these reasons. cancel: fail the run and cancel the task.",
        );
        self.ask_approve_landing(run, question)
    }
    /// Open the `approve_landing` ask of a run whose headless review failed
    /// (task 328), with why and where the review's material and output
    /// are, so that the failure reaches the inbox in the step that records
    /// `review_failed`; returns its ID.
    pub(super) fn open_failed_review_ask(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        error: &str,
    ) -> Result<AskId> {
        // An earlier ask of the run is about an earlier review (the run was
        // sent back or landed by hand since): it would hold the new one back
        // as a repeat, and its answer no longer fits.
        for stale in self
            .queue
            .close_approve_landing_asks(run.id(), STALE_LANDING_ASK_CLOSED)?
        {
            info!(run_id = %run.id(), ask_id = %stale.id, "run {}: closed its earlier approve_landing ask {}", run.id(), stale.id);
        }
        let mut question = format!(
            "The supervisor's headless review of run {} (task {}) failed and gave no verdict (review {attempt}): {error}",
            run.id(),
            run.task_id(),
        );
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!(
                "\nReview material: {run_dir}/review.md\nReview output: {run_dir}/review-{attempt}.out, {run_dir}/review-{attempt}.err"
            ));
        }
        question.push_str(
            "\nReview the material by hand, then answer. land: land it as it is. send_back: resume the session with this failure as the reason. cancel: fail the run and cancel the task.",
        );
        self.ask_approve_landing(run, question)
    }
    fn ask_approve_landing(&mut self, run: &TaskRun, question: String) -> Result<AskId> {
        // Through `ask`, like the CLI: a new ask notifies the inbox.
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: LANDING_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".to_owned(),
                reason_category: AskReason::Scope,
                finding_id: None,
            },
            self.cmux,
        )?;
        outcome["id"]
            .as_i64()
            .map(AskId::new)
            .context("ask returned no id")
    }
    /// Apply the answered `approve_landing` asks of runs awaiting
    /// integration that nobody leases (ADR-0027): `land` lands the run in
    /// the single slot (as an approved one), `send_back` makes it
    /// `needs_session` for a resume that names the review's reasons, and
    /// `cancel` fails the run and cancels its task. The ask is closed once
    /// applied; any other answer is left to the inbox. An error is
    /// noted and the ask is tried again on a later pass.
    pub(super) fn apply_landing_answers(&mut self, parallel: usize) -> Result<()> {
        for ask in self.queue.landing_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            if run.status() != RunStatus::AwaitingIntegration
                || !LANDING_OPTIONS.contains(&answer.as_str())
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            // A landing would fail on the missing program (ADR-0049
            // decision 9): the answer waits until it is found.
            if answer == "land"
                && (self.run_env_missing
                    || self.slots.len() >= parallel
                    || !self
                        .queue
                        .runs_with_status(RunStatus::Integrating)?
                        .is_empty())
            {
                continue;
            }
            if let Err(error) = self.apply_landing_answer(&run, ask.id, &answer) {
                warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id);
            }
        }
        Ok(())
    }
    pub(super) fn apply_landing_answer(
        &mut self,
        run: &TaskRun,
        ask_id: AskId,
        answer: &str,
    ) -> Result<()> {
        let payload = json!({"ask_id": ask_id, "answer": answer});
        match answer {
            "land" => {
                if !self.queue.has_run_event(run.id(), "integration_approved")? {
                    self.queue.record_runtime_event(
                        run.id(),
                        "integration_approved",
                        json!({"status": run.status().as_str(), "pid": self.layout.pid, "push": true, "ask_id": ask_id}),
                    )?;
                }
                let main = self.repository.main_head()?;
                let landing = self.queue.begin_integration(run.id(), &self.token, &main)?;
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), "run {} lands onto main {main} as ask {ask_id} answered", run.id());
                let handle =
                    self.spawn_landing(landing.clone(), RunStatus::AwaitingIntegration, main)?;
                self.slots.push(Slot {
                    run: landing,
                    phase: Phase::Landing(Some(handle)),
                });
            }
            "send_back" => {
                let reasons = latest_review_reasons(&*self.queue, run.id())?;
                let reason = format!(
                    "the review's findings were sent back by ask {ask_id}: {}",
                    if reasons.is_empty() {
                        "(no reasons recorded)".to_owned()
                    } else {
                        reasons.join("; ")
                    }
                );
                self.queue.decide_landing(
                    run.id(),
                    RunStatus::NeedsSession,
                    &reason,
                    Reason::new(ReasonCode::SentBack).on(payload),
                )?;
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), "run {} was sent back by ask {ask_id}; it waits for a resume", run.id());
            }
            _ => {
                let reason = format!("canceled by ask {ask_id}");
                self.queue.decide_landing(
                    run.id(),
                    RunStatus::Failed,
                    &reason,
                    Reason::new(ReasonCode::Cancelled).on(payload),
                )?;
                self.queue.transition(run.task_id(), TaskAction::Cancel)?;
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), task_id = %run.task_id(), "run {} failed and task {} was canceled by ask {ask_id}", run.id(), run.task_id());
                self.clean_task_worktrees(run.task_id());
            }
        }
        Ok(())
    }
}
