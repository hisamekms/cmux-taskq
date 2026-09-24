//! Resumed sessions of `needs_session` runs (ADR-0019): which runs to
//! resume, the resolution request and the [`ResumeWatch`] of the session.

use super::*;

impl Supervisor<'_> {
    /// Resume `needs_session` runs with attempts left (ADR-0019 decision 1),
    /// oldest first, while slots are free: a run with a lease that is not
    /// stale, or whose last session still runs, is someone's already.
    pub(super) fn resume_parked_runs(&mut self, parallel: usize) -> Result<()> {
        let candidates = self.queue.runs_needing_session()?;
        // A resumed session let go after the exit timeout raised a
        // stuck_exit ask; once it ended nobody needs to answer it, whether
        // or not a slot is free.
        for candidate in &candidates {
            let alive = candidate
                .wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            if !alive {
                for ask in self
                    .queue
                    .close_stuck_exit_asks(candidate.run.id(), STUCK_EXIT_CLOSED)?
                {
                    info!(run_id = %candidate.run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", candidate.run.id(), ask.id);
                }
            }
        }
        for candidate in candidates {
            let ResumeCandidate {
                run,
                lease,
                wrapper,
                attempts,
            } = candidate;
            let now = self.generators.clock.now();
            let session_alive = wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            // A previous session whose wrapper process lives on, however
            // silent, is never joined by a second one on the same worktree.
            if lease.is_some_and(|lease| !self.lease_stale(&lease, now)) || session_alive {
                continue;
            }
            // Out of attempts: a person decides, whether or not a slot is free.
            if attempts >= MAX_RESUME_ATTEMPTS {
                if let Err(error) = self.exhaust_resumes(&run, attempts) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its used-up resumes could not be handed to a person: {error:#}", run.id());
                }
                continue;
            }
            if self.slots.len() >= parallel {
                break;
            }
            self.close_left_resume_workspaces(&run)?;
            let main = self.repository.main_head()?;
            if let Some(head) = self.resolved_head(&run, &main)? {
                self.skip_resume(&run, &head, &main)?;
                continue;
            }
            let (reason, kind) = resume_reason(&*self.queue, &run)?;
            let Some((run, attempt)) = self.queue.begin_resume(
                run.id(),
                &self.token,
                &main,
                reason.as_deref(),
                MAX_RESUME_ATTEMPTS,
            )?
            else {
                continue;
            };
            let request = ResumeRequest {
                main,
                reason: reason.unwrap_or_else(|| "(no reason recorded)".to_owned()),
                kind,
            };
            match self.start_resume(&run, attempt, &request) {
                Ok(watch) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} resumed (attempt {attempt} of {MAX_RESUME_ATTEMPTS}) in workspace {}", run.id(), run.task_id(), watch.workspace);
                    self.slots.push(Slot {
                        run,
                        phase: Phase::Resume(watch),
                    });
                }
                Err(error) => {
                    let message = format!("run {} could not be resumed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.give_up_resume(
                        &run,
                        attempt,
                        None,
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                }
            }
        }
        Ok(())
    }
    /// The worktree head of a `needs_session` run an earlier resume already
    /// resolved although it was not judged so (its session rewrote the
    /// receipt before the attempt that saw it, or went idle without
    /// rewriting it again): the run was last parked by the landing or
    /// validation (not a person's `send_back`, `landing_decided`), the last
    /// of the parking events, `resume_finished` and `resume_skipped` is a
    /// `resume_finished` with `outcome: unresolved` (a session ran; a resume
    /// that could not start changed nothing), the receipt parses with
    /// this run's `run_id`, `succeeded` and the task's required evidence,
    /// its `commit` is the head of a clean worktree, and that head has
    /// `main` as a proper ancestor. `None` whenever one of these fails or
    /// cannot be read, and the run is resumed as before. Back in
    /// `needs_session` after a skip, however it got there, the run needs a
    /// resume first, so a skip never repeats without one.
    pub(super) fn resolved_head(
        &mut self,
        run: &TaskRun,
        main: &CommitSha,
    ) -> Result<Option<CommitSha>> {
        const PARKING: [&str; 5] = [
            "integration_deferred",
            "integration_error",
            "evidence_missing",
            "scope_violation",
            "landing_decided",
        ];
        let events = self.queue.run_events(run.id())?;
        let parked = events
            .iter()
            .rev()
            .find(|e| PARKING.contains(&e.kind.as_str()));
        let last = events.iter().rev().find(|e| {
            PARKING.contains(&e.kind.as_str())
                || matches!(e.kind.as_str(), "resume_finished" | "resume_skipped")
        });
        if parked.is_none_or(|e| e.kind == "landing_decided")
            || last
                .is_none_or(|e| e.kind != "resume_finished" || e.payload["outcome"] != "unresolved")
        {
            return Ok(None);
        }
        let (Some(worktree), Some(receipt_path)) = (&run.worktree_path(), &run.receipt_path())
        else {
            return Ok(None);
        };
        let Some(receipt) = self
            .files
            .read_to_string(Path::new(receipt_path))
            .ok()
            .and_then(|text| Receipt::parse(&text).ok())
        else {
            return Ok(None);
        };
        let task = self.queue.show(run.task_id())?.task;
        if receipt.run_id != *run.id().as_str()
            || receipt.result != ReceiptResult::Succeeded
            || !receipt
                .missing_evidence(task.required_evidence())
                .is_empty()
        {
            return Ok(None);
        }
        let worktree = Path::new(worktree);
        let Ok(head) = self.repository.head(worktree) else {
            return Ok(None);
        };
        let resolved = head.as_str() == receipt.commit.to_ascii_lowercase()
            && head != *main
            && self
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty())
            && self
                .repository
                .is_ancestor(main.as_str(), head.as_str())
                .unwrap_or(false);
        Ok(resolved.then_some(head))
    }
    /// Move a run [`Self::resolved_head`] found resolved on without opening
    /// a session or using an attempt: record `resume_skipped` and, under
    /// its lease, land it when its integrate was approved, or validate and
    /// review it (with no session to keep) otherwise.
    pub(super) fn skip_resume(
        &mut self,
        run: &TaskRun,
        head: &CommitSha,
        main: &CommitSha,
    ) -> Result<()> {
        let approved = self.queue.has_run_event(run.id(), "integration_approved")?;
        let Some(run) = self
            .queue
            .skip_resume(run.id(), &self.token, head, main, approved)?
        else {
            return Ok(());
        };
        info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} was already resolved at {head} on main {main}; {} without a resume", run.id(), run.task_id(), if approved {
            "landing it"
        } else {
            "validating it"
        });
        let phase = if approved {
            Phase::AwaitingSlot
        } else {
            Phase::Validating(Some(self.validate(run.clone())), None)
        };
        self.slots.push(Slot { run, phase });
        Ok(())
    }
    /// Hand a `needs_session` run whose resumes are used up to a person
    /// through the triage's `decide` ask (ADR-0024's Consequences): no
    /// headless triage runs, since resuming is no longer an option and a
    /// run that did not resolve in [`MAX_RESUME_ATTEMPTS`] sessions is not
    /// retried without a person. The ask (options `retry` and `cancel`,
    /// applied like a triage's answer) is opened first, then the run becomes
    /// `failed` with `triage_finished` naming the ask, and the workspaces it
    /// left open are closed as after a triage. A run of a task that moved
    /// on is left alone.
    pub(super) fn exhaust_resumes(&mut self, run: &TaskRun, attempts: usize) -> Result<()> {
        let detail = self.queue.show(run.task_id())?;
        if detail.task.status() != TaskStatus::InProgress
            || detail
                .runs
                .last()
                .is_some_and(|latest| *latest.id() != *run.id())
        {
            return Ok(());
        }
        let last_error = run.last_error().map(str::to_owned).unwrap_or_default();
        let reason = format!(
            "resumed {attempts} times (at most {MAX_RESUME_ATTEMPTS}) and still needs a session: {}",
            tail(&last_error, 500)
        );
        let mut question = format!(
            "Run {} of task {} ({}) was resumed {attempts} times (at most {MAX_RESUME_ATTEMPTS}) and still needs a session, so the supervisor stops resuming it.\nLast error: {}",
            run.id(),
            run.task_id(),
            detail.task.title(),
            or_none(tail(&last_error, 500))
        );
        if let Some(run_dir) = &run.run_dir() {
            question.push_str(&format!("\nRun directory: {run_dir}"));
        }
        question.push_str(
            "\nretry: make the task ready for a new run. cancel: cancel the task. To change the task first, answer with what to change instead.",
        );
        let outcome = ask::ask(
            &mut *self.queue,
            &self.layout.repo_root,
            NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: EXHAUSTED_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: TRIAGE_ASKER.to_owned(),
            },
            self.cmux,
        )?;
        let ask_id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let Some(failed) =
            self.queue
                .exhaust_resumes(run.id(), MAX_RESUME_ATTEMPTS, ask_id, &reason)?
        else {
            // The run changed meanwhile (another supervisor took it): an ask
            // this pass opened has nothing left to decide.
            if outcome["created"] == true {
                self.queue.answer(
                    ask_id,
                    "withdrawn: the run changed before it was handed over",
                )?;
                self.queue.close_ask(ask_id)?;
            }
            return Ok(());
        };
        warn!(run_id = %failed.id(), task_id = %failed.task_id(), "run {} of task {} used up its resumes; it is failed and waits for ask {ask_id}", failed.id(), failed.task_id());
        self.close_triaged_workspaces(&failed)?;
        self.note_triaged(&failed);
        Ok(())
    }
    /// Close the resume workspaces earlier attempts of this run left open
    /// (a session let go after the exit timeout, or one that might have
    /// lived when a resume failed), found by the IDs recorded in its
    /// `resume_finished` events (ADR-0026). The caller checked that no
    /// session of the run is alive.
    pub(super) fn close_left_resume_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let left: Vec<String> = self
            .queue
            .run_events(run.id())?
            .iter()
            .filter(|e| e.kind == "resume_finished" && e.payload["workspace_closed"] != true)
            .filter_map(|e| e.payload.get("workspace_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        for workspace in left {
            if self.cmux.exists(&workspace)? {
                info!(run_id = %run.id(), "run {}: closing resume workspace {workspace} left by an earlier attempt; its session has ended", run.id());
                self.cmux.close(&workspace)?;
            }
        }
        Ok(())
    }
    /// Write the resolution request, refresh the runtime snapshot (the one
    /// the worker ran may predate `session --resume`) and open the resume
    /// workspace with the same wrapper and settings as the worker's.
    pub(super) fn start_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        request: &ResumeRequest,
    ) -> Result<ResumeWatch> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        ensure!(
            self.files.is_dir(worktree),
            "worktree {} is missing",
            worktree.display()
        );
        let task = self.queue.show(run.task_id())?.task;
        let landed = landed_since(
            &mut *self.queue,
            &*self.repository,
            &*self.files,
            run,
            &request.main,
        )?;
        let message = resume_request(&task, run, request, &landed)?;
        self.files.write(
            &run_dir.join(format!("resume-{attempt}.txt")),
            message.as_bytes(),
        )?;
        self.files
            .copy(&self.layout.runner, &run_dir.join("runner"))
            .context("snapshot runtime binary")?;
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
            "--resume".into(),
        ]);
        // The worker's env and group (the same session of the run) and the
        // description `run <run-id> resume` (ADR-0028).
        let tags = WorkspaceTags {
            env: self.layout.worker_env.clone(),
            description: Some(resume_workspace_description(run)),
            group: self.workspace_group(),
        };
        let workspace = self.cmux.create_resume(&task, run, &command, &tags)?;
        Ok(ResumeWatch {
            workspace,
            attempt,
            run_dir,
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at: self.files.now(),
            startup: Instant::now(),
            message,
            agent_seen: None,
            message_sent: None,
            exit_requested: None,
            required_evidence: task.required_evidence().to_vec(),
            approved: self.queue.has_run_event(run.id(), "integration_approved")?,
            silent: false,
            exit_for_silence: false,
        })
    }
    /// The resumed session ended, or resolved the run: record
    /// `resume_finished` and move the run on. A resolved run whose
    /// integrate was approved has exited; its workspace is closed and it
    /// keeps its lease and waits for the landing slot. An unapproved
    /// resolved run keeps its session and lease and goes through
    /// validation and review like the worker's (ADR-0027 decision 3); a
    /// `failed` receipt ends the run; anything else leaves it
    /// `needs_session` for the next attempt, or for a human after the last.
    pub(super) fn finish_resumed_session(
        &mut self,
        slot: &mut Slot,
        attempt: usize,
        workspace: &str,
        verdict: ResumeVerdict,
    ) -> Result<Step> {
        let approved = self
            .queue
            .has_run_event(slot.run.id(), "integration_approved")?;
        let reviewed = matches!(verdict.kind, ResumeOutcome::Resolved) && !approved;
        // A session let go after the exit timeout still runs: its
        // workspace stays, and blocks the next attempt until it ends. A
        // session going on to review keeps it until the verdict.
        let closed = !verdict.exit_timed_out
            && !reviewed
            && match self.cmux.close(workspace) {
                Ok(()) => true,
                Err(error) => {
                    warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: resume workspace {workspace} could not be closed: {error:#}", slot.run.id());
                    false
                }
            };
        let mut payload = json!({
            "attempt": attempt,
            "outcome": verdict.outcome(),
            "head": verdict.head,
            "workspace_id": workspace,
            "workspace_closed": closed,
            "approved": approved,
        });
        if verdict.exit_timed_out {
            payload["exit_timed_out"] = json!(true);
        }
        if reviewed {
            payload["session_live"] = json!(verdict.live);
        }
        let id = slot.run.id().clone();
        let run = match verdict.kind {
            ResumeOutcome::Resolved if approved => {
                let run = self
                    .queue
                    .finish_resume(&id, &self.token, None, None, true, payload)?;
                slot.run = run;
                slot.phase = Phase::AwaitingSlot;
                return Ok(Step::Continue);
            }
            ResumeOutcome::Resolved => {
                let run = self.queue.finish_resume(
                    &id,
                    &self.token,
                    Some(RunStatus::Validating),
                    None,
                    true,
                    payload,
                )?;
                let handle = self.validate(run.clone());
                slot.run = run;
                slot.phase = Phase::Validating(
                    Some(handle),
                    Some(SessionRef {
                        workspace: workspace.to_owned(),
                        resume: Some(attempt),
                    }),
                );
                return Ok(Step::Continue);
            }
            ResumeOutcome::Failed(reason) => self.queue.finish_resume(
                &id,
                &self.token,
                Some(RunStatus::Failed),
                Some(&reason),
                false,
                Reason::new(ReasonCode::WorkerFailed).on(payload),
            )?,
            ResumeOutcome::Unresolved => {
                payload["exhausted"] = json!(attempt >= MAX_RESUME_ATTEMPTS);
                self.queue
                    .finish_resume(&id, &self.token, None, None, false, payload)?
            }
        };
        Ok(Step::Done(Box::new(run)))
    }
}

