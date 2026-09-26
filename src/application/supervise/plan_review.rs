//! Plan review (ADR-0041 decisions 11-15, 17): the supervisor takes the
//! submitted proposals one at a time (interrupt first, then the oldest
//! submission) through a headless job, applies its verdict, applies a
//! person's answer to its `approve_plan` ask, delivers a revise to the
//! proposal's planner (or opens a planner of the runtime's for it, within
//! [`LoopSettings::runtime_planners`]), tells the inbox of a planner that
//! does not answer, and ends the runtime's planners that are done. None of
//! this takes a run slot; a failure is logged and tried again on a later
//! pass.

use super::*;
use crate::{
    application::{
        PlanReviewApply, PlanReviewJob, StatusFilter, TaskQuery,
        planner::{PlannerLaunch, PlannerProbes, PlannerView, open_runtime_planner, planner_view},
        prompt::{
            PLAN_REVIEW_TOOLS, PlanReviewMaterial, plan_review_prompt, plan_revise_request,
            precedent_line,
        },
    },
    domain::{
        MAX_PLAN_REVISES, PLAN_OPTIONS, PLAN_REVIEW_ASKER, PlanReviewDecision, PlanReviewVerdict,
        PlannerOrigin, PlannerState, Proposal, next_to_review,
        stats::{LiveSnapshot, SlotSnapshot, StatsQuery, conflicts::ConflictHotspot},
    },
};

/// Asks a person answered that the plan review prompt offers as
/// precedents, newest first.
const PRECEDENT_ASKS: usize = 30;

/// Files that conflict often the plan review prompt lists at most.
const HOTSPOT_FILES: usize = 15;

/// Ready and in-progress tasks the plan review prompt lists at most.
const QUEUED_TASKS: usize = 200;

/// The plan review job running now: one at a time, queue-wide.
pub(super) struct PlanReviewWatch {
    pub(super) job: PlanReviewJob,
    pub(super) headless: HeadlessJob,
    /// How many times the proposal was sent back when the job started.
    pub(super) revise_count: u32,
}

