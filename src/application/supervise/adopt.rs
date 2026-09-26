//! Adoption (ADR-0012): runs whose supervisor died while their session
//! lives on are taken over, each in the phase it was in.

use super::*;

impl Supervisor<'_> {
    /// Take over `running` / `validating` runs whose lease went stale under
    /// another token while their wrapper is alive (heartbeat within the
    /// lease TTL) or has already reported its exit (ADR-0012), and every
    /// `awaiting_integration` run whose lease went stale, whatever its
    /// wrapper: its review is headless and needs no session, and a session
    /// that died is handled as one that ended (task 236). A `running` /
    /// `validating` run whose wrapper is dead or silent is `recover`'s
    /// business; a run without a lease was abandoned or recovered on
    /// purpose and is never adopted. The staleness is judged here and again
    /// inside `adopt_run`, so two supervisors racing for one run take it
    /// exactly once.
    pub(super) fn adopt_stale_runs(&mut self, parallel: usize) -> Result<()> {
        for candidate in self.queue.runs_leased_by_others(&self.token)? {
            let now = self.generators.clock.now();
            let LeasedRun {
                run,
                lease,
                wrapper,
            } = candidate;
            if !self.lease_stale(&lease, now) {
                continue;
            }
            if !self.adoptable(&run, wrapper.as_ref(), now)? {
                continue;
            }
            // A run that waits for a person needs no free slot while the
            // waits are under their limit (ADR-0062 decision 11).
            let as_waiting = self.adopts_as_waiting(run.id())?;
            if !as_waiting && self.used_slots() >= parallel {
                continue;
            }
            let alive = self.wrapper_alive(wrapper.as_ref(), now);
            let observed = match &wrapper {
                Some(wrapper) => json!({
                    "pid": wrapper.pid,
                    "alive": alive,
                    "exited_at": wrapper.exited_at,
                }),
                None => Value::Null,
            };
            let pid = self.layout.pid;
            let Some(run) =
                self.queue
                    .adopt_run(run.id(), &lease.token, &self.token, pid, observed)?
            else {
                info!(run_id = %run.id(), "run {} was not adopted: its lease changed while judging it", run.id());
                continue;
            };
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} adopted from supervisor {} (pid {}, heartbeat {}s old; wrapper pid {} {}): task {} in workspace {}", run.id(), lease.token, lease.pid, now - lease.heartbeat_at, wrapper.as_ref().map_or(0, |w| w.pid), match wrapper.as_ref().map(|w| w.exited_at) {
                    Some(Some(at)) => format!("exited at {at}"),
                    Some(None) if alive == Some(true) => "alive".to_owned(),
                    Some(None) => "gone".to_owned(),
                    None => "none".to_owned(),
                }, run.task_id(), run.workspace_id().unwrap_or("?"));
            let phase = if run.status() == RunStatus::AwaitingIntegration {
                self.adopt_review(&run)
            } else {
                self.resume(&run)
            };
            match phase {
                Ok(phase) => {
                    let mut slot = Slot::new(run, phase);
                    self.restore_waiting(&mut slot, as_waiting)?;
                    self.slots.push(slot);
                }
                Err(error) => {
                    // The lease is this process's now; give it up like any
                    // other runtime error so `recover` can judge the run.
                    let message = format!("run {} could not be resumed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }
    /// Whether a run whose lease went stale is taken over by
    /// [`Self::adopt_stale_runs`] rather than recovered: an
    /// `awaiting_integration` run always; a run moved on by `resume_skipped`
    /// (it has no session of its own since: its supervisor alone owned it,
    /// whatever the wrapper of an earlier session left behind); otherwise
    /// only one whose wrapper is alive or has reported its exit. Never a
    /// run outside `running` / `validating` / `awaiting_integration`
    /// (`claimed` / `starting` need the claimer's token).
    pub(super) fn adoptable(
        &self,
        run: &TaskRun,
        wrapper: Option<&RunProcess>,
        now: i64,
    ) -> Result<bool> {
        if !matches!(
            run.status(),
            RunStatus::Running | RunStatus::Validating | RunStatus::AwaitingIntegration
        ) {
            return Ok(false);
        }
        if run.status() == RunStatus::AwaitingIntegration || self.skipped_resume(run.id())? {
            return Ok(true);
        }
        Ok(wrapper.is_some() && self.wrapper_alive(wrapper, now) != Some(false))
    }
    /// Whether the wrapper that has not reported its exit is alive (its
    /// process lives and its heartbeat is within the lease TTL); `None`
    /// when there is no wrapper or it reported its exit.
    fn wrapper_alive(&self, wrapper: Option<&RunProcess>, now: i64) -> Option<bool> {
        wrapper.and_then(|wrapper| {
            wrapper.exited_at.is_none().then(|| {
                self.processes.alive(wrapper.pid)
                    && now - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
            })
        })
    }
    /// Rebuild the slot of an adopted run from what the queue and the run
    /// directory hold: the planned paths, whether the receipt is already on
    /// disk and whether `/exit` was already requested (never sent twice; its
    /// timeout restarts now, and an `exit_request_timed_out` already recorded
    /// is not recorded again). The wrapper is registered, so no registration
    /// timeout applies. A `validating` run restarts validation from the
    /// beginning: it is a function of the receipt and the worktree alone.
    pub(super) fn resume(&self, run: &TaskRun) -> Result<Phase> {
        Ok(match run.status() {
            RunStatus::Validating => {
                Phase::Validating(Some(self.validate(run.clone())), self.session_of(run)?)
            }
            _ => {
                let receipt_path =
                    PathBuf::from(run.receipt_path().context("missing receipt path")?);
                let receipt_seen = self.files.is_file(&receipt_path)
                    && self.queue.has_run_event(run.id(), "receipt_observed")?;
                let exit_requested = self
                    .queue
                    .has_run_event(run.id(), "exit_requested")?
                    .then(Instant::now);
                let exit_timed_out = self
                    .queue
                    .has_run_event(run.id(), "exit_request_timed_out")?;
                let first_commit_seen = self
                    .queue
                    .has_run_event(run.id(), "first_commit_observed")?;
                // A dialog recorded before adoption is not recorded again
                // while the same screen stays up.
                let prompt_hash = self
                    .queue
                    .run_events(run.id())?
                    .into_iter()
                    .rev()
                    .find(|e| {
                        matches!(
                            e.kind.as_str(),
                            "prompt_waiting" | "prompt_cleared" | "receipt_observed"
                        )
                    })
                    .filter(|e| e.kind == "prompt_waiting")
                    .and_then(|e| e.payload["screen_hash"].as_str().map(str::to_owned));
                Phase::Session(SessionWatch {
                    workspace: run
                        .workspace_id()
                        .map(str::to_owned)
                        .context("adopted run has no workspace")?,
                    run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
                    receipt_path,
                    idle_marker: run.idle_marker_path()?,
                    startup: Instant::now(),
                    receipt_seen,
                    receipt_seen_at: receipt_seen.then(Instant::now),
                    exit_requested,
                    exit_timed_out,
                    first_commit_seen,
                    agent_seen: None,
                    prompt_checked: None,
                    prompt_hash,
                    // A timeout recorded without its ask (by a binary that
                    // made none, or a supervisor that died between the two)
                    // still gets one; one asked before is not asked again.
                    exit_asked: !exit_timed_out || self.queue.has_stuck_exit_ask(run.id())?,
                    // Only a run whose wrapper heartbeats is adopted.
                    silent: false,
                    exit_for_silence: false,
                    answer_start: None,
                    stall: StallWatch::adopt(&*self.queue, run)?,
                    stale: adopted_stale_nudge(&*self.queue, run, SESSION_PHASE, None)?,
                    recovery: RecoveryWatch::adopt(&*self.queue, run)?,
                    input_at: None,
                })
            }
        })
    }
    /// Whether the run's last resume event is `resume_skipped`: it was moved
    /// on without a session, and no resume opened one since.
    pub(super) fn skipped_resume(&self, id: &RunId) -> Result<bool> {
        Ok(self
            .queue
            .run_events(id)?
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.kind.as_str(),
                    "resume_started" | "resume_finished" | "resume_skipped"
                )
            })
            .is_some_and(|e| e.kind == "resume_skipped"))
    }
    /// The session an accepted run keeps open (ADR-0027): the workspace of
    /// the resume that handed its live session to validation
    /// (`resume_finished` with status `validating`) unless a
    /// `workspace_closed` of that resume followed, else the worker's own
    /// workspace while it is not closed.
    pub(super) fn session_of(&self, run: &TaskRun) -> Result<Option<SessionRef>> {
        let events = self.queue.run_events(run.id())?;
        let resumed = events.iter().rev().find(|e| {
            matches!(
                e.kind.as_str(),
                "resume_started" | "resume_finished" | "resume_skipped"
            )
        });
        // A run moved on by `resume_skipped` has no session open.
        if let Some(event) = resumed {
            if event.kind == "resume_finished"
                && event.payload["status"] == RunStatus::Validating.as_str()
                && let (Some(workspace), Some(attempt)) = (
                    event.payload["workspace_id"].as_str(),
                    event.payload["attempt"].as_u64(),
                )
            {
                let closed = events.iter().any(|e| {
                    e.id > event.id
                        && e.kind == "workspace_closed"
                        && e.payload["workspace_id"] == workspace
                });
                return Ok((!closed).then(|| SessionRef {
                    workspace: workspace.to_owned(),
                    resume: Some(attempt as usize),
                }));
            }
            return Ok(None);
        }
        Ok(run
            .workspace_id()
            .map(str::to_owned)
            .filter(|_| run.workspace_closed_at().is_none())
            .map(|workspace| SessionRef {
                workspace,
                resume: None,
            }))
    }
    /// Rebuild an adopted `awaiting_integration` run under review from its
    /// events: a `revise_requested` with nothing after it waits for the live
    /// session again (it is recorded before it is typed, so it is never
    /// typed a second time), and a `revise_unsent` asks a person; a verdict already recorded (`review_finished` with
    /// nothing after it), or an approved run not reviewed since its
    /// validation, goes on to its `/exit` without a second review and
    /// without a second `/exit` if one was already requested; anything else
    /// is reviewed from the start, the review being a function of the
    /// receipt and the commit.
    pub(super) fn adopt_review(&mut self, run: &TaskRun) -> Result<Phase> {
        let session = self.session_of(run)?;
        let events = self.queue.run_events(run.id())?;
        let Some(anchor) = crate::domain::review_anchor(&events) else {
            return self.start_review(run, session);
        };
        if anchor.kind == "review_started"
            && let Some(phase) = self.adopt_failed_review_ask(run, &session, &events, anchor)?
        {
            return Ok(phase);
        }
        let then = match anchor.kind.as_str() {
            "revise_requested" => {
                if let Some(live) = session.clone()
                    && session_alive(self, run.id())?
                {
                    return Ok(Phase::Revise(ReviseWatch::new(
                        run,
                        live,
                        anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        Fix::Revise(
                            serde_json::from_value(anchor.payload["reasons"].clone())
                                .unwrap_or_default(),
                        ),
                        UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        // An adopted request is not checked for a start.
                        None,
                    )?));
                }
                None
            }
            // A conflict request with nothing after it waits for the live
            // session again, with the passed verdict before it.
            "conflict_precheck" if anchor.payload["requested"] == true => {
                let passed = passed_before(&events, anchor.id);
                if let Some(live) = session.clone()
                    && let Some(verdict) = passed
                    && session_alive(self, run.id())?
                {
                    return Ok(Phase::Revise(ReviseWatch::new(
                        run,
                        live,
                        anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        Fix::Conflict(verdict),
                        UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        // An adopted request is not checked for a start.
                        None,
                    )?));
                }
                None
            }
            // A revise request recorded but not sent asks a person, as it
            // did before the supervisor was replaced.
            "revise_unsent" => passed_before(&events, anchor.id).map(|verdict| AfterExit::Ask {
                why: anchor.payload["error"].as_str().map(str::to_owned),
                decision: verdict.verdict,
                reasons: verdict.reasons,
                summary: verdict.summary,
            }),
            "review_finished" => {
                match serde_json::from_value::<ReviewVerdict>(json!({
                    "verdict": anchor.payload["verdict"],
                    "reasons": anchor.payload["reasons"],
                    "summary": anchor.payload["summary"],
                })) {
                    // A pass not yet followed by its /exit is prechecked
                    // (again): main may have moved.
                    Ok(verdict)
                        if verdict.verdict == ReviewDecision::Pass
                            && !events
                                .iter()
                                .any(|e| e.id > anchor.id && e.kind == "exit_requested") =>
                    {
                        return self.precheck(run, session, verdict);
                    }
                    Ok(verdict) if verdict.verdict == ReviewDecision::Pass => Some(AfterExit::Land),
                    Ok(verdict) => Some(AfterExit::Ask {
                        why: (verdict.verdict == ReviewDecision::Revise).then(|| {
                            "the revise could not go on when the supervisor was replaced".to_owned()
                        }),
                        decision: verdict.verdict,
                        reasons: verdict.reasons,
                        summary: verdict.summary,
                    }),
                    Err(_) => None,
                }
            }
            // A precheck that sent nothing decided to land (no session to
            // ask) or to ask a person (past the limit); before its /exit it
            // is prechecked again, as main may have moved.
            "conflict_precheck" => match passed_before(&events, anchor.id) {
                Some(verdict)
                    if !events
                        .iter()
                        .any(|e| e.id > anchor.id && e.kind == "exit_requested") =>
                {
                    return self.precheck(run, session, verdict);
                }
                Some(verdict) => Some(match anchor.payload["asked"].as_str() {
                    Some(why) => Fix::Conflict(verdict).ask(String::new(), why.to_owned()),
                    None => AfterExit::Land,
                }),
                None => None,
            },
            "validation_finished" if events.iter().any(|e| e.kind == "integration_approved") => {
                Some(AfterExit::Land)
            }
            // A rebased run a landing recheck resumed waits for the answer
            // to its approve_landing ask, not a review (ADR-0068 decision 4).
            "validation_finished"
                if crate::domain::resume::parked_by_recheck(&events)
                    && self
                        .queue
                        .has_unclosed_ask(run.id(), AskKind::ApproveLanding)? =>
            {
                Some(AfterExit::Rest { close: true })
            }
            _ => None,
        };
        let Some(mut then) = then else {
            return self.start_review(run, session);
        };
        // The ask was opened before the supervisor died, after this anchor:
        // the run waits for it (open or answered) rather than asking again,
        // which would close it as stale and notify the inbox twice (task 425).
        if matches!(then, AfterExit::Ask { .. })
            && let Some(ask) = self.unclosed_landing_ask_after(&events, anchor)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {} waits for a person in ask {}, opened before the supervisor stopped; it is not asked again", run.id(), ask.id);
            then = AfterExit::Rest { close: true };
        }
        let after = |kind: &str| events.iter().any(|e| e.id > anchor.id && e.kind == kind);
        let mut watch = ExitWatch::new(session, then);
        // Never a second /exit; its timeout restarts now.
        if after("exit_requested") {
            watch.requested = Some(Instant::now());
        }
        watch.timed_out = after("exit_request_timed_out");
        // A timeout recorded without its ask still gets one; one asked
        // before is not asked again (as for a running run, task 104).
        watch.exit_asked = !watch.timed_out || self.queue.has_stuck_exit_ask(run.id())?;
        Ok(Phase::Exiting(watch))
    }
    /// The failed review whose `approve_landing` ask was opened before the
    /// supervisor died (task 424): an ask the supervisor opened after the
    /// run's last `review_started`, not closed since, is that review's
    /// failure, since a review that gave a verdict records
    /// `review_finished` first. The run waits for the ask rather than being
    /// reviewed again: its `review_failed` (with the ask) is recorded if it
    /// was not, and the lease is given back once the session is gone, so
    /// an answer given or to come is applied as any other (ADR-0027).
    /// `None` when there is no such ask.
    fn adopt_failed_review_ask(
        &mut self,
        run: &TaskRun,
        session: &Option<SessionRef>,
        events: &[crate::domain::RunEvent],
        started: &crate::domain::RunEvent,
    ) -> Result<Option<Phase>> {
        let Some(ask) = self.unclosed_landing_ask_after(events, started)? else {
            return Ok(None);
        };
        let after = |kind: &str| events.iter().any(|e| e.id > started.id && e.kind == kind);
        if !after("review_failed") {
            let attempt = started.payload["attempt"].as_u64().unwrap_or(1);
            // The failure is in the ask's first line (`open_failed_review_ask`),
            // and a `send_back` names it to the resumed session.
            let error = ask
                .question
                .lines()
                .next()
                .and_then(|line| line.split_once("): "))
                .map_or_else(
                    || format!("review {attempt} failed and ask {} was opened for it before the supervisor stopped", ask.id),
                    |(_, error)| error.to_owned(),
                );
            self.queue.record_runtime_event(
                run.id(),
                "review_failed",
                json!({
                    "code": ReasonCode::JobFailed,
                    "attempt": attempt,
                    "error": error,
                    "status": run.status().as_str(),
                    "ask_id": ask.id,
                    "adopted": true,
                }),
            )?;
        }
        info!(run_id = %run.id(), ask_id = %ask.id, "run {} waits for a person in ask {} about its failed review, opened before the supervisor stopped; it is not reviewed again", run.id(), ask.id);
        let mut watch = ExitWatch::new(session.clone(), AfterExit::Rest { close: true });
        // The failed review's /exit was requested before the ask: never a
        // second one.
        if after("exit_requested") {
            watch.requested = Some(Instant::now());
        }
        watch.timed_out = after("exit_request_timed_out");
        watch.exit_asked = !watch.timed_out || self.queue.has_stuck_exit_ask(run.id())?;
        Ok(Some(Phase::Exiting(watch)))
    }
    /// The last `approve_landing` ask the supervisor opened after event
    /// `anchor`, if nobody closed it since: the ask of the step the anchor
    /// led to, opened before the supervisor died.
    fn unclosed_landing_ask_after(
        &self,
        events: &[crate::domain::RunEvent],
        anchor: &crate::domain::RunEvent,
    ) -> Result<Option<crate::domain::Ask>> {
        let opened = events.iter().rev().find(|e| {
            e.id > anchor.id
                && e.kind == "ask_opened"
                && e.payload["kind"] == AskKind::ApproveLanding.as_str()
                && e.payload["asked_by"] == "supervisor"
        });
        let Some(ask_id) = opened.and_then(|e| e.payload["ask_id"].as_i64()) else {
            return Ok(None);
        };
        let ask = self.queue.read_ask(AskId::new(ask_id))?;
        Ok(ask.closed_at.is_none().then_some(ask))
    }
    /// Whether a lease no longer has a working process behind it: its pid
    /// is dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
    pub(super) fn lease_stale(&self, lease: &RunLease, now: i64) -> bool {
        heartbeat_stale(self.processes.alive(lease.pid), now - lease.heartbeat_at)
    }
}

/// The verdict of the last `review_finished` before event `before`: the
/// pass a conflict precheck followed, or the revise a `revise_unsent` did.
pub(super) fn passed_before(
    events: &[crate::domain::RunEvent],
    before: EventId,
) -> Option<ReviewVerdict> {
    events
        .iter()
        .rev()
        .find(|e| e.id < before && e.kind == "review_finished")
        .and_then(|e| {
            serde_json::from_value(json!({
                "verdict": e.payload["verdict"],
                "reasons": e.payload["reasons"],
                "summary": e.payload["summary"],
            }))
            .ok()
        })
}