/// Why the run waits for a session: the reason of its latest
/// `integration_deferred` / `integration_error` / `evidence_missing` /
/// `scope_violation` / `landing_decided` event (a runtime error since, such
/// as a failed resume, may have replaced `last_error`), else `last_error`;
/// and what kind of request that makes: `evidence_missing` (or a landing
/// deferred for missing evidence, whose payload names the `checks`),
/// `scope_violation` (or a landing deferred for it, whose payload names the
/// paths), a review sent back, the triage's resume (`triage_finished`,
/// whose `instruction` is the reason, or a person's `triage_decided`), or a
/// landing.
pub(super) fn resume_reason(
    queue: &dyn Queue,
    run: &TaskRun,
) -> Result<(Option<String>, ResumeKind)> {
    let events = queue.run_events(run.id())?;
    let parked = events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            "integration_deferred"
                | "integration_error"
                | "evidence_missing"
                | "scope_violation"
                | "landing_decided"
                | "triage_finished"
                | "triage_decided"
        )
    });
    // The triage's resume asks for its `instruction`, not its reason.
    let key = match parked {
        Some(e) if e.kind == "triage_finished" => "instruction",
        _ => "reason",
    };
    let reason = parked
        .and_then(|e| e.payload.get(key).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| run.last_error().map(str::to_owned));
    let kind = match parked {
        Some(e) if e.kind == "evidence_missing" || e.payload.get("checks").is_some() => {
            ResumeKind::EvidenceMissing
        }
        Some(e) if e.kind == "scope_violation" || e.payload.get("scope_violation").is_some() => {
            ResumeKind::ScopeViolation
        }
        Some(e) if e.kind == "landing_decided" => ResumeKind::SentBack,
        Some(e) if e.kind.starts_with("triage_") => ResumeKind::Triage,
        _ => ResumeKind::Landing,
    };
    Ok((reason, kind))
}