impl Supervisor<'_> {
    /// One pass of plan review: apply the answers, reap the job and apply
    /// its verdict, start the next one (unless the loop is draining), then
    /// deliver the revises and tend the planners.
    pub(super) fn plan_review_pass(&mut self, options: &LoopSettings, starting: bool) -> bool {
        let mut progressed = false;
        match self.queue.settle_proposals() {
            Ok(settled) => {
                for (proposal, status) in &settled {
                    info!(
                        "proposal {proposal} is {}: none of its tasks waits for plan review any more",
                        status.as_str()
                    );
                }
                progressed |= !settled.is_empty();
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "plan review: could not settle the proposals: {error:#}")
            }
        }
        for (what, result) in [
            ("apply the plan answers", self.apply_plan_answers()),
            ("reap the plan review", self.poll_plan_review()),
        ] {
            match result {
                Ok(done) => progressed |= done,
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "plan review: could not {what}: {error:#}")
                }
            }
        }
        if !starting {
            return progressed;
        }
        for (what, result) in [
            ("start a plan review", self.start_plan_review()),
            ("deliver the revises", self.tend_planners(options)),
        ] {
            if let Err(error) = result {
                warn!(error = %format_args!("{error:#}"), "plan review: could not {what}: {error:#}");
            }
        }
        progressed
    }

    /// Start the plan review of the next candidate, when none runs.
    fn start_plan_review(&mut self) -> Result<()> {
        if self.plan_review.is_some() {
            return Ok(());
        }
        let Some(proposal_id) = next_to_review(&self.queue.plan_review_candidates()?) else {
            return Ok(());
        };
        let Some(job) = self.queue.begin_plan_review(
            proposal_id,
            &self.token,
            &self.layout.plan_reviews_dir,
        )?
        else {
            return Ok(());
        };
        let proposal = self.queue.show_proposal(proposal_id)?;
        match self.spawn_plan_review(&job, &proposal) {
            Ok(headless) => {
                info!(task_id = %job.anchor, "proposal {proposal_id} plan review {} started", job.attempt);
                self.plan_review = Some(PlanReviewWatch {
                    job,
                    headless,
                    revise_count: proposal.revise_count(),
                });
            }
            Err(error) => {
                let error = format!("the headless plan review could not start: {error:#}");
                self.fail_plan_review(&job, &error, 0);
            }
        }
        Ok(())
    }

    /// Write the prompt into the job's directory and start the headless
    /// job in the repository's checkout, allowed to read only.
    fn spawn_plan_review(
        &mut self,
        job: &PlanReviewJob,
        proposal: &Proposal,
    ) -> Result<HeadlessJob> {
        self.files
            .create_dir_all(&job.dir)
            .with_context(|| format!("create {}", job.dir.display()))?;
        let prompt = self.plan_review_material(proposal)?;
        self.files
            .write(&job.dir.join("prompt.txt"), prompt.as_bytes())?;
        let stdout = job.dir.join("review.out");
        let stderr = job.dir.join("review.err");
        let mut command =
            self.reviewer
                .headless_command(&self.layout.repo_root, &prompt, PLAN_REVIEW_TOOLS)?;
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
            .context("start the plan review")?;
        Ok(HeadlessJob {
            what: "plan review",
            child,
            started: Instant::now(),
            timeout: self.reviewer.review_timeout(),
            stdout,
            stderr,
        })
    }

    /// The plan review prompt of `proposal` from the queue as it is now.
    fn plan_review_material(&mut self, proposal: &Proposal) -> Result<String> {
        let tasks = proposal
            .task_ids()
            .iter()
            .map(|&id| self.queue.show(id))
            .collect::<Result<Vec<_>>>()?;
        let mut goal_ids: Vec<_> = tasks
            .iter()
            .filter_map(|detail| detail.task.goal_id())
            .chain(proposal.goal_ids().iter().copied())
            .collect();
        goal_ids.sort();
        goal_ids.dedup();
        let goals = goal_ids
            .into_iter()
            .map(|id| Ok(self.queue.show_goal(id)?.goal))
            .collect::<Result<Vec<_>>>()?;
        let lint = crate::domain::lint::lint(&self.queue.lint_input(proposal.task_ids())?);
        let mut others = Vec::new();
        for other in self.queue.proposals(false)? {
            if other.id() == proposal.id() {
                continue;
            }
            let tasks = other
                .task_ids()
                .iter()
                .map(|&id| Ok(self.queue.show(id)?.task))
                .collect::<Result<Vec<_>>>()?;
            others.push((other, tasks));
        }
        let queued = self
            .queue
            .list(&TaskQuery {
                status: StatusFilter::Only(vec![TaskStatus::Ready, TaskStatus::InProgress]),
                limit: QUEUED_TASKS,
                full: true,
                ..TaskQuery::default()
            })?
            .tasks;
        let precedents = self.queue.answered_asks(PRECEDENT_ASKS)?;
        let hotspots = self.conflict_hotspots()?;
        plan_review_prompt(&PlanReviewMaterial {
            proposal,
            tasks: &tasks,
            goals: &goals,
            lint: &lint,
            others: &others,
            queued: &queued,
            precedents: &precedents,
            hotspots: &hotspots,
            repo_root: &self.layout.repo_root,
        })
    }

    /// The files the landings conflicted in, as `stats` counts them over
    /// its default window (goal 31), most conflicts first, at most
    /// [`HOTSPOT_FILES`] of those main still has.
    fn conflict_hotspots(&self) -> Result<Vec<ConflictHotspot>> {
        let events = self.queue.all_events()?;
        let live = LiveSnapshot {
            history: crate::application::stats::conflict_history(&events, &|since| {
                self.repository.main_history(since)
            }),
            conflicts: self.conflicts,
            ..LiveSnapshot::default()
        };
        let stats = crate::domain::stats::stats(
            &events,
            &self.queue.task_goals()?,
            self.generators.clock.now(),
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &live,
        );
        Ok(stats
            .conflict_hotspots
            .files
            .into_iter()
            .filter(|file| file.state != "deleted")
            .take(HOTSPOT_FILES)
            .collect())
    }

    /// Reap the job once it ended and apply its verdict, or record its
    /// failure.
    fn poll_plan_review(&mut self) -> Result<bool> {
        let Some(watch) = self.plan_review.as_mut() else {
            return Ok(false);
        };
        let Some(outcome) = watch.headless.poll(&*self.files)? else {
            return Ok(false);
        };
        let watch = self.plan_review.take().expect("polled above");
        let duration_secs = watch.headless.started.elapsed().as_secs();
        let applied = outcome
            .and_then(|stdout| PlanReviewVerdict::parse(&stdout))
            .map_err(|error| anyhow!(error))
            .and_then(|verdict| {
                self.apply_plan_verdict(&watch.job, watch.revise_count, verdict, duration_secs)
            });
        if let Err(error) = applied {
            self.fail_plan_review(&watch.job, &format!("{error:#}"), duration_secs);
        }
        Ok(true)
    }

    /// What the runtime makes of the verdict (a revise past
    /// [`MAX_PLAN_REVISES`] is a concern; the precedents it names are
    /// quoted), applied in one transaction; the inbox is told of a new
    /// `approve_plan` ask.
    fn apply_plan_verdict(
        &mut self,
        job: &PlanReviewJob,
        revise_count: u32,
        verdict: PlanReviewVerdict,
        duration_secs: u64,
    ) -> Result<()> {
        let proposal = job.proposal_id;
        let (decision, overridden) = match verdict.verdict {
            PlanReviewDecision::Revise if revise_count >= MAX_PLAN_REVISES => (
                PlanReviewDecision::Concern,
                Some(format!(
                    "proposal {proposal} was sent back {revise_count} times already (at most {MAX_PLAN_REVISES})"
                )),
            ),
            decision => (decision, None),
        };
        let answered = self.queue.answered_asks(usize::MAX >> 1)?;
        let precedents: Vec<String> = verdict
            .precedents
            .iter()
            .filter_map(|id| answered.iter().find(|ask| ask.id == *id))
            .map(precedent_line)
            .collect();
        let mut revise_reasons = verdict.reasons.clone();
        revise_reasons.extend(precedents.iter().cloned());
        let ask = (decision == PlanReviewDecision::Concern).then(|| NewAsk {
            kind: AskKind::ApprovePlan,
            task_id: Some(job.anchor),
            run_id: None,
            question: plan_question(job, &verdict, overridden.as_deref(), &precedents),
            options: PLAN_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: PLAN_REVIEW_ASKER.to_owned(),
            reason_category: AskReason::Scope,
            finding_id: None,
        });
        let applied = self.queue.finish_plan_review(
            job,
            &self.token,
            &PlanReviewApply {
                verdict: verdict.clone(),
                decision,
                overridden,
                revise_reasons,
                ask,
                duration_secs,
            },
        )?;
        if applied.stale {
            info!(task_id = %job.anchor, "proposal {proposal} moved on during its plan review; the verdict is not applied");
            return Ok(());
        }
        info!(task_id = %job.anchor, "proposal {proposal} plan review {}: {} ({})", job.attempt, decision.as_str(), verdict.summary);
        for reopened in &applied.reopened {
            info!(task_id = %reopened.task_id, "task {} left ready for proposal {} to fix it", reopened.task_id, reopened.proposal_id);
        }
        if let Some(outcome) = &applied.ask {
            // The verdict is applied: a notification that fails is only
            // reported.
            let error =
                match ask::notify(&mut *self.queue, &self.layout.repo_root, outcome, self.cmux) {
                    Ok(notified) => notified.get("notify_error").map(ToString::to_string),
                    Err(error) => Some(format!("{error:#}")),
                };
            if let Some(error) = error {
                warn!(ask_id = %outcome.ask.id, "proposal {proposal}: the inbox was not notified of ask {}: {error}", outcome.ask.id);
            }
        }
        Ok(())
    }

    /// Record the job's failure; the proposal waits for a person
    /// (`plan_review_failed`) and is not reviewed again by itself.
    fn fail_plan_review(&mut self, job: &PlanReviewJob, error: &str, duration_secs: u64) {
        warn!(task_id = %job.anchor, error = %error, "proposal {} plan review {} failed: {error}; it waits for a person", job.proposal_id, job.attempt);
        if let Err(recorded) = self
            .queue
            .fail_plan_review(job, &self.token, error, duration_secs)
        {
            warn!(error = %format_args!("{recorded:#}"), "proposal {}: the plan review failure could not be recorded: {recorded:#}", job.proposal_id);
        }
    }

    /// Apply the answered `approve_plan` asks (ADR-0041 decision 11).
    fn apply_plan_answers(&mut self) -> Result<bool> {
        let mut applied = false;
        for answered in self.queue.plan_answers()? {
            match self.queue.decide_plan(answered.id) {
                Ok(Some(decided)) => {
                    applied = true;
                    info!(ask_id = %answered.id, "proposal {} is {} as ask {} answered {}", decided.proposal.id(), decided.proposal.status().as_str(), answered.id, decided.answer)
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(ask_id = %answered.id, error = %format_args!("{error:#}"), "the answer of ask {} could not be applied: {error:#}", answered.id)
                }
            }
        }
        Ok(applied)
    }

    /// Take back a revise whose planner is gone, tell the inbox of a revise
    /// no planner answered within the timeout, deliver every revise not
    /// delivered yet, and end the runtime's planners that are done
    /// (ADR-0041 decisions 12, 13).
    fn tend_planners(&mut self, options: &LoopSettings) -> Result<()> {
        let now = self.generators.clock.now();
        let timeout = i64::try_from(options.planner_timeout.as_secs())?;
        let mut views = self.planner_views()?;
        for revise in self.queue.revising_proposals()? {
            let proposal = revise.proposal.id();
            match (revise.sent_at, revise.planner_id) {
                (Some(sent_at), Some(planner_id)) => {
                    let gone = views
                        .iter()
                        .find(|view| view.planner.id == planner_id)
                        .is_none_or(|view| self.planner_gone(view));
                    if gone {
                        info!(
                            "proposal {proposal}: planner {planner_id} is gone before it submitted again; the revise goes to another planner"
                        );
                        self.queue.revise_lost(
                            proposal,
                            Some(planner_id),
                            "its planner is gone before it submitted again",
                        )?;
                    } else if revise.unresponsive_at.is_none() && now - sent_at > timeout {
                        warn!(
                            "proposal {proposal}: planner {planner_id} did not submit it again within {timeout} seconds; the inbox is told"
                        );
                        self.queue.planner_unresponsive(
                            proposal,
                            Some(planner_id),
                            now - sent_at,
                        )?;
                    }
                }
                // A delivery claimed and never recorded (its supervisor
                // died in between) is delivered again.
                (Some(sent_at), None) if now - sent_at > HEARTBEAT_TIMEOUT_SECS => {
                    self.queue
                        .revise_lost(proposal, None, "its delivery was never recorded")?;
                }
                (None, _)
                    if revise.unresponsive_at.is_none()
                        && revise.revised_at.is_some_and(|at| now - at > timeout) =>
                {
                    let waited = now - revise.revised_at.unwrap_or(now);
                    warn!(
                        "proposal {proposal}: its revise waited {waited} seconds for a planner; the inbox is told"
                    );
                    self.queue.planner_unresponsive(proposal, None, waited)?;
                }
                _ => {}
            }
        }
        let mut runtime_open = views
            .iter()
            .filter(|view| view.planner.origin == PlannerOrigin::Runtime && view.alive)
            .count();
        for revise in self.queue.revising_proposals()? {
            if revise.sent_at.is_some() {
                continue;
            }
            let proposal = &revise.proposal;
            let owner = proposal
                .owner()
                .workspace_id
                .as_deref()
                .and_then(|workspace| {
                    views
                        .iter()
                        .find(|view| view.planner.workspace_id.as_deref() == Some(workspace))
                });
            match owner {
                Some(view) if view.alive => {
                    // A planner at work gets the revise once it is idle.
                    if view.state != PlannerState::Idle
                        || !self.queue.claim_revise(proposal.id())?
                    {
                        continue;
                    }
                    let workspace = view.planner.workspace_id.clone().unwrap_or_default();
                    let text = plan_revise_request(proposal.id(), &revise.reasons);
                    if let Err(error) =
                        submit_input(self.cmux, self.signals, &workspace, Input::Text(&text))
                    {
                        warn!(error = %format_args!("{error:#}"), "proposal {}: the revise could not be typed into workspace {workspace}: {error:#}", proposal.id());
                        self.queue
                            .revise_lost(proposal.id(), None, "the typing failed")?;
                        continue;
                    }
                    info!(
                        "proposal {}: the revise went to planner {} in workspace {workspace}",
                        proposal.id(),
                        view.planner.id
                    );
                    self.queue
                        .revise_sent(proposal.id(), view.planner.id, &workspace, false)?;
                }
                // Not listed, yet its session runs: it is not given up on
                // that evidence; the timeout tells the inbox.
                Some(view) if !self.planner_gone(view) => {}
                _ if runtime_open < options.runtime_planners => {
                    if !self.queue.claim_revise(proposal.id())? {
                        continue;
                    }
                    let opened = match self.open_planner_for(proposal, &revise.reasons) {
                        Ok(opened) => opened,
                        Err(error) => {
                            self.queue.revise_lost(
                                proposal.id(),
                                None,
                                "the planner could not be opened",
                            )?;
                            return Err(error);
                        }
                    };
                    runtime_open += 1;
                    let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
                    info!(
                        "proposal {}: opened planner {} in workspace {workspace} for its revise",
                        proposal.id(),
                        opened.planner.id
                    );
                    self.queue
                        .revise_sent(proposal.id(), opened.planner.id, &workspace, true)?;
                    views = self.planner_views()?;
                }
                // At the limit: the revise waits for a runtime planner to end.
                _ => {}
            }
        }
        // Then the drafts the runtime or a job registered (ADR-0041
        // decision 16), within the same limit.
        self.deliver_planner_answers(options, &views, &mut runtime_open)?;
        self.open_draft_planners(options, &mut runtime_open)?;
        self.end_runtime_planners(&views)
    }

    /// Whether a planner's session is over: it exited, its wrapper stopped
    /// heartbeating, or its workspace is not listed and its wrapper is dead
    /// (a workspace not listed alone is no evidence: its session is judged
    /// by its wrapper, as a worker's is).
    fn planner_gone(&self, view: &PlannerView) -> bool {
        match view.state {
            PlannerState::Exited | PlannerState::Lost => true,
            PlannerState::Closed => view
                .planner
                .wrapper_pid
                .is_none_or(|pid| view.planner.exited_at.is_some() || !self.processes.alive(pid)),
            _ => false,
        }
    }

    pub(super) fn planner_views(&self) -> Result<Vec<PlannerView>> {
        let probes = PlannerProbes {
            cmux: self.cmux,
            processes: &*self.processes,
            files: &*self.files,
            signals: self.signals,
            clock: &*self.generators.clock,
            planners_dir: &self.layout.planners_dir,
        };
        self.queue
            .planners(false)?
            .into_iter()
            .map(|planner| planner_view(&probes, planner))
            .collect()
    }

    /// Open a planner of the runtime's for `proposal` with its reasons.
    fn open_planner_for(
        &mut self,
        proposal: &Proposal,
        reasons: &[String],
    ) -> Result<crate::application::planner::OpenedPlanner> {
        let tasks = proposal
            .task_ids()
            .iter()
            .map(|&id| Ok(self.queue.show(id)?.task))
            .collect::<Result<Vec<_>>>()?;
        open_runtime_planner(&self.planner_launch(), proposal.id(), &tasks, reasons)
    }

    /// What opening a planner of the runtime's works with.
    pub(super) fn planner_launch(&self) -> PlannerLaunch<'_> {
        let layout = self.layout;
        PlannerLaunch {
            queue: &*self.queue,
            cmux: self.cmux,
            files: &*self.files,
            db: &layout.db,
            queue_hash: &layout.queue_hash,
            planners_dir: &layout.planners_dir,
            repo_root: &layout.repo_root,
            runner: &layout.runner,
            claude: &layout.claude,
            plugin_dir: layout.plugin_dir.as_deref(),
        }
    }

    /// End the runtime's planners that are done: one idle with no revise of
    /// its own left is asked to `/exit` once, and closed after the exit
    /// timeout if it does not; one whose session is over is given up and
    /// its workspace closed. A person's planner is never closed by the
    /// runtime.
    fn end_runtime_planners(&mut self, views: &[PlannerView]) -> Result<()> {
        let revising = self.queue.revising_proposals()?;
        for view in views
            .iter()
            .filter(|view| view.planner.origin == PlannerOrigin::Runtime)
        {
            let id = view.planner.id;
            let workspace = view.planner.workspace_id.clone();
            let asked = self
                .planner_exits
                .iter()
                .find(|(sent, _)| *sent == id)
                .map(|(_, at)| at.elapsed());
            let overdue = asked.is_some_and(|elapsed| elapsed > self.cmux.exit_timeout());
            if self.planner_gone(view) || overdue {
                if overdue {
                    warn!(
                        "planner {id} of the runtime did not exit within {} seconds of /exit; its workspace is closed",
                        self.cmux.exit_timeout().as_secs()
                    );
                }
                if let Some(workspace) = &workspace
                    && self.cmux.exists(workspace)?
                {
                    self.cmux.close(workspace)?;
                }
                self.queue.close_planner(id, None)?;
                self.planner_exits.retain(|(sent, _)| *sent != id);
                info!(
                    "planner {id} of the runtime ended ({})",
                    view.state.as_str()
                );
                continue;
            }
            let busy = revising.iter().any(|revise| {
                revise.planner_id == Some(id)
                    || (revise.sent_at.is_none()
                        && revise.proposal.owner().workspace_id.is_some()
                        && revise.proposal.owner().workspace_id == workspace)
            }) || self.waits_on_question(view)?;
            if view.state == PlannerState::Idle
                && !busy
                && asked.is_none()
                && let Some(workspace) = &workspace
            {
                submit_input(self.cmux, self.signals, workspace, Input::Exit)?;
                self.planner_exits.push((id, Instant::now()));
                info!("planner {id} of the runtime is done; asked it to exit");
            }
        }
        Ok(())
    }
}

