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
            if self.slots.len() >= parallel {
                break;
            }
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
                Ok(phase) => self.slots.push(Slot { run, phase }),
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
    /// session again; a verdict already recorded (`review_finished` with
    /// nothing after it), or an approved run not reviewed since its
    /// validation, goes on to its `/exit` without a second review and
    /// without a second `/exit` if one was already requested; anything else
    /// is reviewed from the start, the review being a function of the
    /// receipt and the commit.
    pub(super) fn adopt_review(&mut self, run: &TaskRun) -> Result<Phase> {
        let session = self.session_of(run)?;
        let events = self.queue.run_events(run.id())?;
        let Some(anchor) = events.iter().rev().find(|e| {
            matches!(
                e.kind.as_str(),
                "validation_finished"
                    | "review_started"
                    | "review_finished"
                    | "revise_requested"
                    | "revise_finished"
                    | "conflict_precheck"
                    | "conflict_resolved"
            )
        }) else {
            return self.start_review(run, session);
        };
        let then = match anchor.kind.as_str() {
            "revise_requested" => {
                if let Some(live) = session.clone()
                    && session_alive(self, run.id())?
                {
                    return Ok(Phase::Revise(ReviseWatch {
                        session: live,
                        attempt: anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        fix: Fix::Revise(
                            serde_json::from_value(anchor.payload["reasons"].clone())
                                .unwrap_or_default(),
                        ),
                        sent_at: UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        sent: Instant::now(),
                        // An adopted request is not checked for a start.
                        start: None,
                    }));
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
                    return Ok(Phase::Revise(ReviseWatch {
                        session: live,
                        attempt: anchor.payload["attempt"].as_u64().unwrap_or(1) as usize,
                        fix: Fix::Conflict(verdict),
                        sent_at: UNIX_EPOCH
                            + Duration::from_secs(
                                anchor.payload["sent_at"].as_u64().unwrap_or_default(),
                            ),
                        sent: Instant::now(),
                        // An adopted request is not checked for a start.
                        start: None,
                    }));
                }
                None
            }
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
            _ => None,
        };
        let Some(then) = then else {
            return self.start_review(run, session);
        };
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
    /// Whether a lease no longer has a working process behind it: its pid
    /// is dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
    pub(super) fn lease_stale(&self, lease: &RunLease, now: i64) -> bool {
        heartbeat_stale(self.processes.alive(lease.pid), now - lease.heartbeat_at)
    }
}

/// The verdict of the last `review_finished` before event `before`: the
/// pass a conflict precheck followed.
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