/// The tasks landed on `main` since the run's base, oldest first, from the
/// `Dagq-Task` trailers, each with its integrated run's receipt summary.
pub(super) fn landed_since(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    files: &dyn RunFiles,
    run: &TaskRun,
    main: &CommitSha,
) -> Result<Vec<PredecessorSummary>> {
    let mut landed = Vec::new();
    for task_id in repository.landed_task_ids(run.base_commit().as_str(), main.as_str())? {
        let Ok(detail) = queue.show(task_id) else {
            continue;
        };
        let integrated_run = detail
            .runs
            .iter()
            .rev()
            .find(|r| r.status() == RunStatus::Integrated)
            .cloned();
        landed.push(PredecessorSummary::from_predecessor(
            files,
            &Predecessor {
                task: detail.task,
                integrated_run,
            },
        ));
    }
    Ok(landed)
}

/// The options of the `decide` ask of a run whose resumes are used up: a
/// subset of [`TRIAGE_OPTIONS`], applied the same way.
pub(super) const EXHAUSTED_OPTIONS: &[&str] = &["retry", "cancel"];

/// Watches one resumed session: its wrapper registration, the single
/// resolution request once its agent is up, the rewritten receipt and the
/// idle marker, the single `/exit`, and the wrapper's exit.
pub(super) struct ResumeWatch {
    pub(super) workspace: String,
    pub(super) attempt: usize,
    pub(super) run_dir: PathBuf,
    pub(super) receipt_path: PathBuf,
    pub(super) idle_marker: PathBuf,
    /// A receipt no newer than this is the one from before the resume.
    /// Like every time compared with a file's mtime (`sent_at` of a revise
    /// or conflict request, `message_sent`), it is read from the wall clock
    /// that stamps the files, not from the injected [`Clock`].
    pub(super) started_at: SystemTime,
    pub(super) startup: Instant,
    pub(super) message: String,
    pub(super) agent_seen: Option<Instant>,
    /// When the resolution request was sent (for its timeout, and for the
    /// idle marker of the response to it).
    pub(super) message_sent: Option<(Instant, SystemTime)>,
    pub(super) exit_requested: Option<Instant>,
    /// The task's required checks: a rewritten receipt still without them
    /// has not resolved the run.
    pub(super) required_evidence: Vec<EvidenceCheck>,
    /// Its integrate was called: resolved, it exits and lands without a
    /// review; otherwise it stays open for validation and review.
    pub(super) approved: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
}

