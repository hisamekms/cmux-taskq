//! The recovery of `failed` and `interrupted` runs (ADR-0047 decisions 39
//! and 40, in place of the triage of ADR-0024 decision 3): dead runs
//! recovered, the recovery job of their `failed`, `interrupted` and
//! `resume_exhausted` alerts, its verdict, and the answers to the `decide`
//! asks it escalates to. The rounds keep the triage's event names
//! (`triage_started`, `triage_finished`, `triage_failed`).

use super::*;
use crate::domain::recovery::{
    ENDED_ACTIONS, MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, RecoveryAction, attempts,
    current_alert, pending_request, run_processes,
};

impl Supervisor<'_> {
    /// Recover the unfinished runs whose wrapper exited or died and whose
    /// supervisor is gone (ADR-0024 decision 3, amending ADR-0012):
    /// `recover`'s own check (no live process of the run; `doctor`'s
    /// blockers empty) on `claimed` / `starting` / `running` / `validating`
    /// runs without a lease row, or with a stale lease whose pid is dead
    /// that [`Self::adopt_stale_runs`] would not take (task 236: a run the
    /// dead supervisor left behind with its session). They become
    /// `interrupted` with `run_recovered` (`by: supervisor`) and go to the
    /// triage, never straight to `ready`. A run that changed meanwhile is
    /// left for a later pass.
    pub(super) fn recover_dead_runs(&mut self) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.active_runs()? {
            if run.status() == RunStatus::Integrating {
                continue;
            }
            let processes = self.queue.processes(run.id())?;
            let lease = match self.queue.run_lease(run.id())? {
                None => None,
                Some(lease) => {
                    let wrapper = processes.iter().find(|p| p.role == "wrapper");
                    if self.processes.alive(lease.pid) || self.adoptable(&run, wrapper, now)? {
                        continue;
                    }
                    Some(lease_health(&lease, now, &*self.processes))
                }
            };
            let leased = lease.is_some();
            let health = run_health(&run, &processes, lease, now, &*self.processes, &*self.files);
            if !health.recoverable {
                continue;
            }
            let report = json!({"run": health, "by": "supervisor"});
            match self.queue.recover_run(run.id(), processes.len(), report) {
                Ok(recovered) => {
                    let whose = if leased {
                        "its supervisor died"
                    } else {
                        "nobody leases it"
                    };
                    info!(run_id = %recovered.id(), task_id = %recovered.task_id(), "run {} of task {} recovered from {}: {whose} and its session is gone; it goes to triage", recovered.id(), recovered.task_id(), run.status().as_str())
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} could not be recovered: {error:#}", run.id())
                }
            }
        }
        Ok(())
    }
    /// Apply the answered `decide` asks of the recovery job (an option the
    /// ask offered) to their `failed` / `interrupted` run nobody leases:
    /// `retry` readies the task, `resume` parks the run as `needs_session`
    /// with the round's reason, `cancel` cancels the task, and any other
    /// option (the job's own) goes back to the job: the run is taken by
    /// another round that reads the answer (`triage_decided`'s `action:
    /// recover`, within the alert's [`MAX_RECOVERY_ATTEMPTS`]). The ask is
    /// closed with it. An ask whose task is no longer in progress, or has a
    /// newer run, has nothing left to apply and is closed. A free answer is
    /// a person's to read.
    pub(super) fn apply_triage_answers(&mut self) -> Result<()> {
        for ask in self.queue.triage_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            // Only an option the ask offered: a run whose resumes are used
            // up is not offered `resume`.
            if !ask.options.contains(&answer)
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
                    info!(run_id = %decided.id(), task_id = %decided.task_id(), ask_id = %ask.id, "run {} of task {}: {answer} as ask {} answered; the run is {}", decided.id(), decided.task_id(), ask.id, decided.status().as_str());
                    self.clean_task_worktrees(decided.task_id());
                }
                Err(error) => {
                    warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id)
                }
            }
        }
        Ok(())
    }
    /// Start the recovery job of `failed` / `interrupted` runs no round
    /// took since their last resume, or whose round's `wait` is over,
    /// while slots are free (ADR-0047 decision 39). The alert is that of
    /// the run's latest `recovery_requested` since its last resume
    /// (`resume_exhausted`, or the alert a `wait` or a stopped round left),
    /// else its status; a round records its own request unless one is
    /// pending. A run someone leases (a
    /// session still asked to exit) waits. An alert that got its
    /// [`MAX_RECOVERY_ATTEMPTS`] jobs is escalated without one, and a job
    /// that cannot even start fails its round right away.
    pub(super) fn triage_runs(&mut self, parallel: usize) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.runs_to_triage()? {
            if self.used_slots() >= parallel {
                break;
            }
            let events = self.queue.run_events(run.id())?;
            let due = match triage_state(&events) {
                TriageState::Pending => true,
                TriageState::Waiting { until } => until <= now,
                TriageState::Failed | TriageState::Finished => false,
            };
            if !due
                || self
                    .queue
                    .run_lease(run.id())?
                    .is_some_and(|lease| !self.lease_stale(&lease, now))
            {
                continue;
            }
            let pending = pending_request(&events);
            let alert =
                current_alert(&events).unwrap_or_else(|| RecoveryAlert::of_ended(run.status()));
            let done = attempts(&events, alert);
            let (request, attempt) = match pending {
                Some(_) => (None, done),
                None if done >= MAX_RECOVERY_ATTEMPTS => (None, done + 1),
                None => {
                    let evidence: Vec<EventId> = events
                        .iter()
                        .rev()
                        .find(|e| e.payload["status"] == run.status().as_str())
                        .map(|e| e.id)
                        .into_iter()
                        .collect();
                    let mut request = json!({
                        "alert": alert,
                        "attempt": done + 1,
                        "status": run.status().as_str(),
                        "evidence": evidence,
                        "last_error": run.last_error(),
                    });
                    // A person chose one of the last job's own options.
                    if let Some(decided) = events
                        .iter()
                        .rev()
                        .take_while(|e| e.kind != "triage_started")
                        .find(|e| {
                            e.kind == "triage_decided"
                                && e.payload["action"] == crate::domain::RECOVER_AGAIN
                        })
                    {
                        request["person_answer"] = json!({
                            "ask_id": decided.payload["ask_id"],
                            "answer": decided.payload["answer"],
                        });
                    }
                    (Some(request), done + 1)
                }
            };
            let Some((run, round)) = self.queue.begin_triage(run.id(), &self.token, request)?
            else {
                continue;
            };
            if attempt > MAX_RECOVERY_ATTEMPTS {
                let used = attempt - 1;
                if let Err(error) =
                    self.escalate_ended(&run, round, alert, used, Escalation::UsedUp(used), 0)
                {
                    self.fail_recovery(&run, round, alert, used, format!("{error:#}"), 0);
                }
                let run = self.queue.run(run.id())?;
                self.note_triaged(&run);
                continue;
            }
            match self.spawn_ended(&run, round, alert, attempt) {
                Ok(watch) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} ({}) recovery job {attempt} for {} started (round {round})", run.id(), run.task_id(), run.status().as_str(), alert.as_str());
                    self.slots.push(Slot::new(run, Phase::Recovery(watch)));
                }
                Err(error) => {
                    let error = format!("the recovery job could not start: {error:#}");
                    self.fail_recovery(&run, round, alert, attempt, error, 0);
                    let run = self.queue.run(run.id())?;
                    self.note_triaged(&run);
                }
            }
        }
        Ok(())
    }
    /// Write the recovery job's prompt for a run that ended and start the
    /// headless job in the run's directory, allowed to read only
    /// (ADR-0047 decision 39: the task, the run's error, receipt, logs,
    /// final screen and events, the processes left in its worktree, its
    /// git state and earlier repairs).
    fn spawn_ended(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
    ) -> Result<EndedRecovery> {
        let dir = match &run.run_dir() {
            Some(dir) => PathBuf::from(dir),
            None => self.layout.runs_dir.join(run.id().as_str()),
        };
        let detail = self.queue.show(run.task_id())?;
        let events = self.queue.run_events(run.id())?;
        let resumes = ResumeCount::of(&events);
        let ended = ended_run_material(&*self.files, &detail, run, resumes, &dir);
        let facts = events
            .iter()
            .rev()
            .find(|e| e.kind == "recovery_requested")
            .map_or_else(|| json!({"alert": alert}), |e| e.payload.clone());
        let processes = match run.worktree_path().map(Path::new) {
            Some(worktree) if self.files.is_dir(worktree) => self
                .processes
                .list()
                .map(|all| {
                    run_processes(&all, worktree, None, None, std::process::id())
                        .into_iter()
                        .cloned()
                        .collect()
                })
                .map_err(|error| format!("{error:#}")),
            _ => Err("the run has no worktree".to_owned()),
        };
        let (status, head, receipt) = git_facts(self, run)?;
        let history = repair_history(self, run)?;
        let workspace = run.workspace_id().unwrap_or("none").to_owned();
        let material = RecoveryMaterial {
            alert,
            ended: Some(ended),
            facts: &facts,
            workspace: &workspace,
            screen: "(the session is gone: its final screen is above)",
            processes,
            git_status: &status,
            head: &head,
            receipt_commit: receipt.as_deref(),
            history: &history,
            allowed: &ENDED_ACTIONS,
        };
        let prompt = recovery_prompt(&detail.task, run, attempt, &material)?;
        // The session id `triage_started` recorded (ADR-0048 decision 4).
        let session_id = events
            .iter()
            .rev()
            .find(|e| e.kind == "triage_started")
            .and_then(|e| e.payload["session_id"].as_str())
            .map(str::to_owned);
        let job = start_job(self, &dir, alert, attempt, &prompt, session_id.as_deref())?;
        Ok(EndedRecovery {
            round,
            alert,
            attempt,
            job,
        })
    }
    /// Act on the recovery job's verdict for a run that ended (ADR-0047
    /// decision 40): a `repair` of high confidence whose one action's
    /// preconditions hold now is applied and recorded as `auto_repaired`
    /// (`layer: recovery`, but for `wait`), `triage_finished` (its action)
    /// and `recovery_finished`; anything else becomes the `decide` ask.
    /// Then the workspaces the run left open are closed and the lease is
    /// released.
    pub(super) fn act_on_recovery(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        duration_secs: u64,
        verdict: RecoveryVerdict,
    ) -> Result<TaskRun> {
        ensure!(
            self.queue.holds_lease(run.id(), &self.token)?,
            "the recovery job's lease of run {} was lost",
            run.id()
        );
        if !verdict.applies() {
            return self.escalate_ended(
                run,
                round,
                alert,
                attempt,
                Escalation::Verdict(verdict),
                duration_secs,
            );
        }
        let (action, conditions) = match self.plan_ended(run, &verdict) {
            Ok(planned) => planned,
            Err(why) => {
                warn!(run_id = %run.id(), "run {}: recovery job {attempt} of {} answered repair, but {why}; asking the inbox", run.id(), alert.as_str());
                return self.escalate_ended(
                    run,
                    round,
                    alert,
                    attempt,
                    Escalation::Refused(verdict, why),
                    duration_secs,
                );
            }
        };
        let name = verdict.actions[0].name();
        let payload = json!({
            "attempt": round,
            "alert": alert,
            "recovery_attempt": attempt,
            "verdict": verdict.verdict,
            "confidence": verdict.confidence,
            "reason": verdict.diagnosis,
            "duration_secs": duration_secs,
        });
        let mut also = Vec::new();
        if !matches!(action, TriageAction::Wait { .. }) {
            also.push((
                "auto_repaired",
                json!({
                    "layer": "recovery",
                    "repair": name,
                    "alert": alert,
                    "attempt": attempt,
                    "conditions": conditions,
                }),
            ));
        }
        also.push((
            "recovery_finished",
            json!({
                "alert": alert,
                "attempt": attempt,
                "verdict": verdict.verdict,
                "confidence": verdict.confidence,
                "diagnosis": verdict.diagnosis,
                "applied": [name],
                "escalated": false,
                "recheck_at": match &action {
                    TriageAction::Wait { recheck_at } => Some(*recheck_at),
                    _ => None,
                },
                "duration_secs": duration_secs,
            }),
        ));
        let finished = self
            .queue
            .finish_triage(run.id(), &self.token, &action, payload, also)?;
        info!(run_id = %run.id(), "run {}: recovery job {attempt} of {} repaired it ({name}): {}; the run is {}", run.id(), alert.as_str(), verdict.diagnosis, finished.status().as_str());
        self.end_round(&finished)
    }
    /// The action of a `repair` for a run that ended, once its
    /// preconditions hold now (ADR-0047 decision 40), with the values
    /// checked; `Err` says which one does not. Exactly one of
    /// [`ENDED_ACTIONS`]: `retry` only for a run whose branch holds no
    /// commit of its own and a task that did not fail
    /// [`TRIAGE_RETRY_FAILURES`] times, `retry_inherit` only for one with
    /// commits and once per task, `resume` only with resumes left and a
    /// worktree, `wait` for at most [`MAX_RECHECK_SECS`].
    fn plan_ended(
        &mut self,
        run: &TaskRun,
        verdict: &RecoveryVerdict,
    ) -> std::result::Result<(TriageAction, Value), String> {
        let [action] = verdict.actions.as_slice() else {
            return Err(format!(
                "a run that ended takes exactly one action of {}, not {}",
                ENDED_ACTIONS.join(", "),
                verdict
                    .actions
                    .iter()
                    .map(RecoveryAction::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let unreadable = |error: anyhow::Error| format!("{error:#}");
        match action {
            RecoveryAction::Retry => {
                let failures = self
                    .queue
                    .show(run.task_id())
                    .map_err(unreadable)?
                    .runs
                    .iter()
                    .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
                    .count();
                if failures >= TRIAGE_RETRY_FAILURES {
                    return Err(format!(
                        "task {} has {failures} failed or interrupted runs, so it is not retried without a person",
                        run.task_id()
                    ));
                }
                let own = self.own_commits(run).map_err(|error| {
                    format!("whether its branch holds commits could not be read: {error:#}")
                })?;
                if own {
                    return Err("its branch holds commits of its own, which a retry from scratch would throw away (retry_inherit carries them over; throwing them away is a person's call)".to_owned());
                }
                Ok((
                    TriageAction::Retry,
                    json!({"failures": failures, "own_commits": false}),
                ))
            }
            RecoveryAction::RetryInherit => {
                let detail = self.queue.show(run.task_id()).map_err(unreadable)?;
                if detail
                    .events
                    .iter()
                    .any(crate::domain::resume::is_inherit_retry)
                {
                    return Err(format!(
                        "task {} was retried with a branch carried over already (once per task)",
                        run.task_id()
                    ));
                }
                let own = self.own_commits(run).map_err(|error| {
                    format!("whether its branch holds commits could not be read: {error:#}")
                })?;
                let head = match self.inherited_head(run) {
                    Ok(Some(head)) if own => head,
                    Ok(_) => {
                        return Err(
                            "its branch holds no commit of its own to carry over".to_owned()
                        );
                    }
                    Err(error) => {
                        return Err(format!(
                            "its branch could not be kept for a retry: {error:#}"
                        ));
                    }
                };
                let branch = run.branch().map(str::to_owned);
                Ok((
                    TriageAction::RetryInherit {
                        branch: branch.clone(),
                        head: head.clone(),
                    },
                    json!({"own_commits": true, "branch": branch, "head": head, "first_in_task": true}),
                ))
            }
            RecoveryAction::Resume { instruction } => {
                let resumes =
                    ResumeCount::of(&self.queue.run_events(run.id()).map_err(unreadable)?);
                if resumes.exhausted() {
                    return Err(format!(
                        "its resumes are used up ({} counted of at most {MAX_RESUME_ATTEMPTS}, {} after conflicts only)",
                        resumes.counted, resumes.conflict_only
                    ));
                }
                let worktree = run
                    .worktree_path()
                    .is_some_and(|path| self.files.is_dir(Path::new(path)));
                if !worktree || run.receipt_path().is_none() {
                    return Err("the run has no worktree a session could resume in".to_owned());
                }
                let instruction = if instruction.trim().is_empty() {
                    verdict.diagnosis.clone()
                } else {
                    instruction.trim().to_owned()
                };
                Ok((
                    TriageAction::Resume { instruction },
                    json!({"counted_resumes": resumes.counted, "resumes": resumes.total(), "worktree": true}),
                ))
            }
            RecoveryAction::Wait { recheck_after_secs } => {
                let secs = (*recheck_after_secs).min(MAX_RECHECK_SECS);
                Ok((
                    TriageAction::Wait {
                        recheck_at: self.generators.clock.now() + i64::try_from(secs).unwrap_or(0),
                    },
                    json!({"recheck_after_secs": secs}),
                ))
            }
            other => Err(format!(
                "{} does not apply to a run that ended: its session is gone (allowed: {})",
                other.name(),
                ENDED_ACTIONS.join(", ")
            )),
        }
    }
    /// Whether the run's branch holds commits of its own: its reviewed
    /// commit, else its worktree's HEAD, else its branch, is not on main.
    /// A run that never got a worktree has none; one whose worktree is
    /// gone without a reviewed commit cannot be told.
    fn own_commits(&self, run: &TaskRun) -> Result<bool> {
        let worktree = run
            .worktree_path()
            .map(Path::new)
            .filter(|path| self.files.is_dir(path));
        let head = match (run.result_commit(), worktree) {
            (Some(commit), _) => commit.to_string(),
            (None, Some(worktree)) => self.repository.head(worktree)?.to_string(),
            (None, None) if !self.queue.has_run_event(run.id(), "worktree_created")? => {
                return Ok(false);
            }
            (None, None) => bail!("its worktree is gone and it has no reviewed commit"),
        };
        let main = self.repository.main_head()?;
        Ok(!self.repository.is_ancestor(&head, main.as_str())?)
    }
    /// Escalate the alert of a run that ended to the inbox (ADR-0047
    /// decision 40): a `decide` ask with the job's diagnosis and why a
    /// person is needed, the options `retry` / `resume` / `cancel`
    /// (without `resume` once its resumes are used up) and the job's, and
    /// the job's reason category; recorded as `triage_finished` (action
    /// `ask`) and `recovery_finished`. The supervisor applies the answer
    /// ([`Self::apply_triage_answers`]).
    fn escalate_ended(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        escalation: Escalation,
        duration_secs: u64,
    ) -> Result<TaskRun> {
        let note = escalation.note(run, alert, attempt);
        let exhausted = ResumeCount::of(&self.queue.run_events(run.id())?).exhausted();
        let base = if exhausted {
            EXHAUSTED_OPTIONS
        } else {
            TRIAGE_OPTIONS
        };
        let mut options: Vec<String> = base.iter().map(|o| (*o).to_owned()).collect();
        for option in &note.options {
            if !options.contains(option) {
                options.push(option.clone());
            }
        }
        let resume = if exhausted {
            ""
        } else {
            " resume: resume the run's own session with the job's diagnosis."
        };
        let others = if note.options.is_empty() {
            ""
        } else {
            " Any other option goes back to the recovery job, which runs again with your choice."
        };
        let question = format!(
            "The supervisor's recovery job for run {run_id} (task {task_id}, {status}; alert: {alert}) did not move it on: {why}.\n{text}\nLast error: {last_error}\nretry: make the task ready for a new run from scratch (this run's work is not carried over).{resume} cancel: cancel the task.{others}",
            run_id = run.id(),
            task_id = run.task_id(),
            status = run.status().as_str(),
            alert = alert.as_str(),
            why = note.why,
            text = note.text,
            last_error = or_none(tail(run.last_error().unwrap_or_default(), 500)),
        );
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: alert.ask_kind(),
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: TRIAGE_ASKER.to_owned(),
                reason_category: note.category,
                finding_id: None,
            },
            self.cmux,
        )?;
        let ask_id = outcome["id"]
            .as_i64()
            .map(AskId::new)
            .context("ask returned no id")?;
        let verdict = escalation.verdict();
        let payload = json!({
            "attempt": round,
            "alert": alert,
            "recovery_attempt": attempt,
            "verdict": verdict.map(|v| v.verdict),
            "confidence": verdict.map(|v| v.confidence),
            "reason": match verdict {
                Some(verdict) => format!("{}: {}", note.why, verdict.diagnosis),
                None => note.why.clone(),
            },
            "duration_secs": duration_secs,
        });
        let finished = escalation.finished(
            alert,
            attempt,
            &note,
            Some(ask_id),
            json!({"duration_secs": duration_secs}),
        );
        let asked = match self.queue.finish_triage(
            run.id(),
            &self.token,
            &TriageAction::Ask { ask_id },
            payload,
            vec![("recovery_finished", finished)],
        ) {
            Ok(asked) => asked,
            Err(error) => {
                // The round fails (`triage_failed`): an ask this pass opened
                // would ask a person twice, so it is withdrawn.
                if outcome["created"] == true {
                    self.queue.answer_as(
                        ask_id,
                        "withdrawn: the recovery round could not be recorded",
                        crate::domain::ANSWERED_BY_RUNTIME,
                    )?;
                    self.queue.close_ask(ask_id)?;
                }
                return Err(error);
            }
        };
        warn!(ask_id = %ask_id, run_id = %run.id(), "run {}: {}; decide ask {ask_id} (notified: {})", run.id(), note.why, outcome["notified"]);
        self.end_round(&asked)
    }
    /// The end of a round whose outcome is recorded: the workspaces the run
    /// left open are closed and the lease is released. What fails here is
    /// logged, not a failed round.
    fn end_round(&mut self, run: &TaskRun) -> Result<TaskRun> {
        if let Err(error) = self.close_open_workspaces(run, WorkspaceCloser::Triage) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
        }
        if let Err(error) = self.queue.release_lease(run.id(), &self.token) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
        }
        self.queue.run(run.id())
    }
    /// Record a round whose recovery job failed (it could not start, exited
    /// non-zero, timed out, printed no verdict, or its outcome could not be
    /// applied) as `triage_failed`, the attention a person recovers the run
    /// from by hand, and `recovery_finished` (`outcome: job_failed`), and
    /// give the lease back; the run stays as it is (ADR-0047 decision 40).
    pub(super) fn fail_recovery(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        error: String,
        duration_secs: u64,
    ) {
        warn!(run_id = %run.id(), error = %error, "run {} recovery job {attempt} of {} failed: {error}; the run waits to be recovered by hand", run.id(), alert.as_str());
        for (kind, payload) in [
            (
                "triage_failed",
                json!({
                    "code": ReasonCode::JobFailed,
                    "attempt": round,
                    "alert": alert,
                    "recovery_attempt": attempt,
                    "error": error,
                    "duration_secs": duration_secs,
                    "status": run.status().as_str(),
                }),
            ),
            (
                "recovery_finished",
                json!({
                    "alert": alert,
                    "attempt": attempt,
                    "outcome": "job_failed",
                    "escalated": false,
                    "error": error,
                    "reason_category": AskReason::RecoveryFailed,
                }),
            ),
        ] {
            if let Err(error) = self.queue.record_runtime_event(run.id(), kind, payload) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the failed recovery job: {error:#}", run.id());
            }
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
        self.clean_task_worktrees(run.task_id());
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
