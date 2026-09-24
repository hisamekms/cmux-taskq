//! The triage of `failed` and `interrupted` runs (ADR-0024 decision 3):
//! dead runs recovered, the headless triage, its verdict and the answers
//! to its asks.

use super::*;

impl Supervisor<'_> {
    /// Recover the unfinished runs nobody leases whose wrapper exited or
    /// died (ADR-0024 decision 3, amending ADR-0012): `recover`'s own check
    /// (no live process of the run; `doctor`'s blockers empty) on
    /// `claimed` / `starting` / `running` / `validating` runs without a
    /// lease row. They become `interrupted` with `run_recovered` (`by:
    /// supervisor`) and go to the triage, never straight to `ready`. A run
    /// that changed meanwhile is left for a later pass.
    pub(super) fn recover_dead_runs(&mut self) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.active_runs()? {
            if run.status() == RunStatus::Integrating || self.queue.run_lease(run.id())?.is_some() {
                continue;
            }
            let processes = self.queue.processes(run.id())?;
            let health = run_health(&run, &processes, None, now, &*self.processes, &*self.files);
            if !health.recoverable {
                continue;
            }
            let report = json!({"run": health, "by": "supervisor"});
            match self.queue.recover_run(run.id(), processes.len(), report) {
                Ok(recovered) => {
                    info!(run_id = %recovered.id(), task_id = %recovered.task_id(), "run {} of task {} recovered from {}: nobody leases it and its session is gone; it goes to triage", recovered.id(), recovered.task_id(), run.status().as_str())
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} could not be recovered: {error:#}", run.id())
                }
            }
        }
        Ok(())
    }
    /// Apply the answered `decide` asks of the triage (one of
    /// [`TRIAGE_OPTIONS`]) to their `failed` / `interrupted` run nobody
    /// leases: `retry` readies the task, `resume` parks the run as
    /// `needs_session` with the triage's reason, `cancel` cancels the task;
    /// the ask is closed with it. An ask whose task is no longer in progress,
    /// or has a newer run, has nothing left to apply and is closed. Any other answer is a
    /// person's to read.
    pub(super) fn apply_triage_answers(&mut self) -> Result<()> {
        for ask in self.queue.triage_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            // Only an option the ask offered: a run whose resumes are used
            // up is not offered `resume`.
            if !TRIAGE_OPTIONS.contains(&answer.as_str())
                || !ask.options.contains(&answer)
                || !matches!(run.status(), RunStatus::Failed | RunStatus::Interrupted)
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            let detail = self.queue.show(run.task_id())?;
            if detail.task.status() != TaskStatus::InProgress
                || detail
                    .runs
                    .last()
                    .is_some_and(|latest| *latest.id() != *run.id())
            {
                info!(ask_id = %ask.id, run_id = %run.id(), task_id = %run.task_id(), "ask {} of run {} is closed: task {} moved on without it", ask.id, run.id(), run.task_id());
                self.queue.close_ask(ask.id)?;
                continue;
            }
            let reason = self
                .queue
                .run_events(run.id())?
                .iter()
                .rev()
                .find(|e| e.kind == "triage_finished")
                .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
                .map_or_else(
                    || run.last_error().map(str::to_owned).unwrap_or_default(),
                    str::to_owned,
                );
            let reason = format!("{reason} (a person chose {answer} in ask {})", ask.id);
            match self.queue.decide_triage(run.id(), ask.id, &answer, &reason) {
                Ok(decided) => {
                    info!(run_id = %decided.id(), task_id = %decided.task_id(), ask_id = %ask.id, "run {} of task {}: {answer} as ask {} answered; the run is {}", decided.id(), decided.task_id(), ask.id, decided.status().as_str())
                }
                Err(error) => {
                    warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id)
                }
            }
        }
        Ok(())
    }
    /// Start the triage of `failed` / `interrupted` runs not triaged since
    /// their last resume, while slots are free (ADR-0024 decision 3). A run
    /// someone leases (a session still asked to exit) waits. A triage that
    /// cannot even start fails right away.
    pub(super) fn triage_runs(&mut self, parallel: usize) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.runs_to_triage()? {
            if self.slots.len() >= parallel {
                break;
            }
            if triage_state(&self.queue.run_events(run.id())?) != TriageState::Pending
                || self
                    .queue
                    .run_lease(run.id())?
                    .is_some_and(|lease| !self.lease_stale(&lease, now))
            {
                continue;
            }
            let Some((run, attempt)) = self.queue.begin_triage(run.id(), &self.token)? else {
                continue;
            };
            match self.spawn_triage(&run, attempt) {
                Ok(watch) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} ({}) triage {attempt} started", run.id(), run.task_id(), run.status().as_str());
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Triage(watch),
                    });
                }
                Err(error) => {
                    let error = format!("the headless triage could not start: {error:#}");
                    self.fail_triage(&run, attempt, error, 0);
                    let run = self.queue.run(run.id())?;
                    self.note_triaged(&run);
                }
            }
        }
        Ok(())
    }
    /// Write the triage's prompt and start the headless job in the run's
    /// directory, allowed to read only (ADR-0024 decision 2).
    pub(super) fn spawn_triage(&mut self, run: &TaskRun, attempt: usize) -> Result<TriageWatch> {
        let dir = match &run.run_dir() {
            Some(dir) => PathBuf::from(dir),
            None => self.layout.runs_dir.join(run.id().as_str()),
        };
        self.files
            .create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let detail = self.queue.show(run.task_id())?;
        let resumes = resume_attempts(&*self.queue, run.id());
        let prompt = triage_prompt(&*self.files, &detail, run, resumes, &dir)?;
        self.files.write(
            &dir.join(format!("triage-prompt-{attempt}.txt")),
            prompt.as_bytes(),
        )?;
        let stdout = dir.join(format!("triage-{attempt}.out"));
        let stderr = dir.join(format!("triage-{attempt}.err"));
        let mut command = self
            .reviewer
            .headless_command(&dir, &prompt, TRIAGE_TOOLS)?;
        // Like the review: the CLI knows the job by its role and allows it
        // only reads of this queue.
        command.envs(self.layout.job_env.iter().cloned());
        let child = self
            .spawner
            .spawn(
                &command,
                Streams::Files {
                    stdout: &stdout,
                    stderr: &stderr,
                },
            )
            .context("start the triage")?;
        Ok(TriageWatch {
            attempt,
            job: HeadlessJob {
                what: "triage",
                child,
                started: Instant::now(),
                timeout: self.reviewer.review_timeout(),
                stdout,
                stderr,
            },
        })
    }
    /// Act on the triage's verdict (ADR-0024 decision 3). The runtime's own
    /// rules come first: a task with [`TRIAGE_RETRY_FAILURES`] failed or
    /// interrupted runs is not retried, and a run without resumes left or
    /// without a worktree is not resumed; either becomes an ask. Then
    /// `triage_finished` with the action, the workspaces the run left open
    /// are closed, and the lease is released.
    pub(super) fn act_on_triage(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        duration_secs: u64,
        verdict: TriageVerdict,
    ) -> Result<TaskRun> {
        let failures = self
            .queue
            .show(run.task_id())?
            .runs
            .iter()
            .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
            .count();
        ensure!(
            self.queue.holds_lease(run.id(), &self.token)?,
            "the triage's lease of run {} was lost",
            run.id()
        );
        let resumes = resume_attempts(&*self.queue, run.id());
        let worktree = run
            .worktree_path()
            .is_some_and(|path| self.files.is_dir(Path::new(path)));
        let overridden = match verdict.verdict {
            TriageDecision::Retry if failures >= TRIAGE_RETRY_FAILURES => Some(format!(
                "task {} has {failures} failed or interrupted runs, so it is not retried without a person",
                run.task_id()
            )),
            TriageDecision::Resume if resumes >= MAX_RESUME_ATTEMPTS => Some(format!(
                "the run was resumed {resumes} times already (at most {MAX_RESUME_ATTEMPTS})"
            )),
            TriageDecision::Resume if !worktree || run.receipt_path().is_none() => {
                Some("the run has no worktree a session could resume in".to_owned())
            }
            _ => None,
        };
        let action = match (verdict.verdict, &overridden) {
            (TriageDecision::Retry, None) => TriageAction::Retry,
            (TriageDecision::Resume, None) => TriageAction::Resume {
                instruction: if verdict.instruction.trim().is_empty() {
                    verdict.reason.clone()
                } else {
                    verdict.instruction.clone()
                },
            },
            _ => TriageAction::Ask {
                ask_id: self.open_triage_ask(run, attempt, &verdict, overridden.as_deref())?,
            },
        };
        let payload = json!({
            "attempt": attempt,
            "verdict": verdict.verdict,
            "reason": verdict.reason,
            "instruction": verdict.instruction,
            "overridden": overridden,
            "failures": failures,
            "duration_secs": duration_secs,
        });
        let triaged = self
            .queue
            .finish_triage(run.id(), &self.token, &action, payload)?;
        info!(run_id = %run.id(), "run {} triage {attempt}: {}{} ({}); the run is {}", run.id(), verdict.verdict.as_str(), match &overridden {
                Some(why) => format!(" became ask: {why}"),
                None => String::new(),
            }, verdict.reason, triaged.status().as_str());
        // The verdict is acted on: what fails from here on is logged, not
        // a failed triage.
        if let Err(error) = self.close_triaged_workspaces(&triaged) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
        }
        if let Err(error) = self.queue.release_lease(run.id(), &self.token) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
        }
        self.queue.run(run.id())
    }
    /// Open the triage's `decide` ask (options [`TRIAGE_OPTIONS`]) through
    /// `ask`, so the inbox is notified; returns its ID.
    pub(super) fn open_triage_ask(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        verdict: &TriageVerdict,
        overridden: Option<&str>,
    ) -> Result<i64> {
        let asked = match (verdict.verdict, overridden) {
            (TriageDecision::Ask, _) if !verdict.instruction.trim().is_empty() => {
                verdict.instruction.clone()
            }
            (_, Some(why)) => format!(
                "the triage answered {} ({}), but {why}",
                verdict.verdict.as_str(),
                verdict.instruction.trim()
            ),
            _ => "what should happen to this run?".to_owned(),
        };
        let mut question = format!(
            "The supervisor's triage of run {} (task {}, {}) asks a person: {asked}\nReason: {}\nLast error: {}",
            run.id(),
            run.task_id(),
            run.status().as_str(),
            verdict.reason,
            or_none(tail(run.last_error().unwrap_or_default(), 500))
        );
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!(
                "\nTriage material: {run_dir}/triage-prompt-{attempt}.txt"
            ));
        }
        question.push_str(
            "\nretry: make the task ready for a new run. resume: resume the run's own session with the triage's reason. cancel: cancel the task.",
        );
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: TRIAGE_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: TRIAGE_ASKER.to_owned(),
            },
            self.cmux,
        )?;
        outcome["id"].as_i64().context("ask returned no id")
    }
    /// Close the workspaces a triaged run left open: its worker workspace
    /// (unless the runtime closed it) and the resume workspaces its
    /// `resume_finished` events name as not closed, each only while cmux
    /// still lists it. A close records `workspace_closed` (`by: triage`); a
    /// cmux failure records `cleanup_failed` and the others go on. A
    /// `stuck_exit` ask of the run is closed with its workspace.
    pub(super) fn close_triaged_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let mut workspaces: Vec<String> = run
            .workspace_id()
            .map(str::to_owned)
            .filter(|_| run.workspace_closed_at().is_none())
            .into_iter()
            .collect();
        for event in self.queue.run_events(run.id())? {
            if event.kind == "resume_finished"
                && event.payload["workspace_closed"] != true
                && let Some(workspace) = event.payload.get("workspace_id").and_then(Value::as_str)
                && !workspaces.iter().any(|w| w == workspace)
            {
                workspaces.push(workspace.to_owned());
            }
        }
        let mut closed = false;
        for workspace in workspaces {
            let result = self.cmux.exists(&workspace).and_then(|open| {
                if open {
                    self.cmux.close(&workspace)?;
                }
                Ok(open)
            });
            match result {
                Ok(true) => {
                    self.queue.triage_closed_workspace(run.id(), &workspace)?;
                    closed = true;
                }
                Ok(false) => {}
                Err(error) => {
                    let message = format!("workspace {workspace} could not be closed: {error:#}");
                    warn!(run_id = %run.id(), "run {}: {message}", run.id());
                    self.queue.record_runtime_event(
                        run.id(),
                        "cleanup_failed",
                        reason_of_error(&error, ReasonCode::Other).on(
                            json!({"workspace_id": workspace, "message": message, "by": "triage"}),
                        ),
                    )?;
                }
            }
        }
        if closed {
            self.queue
                .close_stuck_exit_asks(run.id(), "the triage closed the run's workspace")?;
        }
        // Whatever path took the run out of `running`, no dialog of it waits
        // for an answer any more.
        self.queue
            .close_answer_prompt_asks(run.id(), "the run was triaged; closed by the runtime")?;
        Ok(())
    }
    /// Record `triage_failed` (a person triages the run) and give the lease
    /// back; the run stays as it is.
    pub(super) fn fail_triage(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        error: String,
        duration_secs: u64,
    ) {
        warn!(run_id = %run.id(), error = %error, "run {} triage {attempt} failed: {error}; the run waits for a triage by hand", run.id());
        let recorded = self.queue.record_runtime_event(
            run.id(),
            "triage_failed",
            json!({
                "code": ReasonCode::JobFailed,
                "attempt": attempt,
                "error": error,
                "duration_secs": duration_secs,
                "status": run.status().as_str(),
            }),
        );
        if let Err(error) = recorded {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the triage failure: {error:#}", run.id());
        }
        if self
            .queue
            .holds_lease(run.id(), &self.token)
            .unwrap_or(false)
            && let Err(error) = self.queue.release_lease(run.id(), &self.token)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
        }
    }
    pub(super) fn note_triaged(&mut self, run: &TaskRun) {
        let task = self
            .queue
            .show(run.task_id())
            .map(|detail| detail.task.status());
        info!(run_id = %run.id(), "run {} triaged: the run is {}{}", run.id(), run.status().as_str(), match task {
            Ok(status) => format!(", task {} is {}", run.task_id(), status.as_str()),
            Err(_) => String::new(),
        });
        self.triaged.push(json!({
            "run_id": run.id(),
            "task_id": run.task_id(),
            "status": run.status(),
        }));
    }
}