/// What a resumed session left behind when it exited.
pub(super) enum ResumeOutcome {
    /// A rewritten `succeeded` receipt names the worktree head.
    Resolved,
    /// A rewritten receipt reports `failed`; the reason for `last_error`.
    Failed(String),
    /// Anything else: no rewritten receipt, or one for another commit.
    Unresolved,
}

pub(super) struct ResumeVerdict {
    pub(super) kind: ResumeOutcome,
    pub(super) head: Option<CommitSha>,
    /// The session did not exit within the exit timeout of `/exit`: it is
    /// let go (still running, its workspace kept) so the slot and the lease
    /// are not held forever.
    pub(super) exit_timed_out: bool,
    /// The session resolved the run and is still running, never asked to
    /// exit: it goes on to validation and review (ADR-0027 decision 3).
    pub(super) live: bool,
}

impl ResumeVerdict {
    pub(super) fn outcome(&self) -> &'static str {
        match self.kind {
            ResumeOutcome::Resolved => "resolved",
            ResumeOutcome::Failed(_) => "failed",
            ResumeOutcome::Unresolved => "unresolved",
        }
    }
}

impl ResumeWatch {
    /// The receipt the session rewrote during this resume, if any.
    pub(super) fn rewritten_receipt(&self, files: &dyn RunFiles) -> Option<Receipt> {
        let modified = files.modified(&self.receipt_path).ok()?;
        if modified <= self.started_at {
            return None;
        }
        Receipt::parse(&files.read_to_string(&self.receipt_path).ok()?).ok()
    }