impl Supervisor<'_> {
    /// Whether a planner of the runtime's still waits on a
    /// `planner_question` about its draft: one nobody closed (unanswered,
    /// or its answer not typed yet), or one whose answer the supervisor
    /// typed into this planner's workspace after its agent last stopped (it
    /// is at work on the answer). An ask closed without a typing holds
    /// nothing.
    fn waits_on_question(&mut self, view: &PlannerView) -> Result<bool> {
        let Some(draft) = view.planner.draft_task_id else {
            return Ok(false);
        };
        let asks: Vec<_> = self
            .queue
            .asks(crate::application::AskQuery {
                all: true,
                ..Default::default()
            })?
            .into_iter()
            .filter(|ask| ask.kind == AskKind::PlannerQuestion && ask.task_id == Some(draft))
            .collect();
        if asks.iter().any(|ask| ask.closed_at.is_none()) {
            return Ok(true);
        }
        let Some(workspace) = view.planner.workspace_id.as_deref() else {
            return Ok(false);
        };
        let events = self.queue.show(draft)?.events;
        Ok(asks.iter().any(|ask| {
            let typed_here = events.iter().any(|event| {
                event.kind == "ask_delivered"
                    && event.payload.get("ask_id").and_then(Value::as_i64) == Some(ask.id.as_i64())
                    && event.payload.get("workspace_id").and_then(Value::as_str) == Some(workspace)
            });
            // The typing happened when the ask closed; an agent idle since
            // before it has not taken the answer up yet.
            typed_here
                && ask
                    .closed_at
                    .is_some_and(|closed| view.idle_since.is_none_or(|since| since <= closed))
        }))
    }
}

/// The question of the `approve_plan` ask a concern opens.
fn plan_question(
    job: &PlanReviewJob,
    verdict: &PlanReviewVerdict,
    overridden: Option<&str>,
    precedents: &[String],
) -> String {
    let mut question = format!(
        "Plan review of proposal {proposal} (its first task is {anchor}) needs a person: {summary}",
        proposal = job.proposal_id,
        anchor = job.anchor,
        summary = verdict.summary,
    );
    if let Some(why) = overridden {
        question.push_str(&format!(
            "\nIt answered {}, but {why}.",
            verdict.verdict.as_str()
        ));
    }
    if !verdict.reasons.is_empty() {
        question.push_str("\nReasons:");
        for reason in &verdict.reasons {
            question.push_str(&format!("\n- {reason}"));
        }
    }
    for precedent in precedents {
        question.push_str(&format!("\n- {precedent}"));
    }
    question.push_str(&format!(
        "\nPlan review material: {}/prompt.txt\nready: make the proposal's tasks ready as they are. send_back: send it back to its planner (answer `send_back: <your reason>` to add yours). cancel: cancel the proposal's tasks.",
        job.dir.display()
    ));
    question
}