    /// `head` is the worktree's HEAD when the worktree is clean, `None`
    /// otherwise: a resolved receipt must name a clean head.
    pub(super) fn verdict(
        &self,
        files: &dyn RunFiles,
        run: &TaskRun,
        head: Option<&CommitSha>,
    ) -> ResumeOutcome {
        match self.rewritten_receipt(files) {
            Some(receipt) if receipt.run_id != *run.id().as_str() => ResumeOutcome::Unresolved,
            Some(receipt) if receipt.result == ReceiptResult::Failed => ResumeOutcome::Failed(
                format!("session reported the run as failed: {}", receipt.summary),
            ),
            Some(receipt)
                if head
                    .is_some_and(|head| head.as_str() == receipt.commit.to_ascii_lowercase())
                    && receipt.missing_evidence(&self.required_evidence).is_empty() =>
            {
                ResumeOutcome::Resolved
            }
            _ => ResumeOutcome::Unresolved,
        }
    }

    /// One observation; `Some` once the wrapper exited.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<ResumeVerdict>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "resumed session's wrapper did not register within {} seconds",
                timeout.as_secs()
            );
            return Ok(None);
        };
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        if wrapper.exited_at.is_some() {
            match sv.cmux.capture(&self.workspace) {
                Ok(screen) => sv.files.write(
                    &self
                        .run_dir
                        .join(format!("terminal-resume-{}.txt", self.attempt)),
                    screen.as_bytes(),
                )?,
                Err(error) => sv.queue.record_runtime_event(
                    run.id(),
                    "screen_capture_failed",
                    reason_of_error(&error, ReasonCode::BackendFailed)
                        .on(json!({"error": format!("{error:#}")})),
                )?,
            }
            let head = sv.repository.head(worktree).ok();
            let clean = sv
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty());
            return Ok(Some(ResumeVerdict {
                kind: self.verdict(&*sv.files, run, head.as_ref().filter(|_| clean)),
                head,
                exit_timed_out: false,
                live: false,
            }));
        }
        let pulse = wrapper_pulse(
            sv,
            run,
            wrapper,
            &self.workspace,
            &mut self.silent,
            "resumed session's wrapper heartbeat expired; session may still be alive",
        )?;
        if matches!(pulse, WrapperPulse::Exited) {
            return Ok(None);
        }
        if matches!(pulse, WrapperPulse::Silent) && self.exit_requested.is_none() {
            // Ask once, the way a person would; never kill the session.
            sv.cmux.send_exit(&self.workspace)?;
            warn!(run_id = %run.id(), "resumed session of {} lost its wrapper heartbeat; exit requested", run.id());
            self.exit_requested = Some(Instant::now());
            self.exit_for_silence = true;
        }
        if let Some(requested) = self.exit_requested {
            if requested.elapsed() >= sv.cmux.exit_timeout() {
                // /exit is not resent (it could pick a dialog's option).
                warn!(run_id = %run.id(), "resumed session of {} did not exit within {}s of the exit request; letting it go as unresolved (its workspace {} is kept)", run.id(), sv.cmux.exit_timeout().as_secs(), self.workspace);
                // Its dialog stays until someone answers it: raise it to
                // the inbox, as for the worker's session (task 104). The
                // next pass closes the ask once the session ended. A failed
                // ask is only noted: the verdict stands without it.
                let after = stuck_exit_after(
                    self.exit_for_silence,
                    if self.attempt >= MAX_RESUME_ATTEMPTS {
                        "The run stays needs_session after its last resume attempt, and is left to the person once the session exits"
                    } else {
                        "The run stays needs_session, and the supervisor resumes it again once the session exits"
                    },
                );
                if let Err(error) = ask_stuck_exit(sv, run, &self.workspace, &after) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "stuck_exit ask for {} could not be opened: {error:#}", run.id());
                }
                return Ok(Some(ResumeVerdict {
                    kind: ResumeOutcome::Unresolved,
                    head: sv.repository.head(worktree).ok(),
                    exit_timed_out: true,
                    live: false,
                }));
            }
            return Ok(None);
        }
        let Some((sent, sent_at)) = self.message_sent else {
            if processes.iter().any(|p| p.role == "agent") {
                let seen = *self.agent_seen.get_or_insert_with(Instant::now);
                if seen.elapsed() >= sv.cmux.resume_prompt_delay() {
                    sv.cmux.send_text(&self.workspace, &self.message)?;
                    self.message_sent = Some((Instant::now(), sv.files.now()));
                    info!(run_id = %run.id(), "resolution request sent to run {} in workspace {}", run.id(), self.workspace);
                }
            }
            return Ok(None);
        };
        // The idle marker is read before the receipt and the
        // worktree: a receipt rewritten after this read is judged
        // at the next poll, never as idle without it.
        let idle = IdleMarker::read(&*sv.files, sv.signals, &self.idle_marker)?;
        let head = sv.repository.head(worktree)?;
        let clean = sv.repository.status(worktree)?.trim().is_empty();
        // Resolved (or failed) and idle after the receipt; or idle
        // after the request with no such receipt, which a session
        // that could not resolve it (or stopped at a question)
        // never ends by itself; or no idle at all within the
        // resume timeout (a lost request, a dialog, background
        // work that does not end).
        let verdict = self.verdict(&*sv.files, run, clean.then_some(&head));
        let idle_after_receipt = match (&idle, &verdict) {
            (Some(idle), ResumeOutcome::Resolved | ResumeOutcome::Failed(_)) => idle
                .idle_after_receipt(&*sv.files, &self.receipt_path)?
                .is_some(),
            _ => false,
        };
        // An unapproved resolved run keeps its session for
        // validation and review (ADR-0027 decision 3).
        if matches!(verdict, ResumeOutcome::Resolved) && !self.approved && idle_after_receipt {
            info!(run_id = %run.id(), "resumed session of {} rewrote its receipt and went idle (head {head}); validating with the session open", run.id());
            return Ok(Some(ResumeVerdict {
                kind: ResumeOutcome::Resolved,
                head: Some(head),
                exit_timed_out: false,
                live: true,
            }));
        }
        let why = match verdict {
            ResumeOutcome::Unresolved if idle.is_some_and(|idle| idle.idle_since(sent_at)) => {
                Some("went idle without a resolving receipt")
            }
            ResumeOutcome::Unresolved => None,
            _ => idle_after_receipt.then_some("rewrote its receipt and went idle"),
        }
        .or_else(|| {
            (sent.elapsed() >= sv.cmux.resume_timeout())
                .then_some("did not finish within the resume timeout")
        });
        if let Some(why) = why {
            // Ask once, the way a person would; never kill the session.
            sv.cmux.send_exit(&self.workspace)?;
            info!(run_id = %run.id(), "resumed session of {} {why} (head {head}); exit requested", run.id());
            self.exit_requested = Some(Instant::now());
        }
        Ok(None)
    }
}
